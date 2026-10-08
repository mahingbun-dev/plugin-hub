//! 触发器调度的端到端：cron 真的会到点触发，MQ 真的会从外部流里取数。
//!
//! 这两条都不适合拆开测内部函数——调度器的价值全在「一直跑着」这件事上，而「下一次
//! 触发时刻算得对不对」这种断言，测的是 cron 库而不是我们。所以这里起真的循环、
//! 等真的到点，看消息有没有真的落到总线上。
//!
//! 用它自己的时间尺度：cron 用秒级表达式（`cron` 支持 6 段），等待都带上限。

use std::sync::Arc;
use std::time::Duration;

use hub_bus::{Bus, BusConfig};
use hub_engine::{AsyncExecutor, FlowExecutor, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_server::scheduler;
use hub_store::Store;
use hub_store::flows::{self, DraftInput};
use hub_store::model::TriggerRow;
use hub_store::triggers;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::watch;
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

struct Harness {
    store: Store,
    exec: AsyncExecutor,
    shutdown: watch::Sender<bool>,
}

async fn harness(pool: PgPool, tag: &str) -> Harness {
    let store = Store::from_pool(pool);
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let executor = FlowExecutor::new(registry, invoker);

    let stream = format!("test:sched:{tag}:{}", Ulid::generate());
    let bus = Bus::connect(
        &redis_url(),
        BusConfig {
            stream: stream.clone(),
            group: "test-workers".to_string(),
            consumer: "test-consumer".to_string(),
            block: Duration::from_millis(50),
            ..BusConfig::default()
        },
    )
    .await
    .expect("连接 Redis 失败——本机 6379 上应有 Redis");

    let exec = AsyncExecutor::new(store.clone(), bus, executor);
    let (shutdown, _) = watch::channel(false);

    Harness {
        store,
        exec,
        shutdown,
    }
}

/// 建一条单节点的已发布 flow。
async fn publish_flow(store: &Store, name: &str) {
    flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name,
            description: "调度测试",
            definition: &json!({
                "name": name,
                "nodes": [{"id": "a", "plugin": "flow-first"}],
                "edges": []
            }),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("存草稿应成功");
    flows::publish_draft(store.pool(), name)
        .await
        .expect("发布应成功");
}

/// 等到总线上出现消息，或到点放弃。
async fn wait_for_message(exec: &AsyncExecutor, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if exec.bus().depth().await.unwrap_or(0) > 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// 等到某条 flow 的触发器记下至少一次触发，返回那一行；到点没等到返回 `None`。
///
/// 调度器先投总线、后记 `fired_count`，一看到消息就查库会抢在写库前面——CI 机器一忙，
/// 这个窗口足以让断言读到 0。带预算轮询，等计数落库再断言。
async fn wait_for_fired(store: &Store, flow_name: &str, budget: Duration) -> Option<TriggerRow> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let rows = triggers::list_of_flow(store.pool(), flow_name)
            .await
            .unwrap();
        if let Some(row) = rows.into_iter().find(|r| r.fired_count >= 1) {
            return Some(row);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn cron_到点会真的触发(pool: PgPool) {
    let h = harness(pool, "cron").await;
    publish_flow(&h.store, "nightly").await;

    // 秒级表达式：`cron` 支持 6 段，测试用它把等待压到秒级
    triggers::upsert(
        h.store.pool(),
        &triggers::NewTrigger {
            flow_name: "nightly",
            kind: triggers::KIND_CRON,
            name: "every-second",
            config: &json!({"expr": "* * * * * *"}),
        },
    )
    .await
    .expect("登记应成功");

    scheduler::spawn_cron(h.store.clone(), h.exec.clone(), h.shutdown.subscribe());

    assert!(
        wait_for_message(&h.exec, Duration::from_secs(8)).await,
        "cron 到点该把消息投上总线"
    );

    let row = wait_for_fired(&h.store, "nightly", Duration::from_secs(8))
        .await
        .expect("要记下触发次数");
    assert!(row.last_fired_at.is_some(), "要记下最后触发时刻");
    assert!(
        row.last_error.is_none(),
        "成功时不该留错误：{:?}",
        row.last_error
    );

    // 载荷里带上这次是哪个计划点触发的——排障时最常问「它按时跑了没有」
    let deliveries = h.exec.bus().receive().await.expect("应取到消息");
    assert_eq!(deliveries[0].message.flow_name, "nightly");
    let envelope = deliveries[0].message.envelope.as_ref().expect("应有信封");
    let payload =
        hub_proto::decode_payload(envelope.payload.as_ref().unwrap()).expect("载荷应是 JSON");
    assert!(
        payload["scheduled_at"].is_string(),
        "载荷里应带上计划触发时刻：{payload}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 非法的_cron_表达式不影响其他触发器(pool: PgPool) {
    let h = harness(pool, "badcron").await;
    publish_flow(&h.store, "nightly").await;
    publish_flow(&h.store, "daily").await;

    triggers::upsert(
        h.store.pool(),
        &triggers::NewTrigger {
            flow_name: "nightly",
            kind: triggers::KIND_CRON,
            name: "broken",
            config: &json!({"expr": "这不是一个表达式"}),
        },
    )
    .await
    .unwrap();
    triggers::upsert(
        h.store.pool(),
        &triggers::NewTrigger {
            flow_name: "daily",
            kind: triggers::KIND_CRON,
            name: "ok",
            config: &json!({"expr": "* * * * * *"}),
        },
    )
    .await
    .unwrap();

    scheduler::spawn_cron(h.store.clone(), h.exec.clone(), h.shutdown.subscribe());

    assert!(
        wait_for_message(&h.exec, Duration::from_secs(8)).await,
        "一条写错的表达式不该让整轮调度停摆"
    );

    let broken = triggers::list_of_flow(h.store.pool(), "nightly")
        .await
        .unwrap();
    assert!(
        broken[0]
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("非法"),
        "表达式写错要记在触发器行上——控制台立刻看得见，而不是只剩「它好像没跑了」：{:?}",
        broken[0].last_error
    );

    let ok = wait_for_fired(&h.store, "daily", Duration::from_secs(8))
        .await
        .expect("另一条照常跑");
    assert_eq!(ok.last_error, None);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn mq_订阅能触发并消费掉消息(pool: PgPool) {
    let h = harness(pool, "mq").await;
    publish_flow(&h.store, "intake").await;

    let external = format!("test:external:{}", Ulid::generate());
    triggers::upsert(
        h.store.pool(),
        &triggers::NewTrigger {
            flow_name: "intake",
            kind: triggers::KIND_MQ,
            name: "orders",
            config: &json!({"stream": external}),
        },
    )
    .await
    .unwrap();

    scheduler::spawn_mq(
        h.store.clone(),
        h.exec.clone(),
        redis_url(),
        h.shutdown.subscribe(),
    );

    // 让订阅循环先起来并建好消费组，否则我们写进去的消息会被当成历史（建组用 `$`）
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = redis::Client::open(redis_url()).expect("地址合法");
    let mut conn = client.get_connection_manager().await.expect("应连上");
    let _: String = redis::cmd("XADD")
        .arg(&external)
        .arg("*")
        .arg("d")
        .arg(r#"{"orderId":"SO-1"}"#)
        .query_async(&mut conn)
        .await
        .expect("外部系统写入应成功");

    assert!(
        wait_for_message(&h.exec, Duration::from_secs(10)).await,
        "外部流里来了消息，订阅该触发一次"
    );

    let deliveries = h.exec.bus().receive().await.expect("应取到消息");
    let envelope = deliveries[0].message.envelope.as_ref().expect("应有信封");
    let payload =
        hub_proto::decode_payload(envelope.payload.as_ref().unwrap()).expect("载荷应是 JSON");
    assert_eq!(
        payload["orderId"], "SO-1",
        "外部消息的 d 字段原样成为载荷——外部系统不必学一套新格式"
    );

    wait_for_fired(&h.store, "intake", Duration::from_secs(10))
        .await
        .expect("要记下触发次数");

    // 处理完要把外部流上的消息摘掉，否则它会一直占着那边的内存
    let len: usize = redis::cmd("XLEN")
        .arg(&external)
        .query_async(&mut conn)
        .await
        .expect("查长度应成功");
    assert_eq!(len, 0, "触发完要从外部流上摘掉");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 外部消息不是_json_时记错误而不是静默丢弃(pool: PgPool) {
    let h = harness(pool, "badexternal").await;
    publish_flow(&h.store, "intake").await;

    let external = format!("test:external:{}", Ulid::generate());
    triggers::upsert(
        h.store.pool(),
        &triggers::NewTrigger {
            flow_name: "intake",
            kind: triggers::KIND_MQ,
            name: "orders",
            config: &json!({"stream": external}),
        },
    )
    .await
    .unwrap();

    scheduler::spawn_mq(
        h.store.clone(),
        h.exec.clone(),
        redis_url(),
        h.shutdown.subscribe(),
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = redis::Client::open(redis_url()).expect("地址合法");
    let mut conn = client.get_connection_manager().await.expect("应连上");
    let _: String = redis::cmd("XADD")
        .arg(&external)
        .arg("*")
        .arg("d")
        .arg("这不是 JSON")
        .query_async(&mut conn)
        .await
        .expect("写入应成功");

    // 等它被处理掉
    for _ in 0..40 {
        let len: usize = redis::cmd("XLEN")
            .arg(&external)
            .query_async(&mut conn)
            .await
            .unwrap_or(1);
        if len == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let rows = triggers::list_of_flow(h.store.pool(), "intake")
        .await
        .unwrap();
    assert!(
        rows[0]
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("解析失败"),
        "格式不对要说清是格式不对，否则上游只能靠猜为什么没触发：{:?}",
        rows[0].last_error
    );
    assert_eq!(
        h.exec.bus().depth().await.unwrap(),
        0,
        "解析不了就别往下投——投了也只会让下游拿到一份空载荷"
    );
}
