//! 异步链：节点级消费、投递与收敛。
//!
//! M3 的验收是「插件重启 / 网络抖动 / 消费失败重投下不丢消息」。这条性质无法靠打桩
//! 验证——它说的正是「真实的重投发生时会怎样」。所以这里用**真实 Redis**（消费组、
//! 接管、重投都是 Redis 侧的行为）配**真实插件**（节点跑没跑过看得到侧效应）。

use std::sync::Arc;
use std::time::Duration;

use hub_bus::{Bus, BusConfig};
use hub_engine::{AsyncConfig, AsyncExecutor, Disposition, FlowExecutor, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_store::flows::{self, DraftInput};
use hub_testkit as kit;
use serde_json::json;
use sqlx::PgPool;
use ulid::Ulid;

/// 测试用的 Redis。
///
/// 默认硬编码到本地 db 9，**刻意不读 `REDIS_URL`**：`.env` 里的那个指向应用的
/// db 2，测试键混进去会污染正在跑的中台。
/// `TEST_REDIS_URL` 只用来在 CI 里指向另一个端口——CI 自己起一个 Redis，
/// 不假定 runner 上正好有一个（跟测试 PG 用非默认端口是同一个理由）。
fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/9".to_string())
}

struct Harness {
    store: Store,
    registry: Registry,
    exec: AsyncExecutor,
}

/// 每个用例独占一条 Stream，与应用的 db 2 也分开。
async fn harness(pool: PgPool, tag: &str) -> Harness {
    let store = Store::from_pool(pool);
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let executor = FlowExecutor::new(registry.clone(), invoker);

    let bus = Bus::connect(
        &redis_url(),
        BusConfig {
            stream: format!("test:async:{tag}:{}", Ulid::generate()),
            group: "test-workers".to_string(),
            consumer: "test-consumer".to_string(),
            block: Duration::from_millis(100),
            ..BusConfig::default()
        },
    )
    .await
    .expect("连接 Redis 失败——本机 6379 上应有 Redis");

    let exec = AsyncExecutor::new(store.clone(), bus, executor).with_config(AsyncConfig {
        idempotency_ttl: Duration::from_secs(60),
    });

    Harness {
        store,
        registry,
        exec,
    }
}

async fn register(fixture: &kit::Fixture, registry: &Registry) {
    let response = registry.register(&fixture.register_request(), None).await;
    assert!(response.accepted, "注册应通过：{:?}", response.rejections);
}

