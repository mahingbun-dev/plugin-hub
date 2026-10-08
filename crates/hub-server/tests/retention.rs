//! 留存巡检：该丢的丢、该进死信的进死信、该清的清。
//!
//! 这一段出错的方式很安静——不是报错，而是**数据悄悄消失**或**存储悄悄涨满**。
//! 两种都要靠真实 PostgreSQL + 真实 Redis 才能看出来（「消息有没有真的从流上摘掉」
//! 只有问 Redis 才知道）。

use std::time::Duration;

use hub_bus::{Bus, BusConfig};
use hub_proto::BusMessage;
use hub_proto::v1::bus_message::Kind;
use hub_server::tasks::{RetentionPolicy, sweep};
use hub_store::Store;
use hub_store::dead_letters;
use hub_store::flows::{self, DraftInput};
use hub_store::runs::{self, NewRun};
use serde_json::json;
use sqlx::PgPool;
use ulid::Ulid;

/// 测试用的 Redis。
///
/// 默认硬编码到本地 db 9，**刻意不读 REDIS_URL**：.env 里的那个指向应用的
/// db 2，测试键混进去会污染正在跑的中台。
/// TEST_REDIS_URL 只用来在 CI 里指向另一个端口——CI 自己起一个 Redis，
/// 不假定 runner 上正好有一个（跟测试 PG 用非默认端口是同一个理由）。
fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/9".to_string())
}

/// 一套「数据侧什么都要清」的策略：消息 / 执行 / span 的保留期都是 0。
///
/// **死信的保留期特意给足一小时**，理由是留 0 会自相矛盾：一轮巡检刚把超期消息写进
/// 死信，紧接着的「清理死信」就按「躺太久了」把它删掉——巡检吃掉自己的产出。生产上
/// 保留期是天，撞不上；但留着 0 会让用例的结果取决于两次 `now()` 谁在前谁在后。
fn aggressive() -> RetentionPolicy {
    RetentionPolicy {
        dead_letters: Duration::from_secs(3600),
        ..purge_everything()
    }
}

/// 连死信也一起清。只有专门测「已重放的死信会被清掉」时才用它。
fn purge_everything() -> RetentionPolicy {
    RetentionPolicy {
        stream: Duration::ZERO,
        runs: Duration::ZERO,
        spans: Duration::ZERO,
        dead_letters: Duration::ZERO,
        rejections: Duration::ZERO,
    }
}

async fn bus(tag: &str) -> Bus {
    Bus::connect(
        &redis_url(),
        BusConfig {
            stream: format!("test:retention:{tag}:{}", Ulid::generate()),
            group: "test-workers".to_string(),
            consumer: "test-consumer".to_string(),
            block: Duration::from_millis(50),
            ..BusConfig::default()
        },
    )
    .await
    .expect("连接 Redis 失败——本机 6379 上应有 Redis")
}

fn node_message(run_id: &str) -> BusMessage {
    BusMessage {
        kind: Kind::Node as i32,
        run_id: run_id.to_string(),
        flow_name: "intake".to_string(),
        flow_revision: 1,
        node_id: "a".to_string(),
        enqueued_at_ms: 1,
        attempts: 1,
        envelope: Some(hub_proto::Envelope {
            message_id: "msg-1".to_string(),
            payload: Some(
                hub_proto::encode_payload(&json!({"orderId": "SO-1"})).expect("编码应成功"),
            ),
            ..Default::default()
        }),
    }
}

/// 建一条 flow 并落一条指定状态的 run。
async fn run_with_status(store: &Store, run_id: &str, status: &str) {
    run_started_days_ago(store, run_id, status, 365).await;
}