/// 建一条 a → b → c 的链并发布。
async fn publish_chain(store: &Store, name: &str) {
    flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name,
            description: "异步链测试",
            definition: &json!({
                "name": name,
                "nodes": [
                    {"id": "a", "plugin": "flow-first"},
                    {"id": "b", "plugin": "flow-second"},
                    {"id": "c", "plugin": "flow-third"}
                ],
                "edges": [{"from": "a", "to": "b"}, {"from": "b", "to": "c"}]
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

/// 一直取到没有消息为止（或到达轮数上限，防止写错时死循环）。
async fn drain(exec: &AsyncExecutor, max_rounds: usize) -> usize {
    let mut processed = 0;
    for _ in 0..max_rounds {
        let n = exec.run_once().await.expect("消费应成功");
        if n == 0 {
            break;
        }
        processed += n;
    }
    processed
}

fn trigger(message_id: &str) -> hub_proto::Envelope {
    hub_proto::Envelope {
        message_id: message_id.to_string(),
        trace_id: "trace-async-1".to_string(),
        payload: Some(hub_proto::encode_payload(&json!({"orderId": "SO-1"})).expect("编码")),
        ..Default::default()
    }
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 三个节点的链能异步跑通(pool: PgPool) {
    let h = harness(pool, "chain").await;
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    let second = kit::start(kit::Behavior::named("flow-second", "1.0.0")).await;
    let third = kit::start(kit::Behavior::named("flow-third", "1.0.0")).await;
    register(&first, &h.registry).await;
    register(&second, &h.registry).await;
    register(&third, &h.registry).await;

    publish_chain(&h.store, "intake").await;

    let run_id = h
        .exec
        .enqueue("intake", trigger("msg-1"), Some(json!({"kind": "test"})))
        .await
        .expect("入队应成功");

    // 入队那一刻 run 就该在库里——「投递失败」因此是个看得见的状态，而不是查不到的消息
    let queued = hub_store::runs::find_run(h.store.pool(), &run_id)
        .await
        .unwrap()
        .expect("入队后就该有 run 行");
    assert_eq!(queued.status, "queued");

    let processed = drain(&h.exec, 30).await;
    assert_eq!(processed, 3, "三个节点各一条消息");

    let run = hub_store::runs::find_run(h.store.pool(), &run_id)
        .await
        .unwrap()
        .expect("应查得到");
    assert_eq!(run.status, "succeeded", "全部节点跑完该收敛成成功");
    assert!(run.finished_at.is_some(), "终态要带上完成时间");

    let nodes = hub_store::runs::list_run_nodes(h.store.pool(), &run_id)
        .await
        .unwrap();
    assert_eq!(nodes.len(), 3);
    assert_eq!(
        nodes.iter().map(|n| n.node_id.as_str()).collect::<Vec<_>>(),
        vec!["a", "b", "c"],
        "节点按依赖顺序执行"
    );
    assert!(nodes.iter().all(|n| n.status == "succeeded"));

    assert_eq!(first.handled_count(), 1);
    assert_eq!(second.handled_count(), 1);
    assert_eq!(third.handled_count(), 1);

    // 载荷一路传下去，逐跳字段各自更新
    let seen = third.last_envelope().expect("第三个节点应收到信封");
    assert_eq!(seen.run_id, run_id, "run_id 逐跳更新成同一次执行");
    assert_eq!(seen.node_id, "c");
    assert_eq!(seen.message_id, "msg-1", "message_id 是数据的幂等键，不变");
    assert_eq!(seen.trace_id, "trace-async-1", "trace 贯穿全程");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 同一个节点被重投时不会重复执行(pool: PgPool) {
    let h = harness(pool, "redeliver").await;
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    register(&first, &h.registry).await;
    publish_chain(&h.store, "intake").await;

    h.exec
        .enqueue("intake", trigger("msg-2"), None)
        .await
        .expect("入队应成功");

    // 取到入口节点那条消息，但先不 ACK——模拟「处理完之后进程被杀，消息没 ACK」
    let deliveries = h.exec.bus().receive().await.expect("应取到消息");
    assert_eq!(deliveries.len(), 1);
    let delivery = &deliveries[0];

    assert_eq!(
        h.exec.handle(delivery).await.expect("第一次处理应成功"),
        Disposition::Done
    );
    assert_eq!(first.handled_count(), 1);

    // 同一条消息再来一次（重投）
    assert_eq!(
        h.exec.handle(delivery).await.expect("重投处理不该报错"),
        Disposition::Duplicate,
        "已经跑完过的节点，重投时要跳过而不是再跑一遍"
    );
    assert_eq!(
        first.handled_count(),
        1,
        "插件绝不能被执行第二次——这正是幂等键存在的理由"
    );

    let nodes = hub_store::runs::list_run_nodes(h.store.pool(), &delivery.message.run_id)
        .await
        .unwrap();
    assert_eq!(nodes.len(), 1, "重投不该多写一条执行记录");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 上一个持有者没做完时会被接管重做(pool: PgPool) {
    let h = harness(pool, "takeover").await;
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    register(&first, &h.registry).await;
    publish_chain(&h.store, "intake").await;

    h.exec
        .enqueue("intake", trigger("msg-3"), None)
        .await
        .expect("入队应成功");

    let deliveries = h.exec.bus().receive().await.expect("应取到消息");
    let delivery = &deliveries[0];

    // 手工造出「认领了但没做完」：键在，run_nodes 里却没有这个节点。
    // 这正是进程在「认领之后、跑完之前」被杀留下的现场。
    let key = hub_store::idempotency::node_key(&delivery.message.run_id, &delivery.message.node_id);
    hub_store::idempotency::claim(
        h.store.pool(),
        &key,
        Some(&delivery.message.run_id),
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .expect("手工认领应成功");
    assert!(
        !hub_store::runs::node_done(h.store.pool(), &delivery.message.run_id, "a")
            .await
            .unwrap(),
        "前提：这个节点还没跑过"
    );

    assert_eq!(
        h.exec.handle(delivery).await.expect("处理应成功"),
        Disposition::Done,
        "只看幂等键就跳过会让这个节点永远没人跑——那是静默丢数据，比重复执行糟糕得多"
    );
    assert_eq!(first.handled_count(), 1, "接管之后确实重做了一次");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 中间节点失败会短路下游并收敛(pool: PgPool) {
    let h = harness(pool, "failure").await;
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    // b 永远失败
    let second = kit::start(kit::Behavior {
        fail_first: usize::MAX,
        ..kit::Behavior::named("flow-second", "1.0.0")
    })
    .await;
    let third = kit::start(kit::Behavior::named("flow-third", "1.0.0")).await;
    register(&first, &h.registry).await;
    register(&second, &h.registry).await;
    register(&third, &h.registry).await;

    publish_chain(&h.store, "intake").await;

    let run_id = h
        .exec
        .enqueue("intake", trigger("msg-4"), None)
        .await
        .expect("入队应成功");

    // 失败即短路：b 失败后 c 的消息根本不会被投出去。所以这里的轮数上限给得比
    // 「消息条数」宽——失败消息不 ACK 会让它被反复取出（重投），直到 attempt 耗尽。
    let _ = drain(&h.exec, 5).await;

    let run = hub_store::runs::find_run(h.store.pool(), &run_id)
        .await
        .unwrap()
        .expect("应查得到");
    assert_eq!(run.status, "failed", "节点失败该收敛成 failed");
    assert!(
        run.error.as_deref().unwrap_or("").contains('b'),
        "错误要指明是哪个节点：{:?}",
        run.error
    );
    assert!(run.finished_at.is_some());

    assert_eq!(third.handled_count(), 0, "失败即短路，下游不该被执行");

    let nodes = hub_store::runs::list_run_nodes(h.store.pool(), &run_id)
        .await
        .unwrap();
    assert_eq!(nodes.len(), 2, "只该有 a 和 b 两条执行记录");
    assert_eq!(nodes[1].node_id, "b");
    assert_eq!(nodes[1].status, "failed");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 校验器拒绝时收敛成_rejected(pool: PgPool) {
    let h = harness(pool, "rejected").await;
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    register(&first, &h.registry).await;
    publish_chain(&h.store, "intake").await;

    // 默认行为：message_id 以 bad- 开头时校验器拒绝
    let run_id = h
        .exec
        .enqueue("intake", trigger("bad-async"), None)
        .await
        .expect("入队应成功");

    let _ = drain(&h.exec, 5).await;

    let run = hub_store::runs::find_run(h.store.pool(), &run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        run.status, "rejected",
        "拒绝与失败要分开：前者是数据没过规则（调用方改数据就能解决），后者是插件出错"
    );
    assert_eq!(first.handled_count(), 0, "校验不通过时插件体绝不能被执行");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 入队未发布的_flow_被拒(pool: PgPool) {
    let h = harness(pool, "unpublished").await;
    let err = h
        .exec
        .enqueue("从来没发布过", trigger("msg-5"), None)
        .await
        .expect_err("未定义/未发布都不该入队");
    assert!(err.to_string().contains("从来没发布过"), "{err}");
}