/// 同上，但可以指定开始时间——留存相关的断言全靠这个时间差。
async fn run_started_days_ago(store: &Store, run_id: &str, status: &str, days: i64) {
    flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name: "intake",
            description: "留存测试",
            definition: &json!({"name": "intake", "nodes": [], "edges": []}),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("建 flow 应成功");

    let flow = flows::find_flow(store.pool(), "intake")
        .await
        .unwrap()
        .expect("应查得到");

    runs::upsert_run(
        store.pool(),
        &NewRun {
            run_id,
            flow_id: flow.id,
            flow_revision: 1,
            trace_id: "trace-1",
            subject: None,
            trigger: None,
            status,
            input_summary: None,
            error: None,
            started_at: chrono::Utc::now() - chrono::Duration::days(days),
            finished_at: None,
        },
    )
    .await
    .expect("落 run 应成功");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 超期消息进死信并从流上摘掉(pool: PgPool) {
    let store = Store::from_pool(pool);
    let bus = bus("stale").await;

    // run 不存在（或还没定终态）→ 这条消息还有救，该进死信
    bus.publish(&node_message("run-未完结")).await.unwrap();
    assert_eq!(bus.depth().await.unwrap(), 1);

    sweep(&store, &bus, &aggressive()).await;

    let letters = dead_letters::list(store.pool(), true, 10).await.unwrap();
    assert_eq!(letters.len(), 1, "超期消息该进死信");
    assert_eq!(letters[0].run_id.as_deref(), Some("run-未完结"));
    assert_eq!(letters[0].flow_name.as_deref(), Some("intake"));
    assert!(
        letters[0].error.contains("躺太久"),
        "错误要说清为什么进死信：{:?}",
        letters[0].error
    );
    assert!(
        letters[0].payload_summary.is_some(),
        "留一份摘要，排障时至少知道这条是什么数据"
    );

    assert_eq!(
        bus.depth().await.unwrap(),
        0,
        "进死信之后要从流上摘掉——**留着它就等于每次巡检都重来一遍**，\
         而且它会一直占着 Redis 的内存"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 已完结执行的迟到消息直接丢掉(pool: PgPool) {
    let store = Store::from_pool(pool);
    let bus = bus("settled").await;

    run_with_status(&store, "run-done", "succeeded").await;
    bus.publish(&node_message("run-done")).await.unwrap();

    sweep(&store, &bus, &aggressive()).await;

    assert_eq!(
        dead_letters::list(store.pool(), true, 10)
            .await
            .unwrap()
            .len(),
        0,
        "这次执行早就跑完了，它的迟到消息没有可补救的东西——送进死信只会让死信列表\
         变成一堆需要人逐个确认的噪音"
    );
    assert_eq!(
        bus.depth().await.unwrap(),
        0,
        "但消息还是要摘掉，否则它会一直占着内存"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 解不开的消息也能被摘掉并记下来(pool: PgPool) {
    let store = Store::from_pool(pool);
    let bus = bus("broken").await;

    // 直接往流里塞一段不是 protobuf 的字节
    let client = redis::Client::open(redis_url()).expect("地址合法");
    let mut conn = client.get_connection_manager().await.expect("应连上");
    let _: String = redis::cmd("XADD")
        .arg(bus.config().stream.as_str())
        .arg("*")
        .arg("d")
        .arg(vec![0xffu8, 0xff, 0xff, 0xff])
        .query_async(&mut conn)
        .await
        .expect("塞入应成功");

    sweep(&store, &bus, &aggressive()).await;

    let letters = dead_letters::list(store.pool(), true, 10).await.unwrap();
    assert_eq!(letters.len(), 1, "解不开的消息同样占内存、同样躺太久了");
    assert!(
        letters[0].error.contains("解不开"),
        "要说清是消息体本身有问题，而不是含糊地报「处理失败」：{:?}",
        letters[0].error
    );
    assert_eq!(
        bus.depth().await.unwrap(),
        0,
        "解不开不等于摘不掉——留在流里会让每次巡检都报同一个错"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 终态执行记录会被清掉而未完结的留着(pool: PgPool) {
    let store = Store::from_pool(pool);

    run_with_status(&store, "run-finished", "failed").await;
    run_with_status(&store, "run-stuck", "queued").await;

    sweep(&store, &bus("purge").await, &aggressive()).await;

    assert!(
        runs::find_run(store.pool(), "run-finished")
            .await
            .unwrap()
            .is_none(),
        "过了保留期的终态记录该清掉"
    );
    assert!(
        runs::find_run(store.pool(), "run-stuck")
            .await
            .unwrap()
            .is_some(),
        "停在 queued 的执行是「有东西卡住了」的信号——按时间无差别清掉它，\
         等于把这个问题从记录里抹掉，而它本来正是要被看见的"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 一项出错不影响其余各项(pool: PgPool) {
    let store = Store::from_pool(pool);

    // 造出三类该被清的数据：过期的幂等键、过期的载荷、已重放的死信
    hub_store::idempotency::claim(
        store.pool(),
        "old-key",
        None,
        chrono::Utc::now() - chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    hub_store::payloads::put(store.pool(), b"blob", Duration::ZERO)
        .await
        .unwrap();
    let id = dead_letters::insert(
        store.pool(),
        &dead_letters::NewDeadLetter {
            stream: "hub:flows",
            stream_id: "x-1",
            run_id: None,
            flow_name: None,
            node_id: None,
            attempts: 1,
            error: "e",
            payload_summary: None,
        },
    )
    .await
    .unwrap();
    dead_letters::mark_replayed(store.pool(), id, "run-new", chrono::Utc::now())
        .await
        .unwrap();

    // 这里要连死信一起清，所以用最彻底的那一套
    sweep(&store, &bus("mixed").await, &purge_everything()).await;

    assert!(
        !hub_store::idempotency::seen(store.pool(), "old-key")
            .await
            .unwrap(),
        "过期的幂等键该清掉"
    );
    assert!(
        hub_store::payloads::get(store.pool(), "never")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        dead_letters::list(store.pool(), false, 10)
            .await
            .unwrap()
            .len(),
        0,
        "已重放且过了保留期的死信该清掉"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 保留期内什么都不动(pool: PgPool) {
    let store = Store::from_pool(pool);
    let bus = bus("keep").await;

    bus.publish(&node_message("run-1")).await.unwrap();
    // 刚刚发生的执行：90 天的保留期远没到
    run_started_days_ago(&store, "run-1", "succeeded", 0).await;

    // 保留期都很长：这一轮不该动任何东西
    sweep(
        &store,
        &bus,
        &RetentionPolicy {
            stream: Duration::from_secs(86_400),
            runs: Duration::from_secs(86_400 * 90),
            spans: Duration::from_secs(86_400 * 7),
            dead_letters: Duration::from_secs(86_400 * 90),
            rejections: Duration::from_secs(86_400 * 30),
        },
    )
    .await;

    assert_eq!(bus.depth().await.unwrap(), 1, "还没到期的消息不该被动");
    assert!(
        runs::find_run(store.pool(), "run-1")
            .await
            .unwrap()
            .is_some(),
        "还没到期的执行记录不该被清"
    );
}
