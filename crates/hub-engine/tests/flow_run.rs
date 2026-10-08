//! 同步链执行引擎的集成测试：真实 gRPC 上跑多插件链路。
//!
//! 编排出错的方式很隐蔽——「某个节点悄悄没跑」「重试把一次故障放大成三次」
//! 「trace 断在中间某一跳」。这些只能靠真实链路才验得出来。

use std::sync::Arc;
use std::time::Duration;

use hub_engine::{FlowExecutor, Invoker, NodeStatus, RunStatus};
use hub_flow::{Edge, FlowDefinition, Node};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_proto::v1::Envelope;
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_testkit as kit;
use sqlx::PgPool;

fn node(id: &str, plugin: &str) -> Node {
    Node {
        id: id.to_string(),
        plugin: plugin.to_string(),
        version: None,
        timeout_ms: None,
        retries: None,
    }
}

fn edge(from: &str, to: &str) -> Edge {
    Edge {
        from: from.to_string(),
        to: to.to_string(),
    }
}

fn flow(name: &str, nodes: Vec<Node>, edges: Vec<Edge>) -> FlowDefinition {
    FlowDefinition {
        name: name.to_string(),
        description: String::new(),
        nodes,
        edges,
    }
}

struct Harness {
    registry: Registry,
    executor: FlowExecutor,
}

fn harness(store: Store) -> Harness {
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store,
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let executor = FlowExecutor::new(registry.clone(), invoker);
    Harness { registry, executor }
}

fn envelope(message_id: &str) -> Envelope {
    // 触发信封必须带载荷：链路的意义就是把数据一路传下去
    let payload =
        hub_proto::encode_payload(&serde_json::json!({"orderId": "SO-1"})).expect("构造载荷失败");

    Envelope {
        message_id: message_id.to_string(),
        trace_id: "trace-fixed-1".to_string(),
        deadline_ms: chrono::Utc::now().timestamp_millis() + 30_000,
        payload: Some(payload),
        ..Default::default()
    }
}

fn payload_of(envelope: &Envelope) -> serde_json::Value {
    hub_proto::decode_payload(envelope.payload.as_ref().expect("应有载荷")).expect("应是 JSON 载荷")
}

async fn register(fixture: &kit::Fixture, registry: &Registry) {
    let response = registry.register(&fixture.register_request(), None).await;
    assert!(response.accepted, "注册应通过：{:?}", response.rejections);
}

// ---------------------------------------------------------------- 用例

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 单节点编排跑通(pool: PgPool) {
    let a = kit::start(kit::Behavior::named("solo", "1.0.0")).await;
    let h = harness(Store::from_pool(pool));
    register(&a, &h.registry).await;

    let run = h
        .executor
        .run(
            &flow("single", vec![node("n1", "solo")], vec![]),
            envelope("msg-1"),
        )
        .await;

    assert!(run.succeeded(), "应成功：{:?}", run.error);
    assert_eq!(run.nodes.len(), 1);
    assert_eq!(run.nodes[0].node_id, "n1");
    assert_eq!(run.nodes[0].status, NodeStatus::Succeeded);
    assert_eq!(run.nodes[0].attempts, 1);
    assert_eq!(a.handled_count(), 1);

    let output = run.output.expect("应有输出");
    // 夹具把痕迹留在 meta 里，载荷则原样回显
    assert_eq!(
        output.meta.get("handled_by").map(String::as_str),
        Some("solo")
    );
    let payload = payload_of(&output);
    assert_eq!(
        payload["orderId"], "SO-1",
        "原始载荷要一路传下去，不能在中途丢掉"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 两节点链下游收到上游的输出(pool: PgPool) {
    let first = kit::start(kit::Behavior::named("first", "1.0.0")).await;
    let second = kit::start(kit::Behavior::named("second", "1.0.0")).await;
    let h = harness(Store::from_pool(pool));
    register(&first, &h.registry).await;
    register(&second, &h.registry).await;

    let run = h
        .executor
        .run(
            &flow(
                "chain",
                vec![node("a", "first"), node("b", "second")],
                vec![edge("a", "b")],
            ),
            envelope("msg-2"),
        )
        .await;

    assert!(run.succeeded(), "应成功：{:?}", run.error);
    assert_eq!(run.nodes.len(), 2, "两个节点都要跑");

    // 下游收到的信封应带着上游留下的痕迹
    let seen = second.last_envelope().expect("下游应收到信封");
    assert_eq!(
        seen.meta.get("handled_by").map(String::as_str),
        Some("first"),
        "下游的输入应来自上游的输出"
    );
    assert_eq!(seen.node_id, "b", "node_id 应逐跳更新");
    assert_eq!(
        seen.message_id, "msg-2",
        "message_id 是数据的幂等键，穿过多少插件都不能变"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 扇出时下游都执行(pool: PgPool) {
    let first = kit::start(kit::Behavior::named("fan-first", "1.0.0")).await;
    let left = kit::start(kit::Behavior::named("fan-left", "1.0.0")).await;
    let right = kit::start(kit::Behavior::named("fan-right", "1.0.0")).await;
    let h = harness(Store::from_pool(pool));
    register(&first, &h.registry).await;
    register(&left, &h.registry).await;
    register(&right, &h.registry).await;

    let run = h
        .executor
        .run(
            &flow(
                "fanout",
                vec![
                    node("a", "fan-first"),
                    node("b", "fan-left"),
                    node("c", "fan-right"),
                ],
                vec![edge("a", "b"), edge("a", "c")],
            ),
            envelope("msg-3"),
        )
        .await;

    assert!(run.succeeded(), "应成功：{:?}", run.error);
    assert_eq!(run.nodes.len(), 3);
    assert_eq!(left.handled_count(), 1);
    assert_eq!(right.handled_count(), 1);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 中间节点失败会短路下游(pool: PgPool) {
    let first = kit::start(kit::Behavior::named("bad-first", "1.0.0")).await;
    let second = kit::start(kit::Behavior::named("bad-second", "1.0.0")).await;

    // 让第一个节点一直失败
    let failing = kit::start(kit::Behavior {
        fail_first: 99,
        ..kit::Behavior::named("bad-failing", "1.0.0")
    })
    .await;

    let h = harness(Store::from_pool(pool));
    register(&first, &h.registry).await;
    register(&failing, &h.registry).await;
    register(&second, &h.registry).await;

    let run = h
        .executor
        .run(
            &flow(
                "short-circuit",
                vec![
                    node("a", "bad-first"),
                    node("b", "bad-failing"),
                    node("c", "bad-second"),
                ],
                vec![edge("a", "b"), edge("b", "c")],
            ),
            envelope("msg-4"),
        )
        .await;

    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.nodes.len(), 2, "第三个节点不该被执行");
    assert_eq!(run.nodes[1].status, NodeStatus::Failed);
    assert!(
        run.error.as_deref().unwrap_or("").contains("bad-failing"),
        "错误信息要指明是哪个节点，实际 {:?}",
        run.error
    );
    assert_eq!(second.handled_count(), 0, "下游绝不能在失败后被调用");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 节点按策略重试(pool: PgPool) {
    // 前一次失败、第二次成功
    let flaky = kit::start(kit::Behavior {
        fail_first: 1,
        ..kit::Behavior::named("flaky", "1.0.0")
    })
    .await;

    let h = harness(Store::from_pool(pool));
    register(&flaky, &h.registry).await;

    let mut n = node("n1", "flaky");
    n.retries = Some(2);

    let run = h
        .executor
        .run(&flow("retry", vec![n], vec![]), envelope("msg-5"))
        .await;

    assert!(run.succeeded(), "重试后应成功：{:?}", run.error);
    assert_eq!(run.nodes[0].attempts, 2, "应记录实际尝试了两次");
    assert_eq!(flaky.handled_count(), 2);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 未开重试时一次失败就结束(pool: PgPool) {
    let flaky = kit::start(kit::Behavior {
        fail_first: 1,
        ..kit::Behavior::named("no-retry", "1.0.0")
    })
    .await;

    let h = harness(Store::from_pool(pool));
    register(&flaky, &h.registry).await;

    let run = h
        .executor
        .run(
            &flow("no-retry", vec![node("n1", "no-retry")], vec![]),
            envelope("msg-6"),
        )
        .await;

    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.nodes[0].attempts, 1);
    assert_eq!(flaky.handled_count(), 1);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 节点超时按声明生效(pool: PgPool) {
    let slow = kit::start(kit::Behavior {
        delay: Some(Duration::from_secs(5)),
        ..kit::Behavior::named("slow", "1.0.0")
    })
    .await;

    let h = harness(Store::from_pool(pool));
    register(&slow, &h.registry).await;

    let mut n = node("n1", "slow");
    n.timeout_ms = Some(200);

    let started = std::time::Instant::now();
    let run = h
        .executor
        .run(&flow("timeout", vec![n], vec![]), envelope("msg-7"))
        .await;
    let elapsed = started.elapsed();

    assert_eq!(run.status, RunStatus::Failed);
    assert!(
        run.nodes[0].error.as_deref().unwrap_or("").contains("预算"),
        "应说清是超时，实际 {:?}",
        run.nodes[0].error
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "应按节点声明的 200ms 超时返回，实际耗时 {elapsed:?}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 信封预算比节点超时更紧时以信封为准(pool: PgPool) {
    let slow = kit::start(kit::Behavior {
        delay: Some(Duration::from_secs(5)),
        ..kit::Behavior::named("slow-budget", "1.0.0")
    })
    .await;

    let h = harness(Store::from_pool(pool));
    register(&slow, &h.registry).await;

    // 节点声明 10 秒，但信封只剩 200ms
    let mut n = node("n1", "slow-budget");
    n.timeout_ms = Some(10_000);

    let mut env = envelope("msg-8");
    env.deadline_ms = chrono::Utc::now().timestamp_millis() + 200;

    let started = std::time::Instant::now();
    let run = h.executor.run(&flow("budget", vec![n], vec![]), env).await;

    assert_eq!(run.status, RunStatus::Failed);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "信封的 deadline 更紧，应当以它为准"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 校验拒绝停止链路且不重试(pool: PgPool) {
    // reject_prefix 默认是 "bad-"，message_id 以它开头即被拒
    let strict = kit::start(kit::Behavior {
        fail_first: 0,
        ..kit::Behavior::named("strict", "1.0.0")
    })
    .await;
    let downstream = kit::start(kit::Behavior::named("strict-down", "1.0.0")).await;

    let h = harness(Store::from_pool(pool));
    register(&strict, &h.registry).await;
    register(&downstream, &h.registry).await;

    let mut n = node("a", "strict");
    n.retries = Some(3); // 校验拒绝不该触发重试

    let run = h
        .executor
        .run(
            &flow(
                "rejected",
                vec![n, node("b", "strict-down")],
                vec![edge("a", "b")],
            ),
            envelope("bad-1"),
        )
        .await;

    assert_eq!(run.status, RunStatus::Rejected, "校验拒绝是独立的状态");
    assert_eq!(run.nodes[0].status, NodeStatus::Rejected);
    assert_eq!(
        run.nodes[0].attempts, 1,
        "校验拒绝即使配了重试也只跑一次——同样的数据重发结果还是拒绝"
    );
    assert_eq!(strict.handled_count(), 0, "校验不通过时插件体绝不能被执行");
    assert_eq!(downstream.handled_count(), 0);
    assert!(
        run.nodes[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("message_id"),
        "拒绝原因应指出问题字段，实际 {:?}",
        run.nodes[0].error
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn trace_贯穿而_run_同一份(pool: PgPool) {
    let first = kit::start(kit::Behavior::named("t-first", "1.0.0")).await;
    let second = kit::start(kit::Behavior::named("t-second", "1.0.0")).await;
    let h = harness(Store::from_pool(pool));
    register(&first, &h.registry).await;
    register(&second, &h.registry).await;

    let run = h
        .executor
        .run(
            &flow(
                "trace",
                vec![node("a", "t-first"), node("b", "t-second")],
                vec![edge("a", "b")],
            ),
            envelope("msg-9"),
        )
        .await;

    assert!(run.succeeded(), "{:?}", run.error);

    let at_first = first.last_envelope().expect("第一个节点应收到信封");
    let at_second = second.last_envelope().expect("第二个节点应收到信封");

    assert_eq!(at_first.trace_id, "trace-fixed-1");
    assert_eq!(at_second.trace_id, "trace-fixed-1", "trace 必须贯穿全程");
    assert_eq!(at_first.run_id, run.run_id);
    assert_eq!(at_second.run_id, run.run_id, "同一次执行共用一个 run_id");
    assert_eq!(at_first.node_id, "a");
    assert_eq!(at_second.node_id, "b");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件不可达时报失败而不是挂住(pool: PgPool) {
    let ghost = kit::start(kit::Behavior::named("ghost", "1.0.0")).await;
    let h = harness(Store::from_pool(pool));
    register(&ghost, &h.registry).await;

    ghost.stop().await;

    let started = std::time::Instant::now();
    let run = h
        .executor
        .run(
            &flow("ghost", vec![node("n1", "ghost")], vec![]),
            envelope("msg-10"),
        )
        .await;

    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.nodes[0].error.is_some());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "不可达应快速失败，实际 {started:?}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 节点结果能算出编排自身开销(pool: PgPool) {
    let a = kit::start(kit::Behavior::named("overhead", "1.0.0")).await;
    let h = harness(Store::from_pool(pool));
    register(&a, &h.registry).await;

    let run = h
        .executor
        .run(
            &flow("overhead", vec![node("n1", "overhead")], vec![]),
            envelope("msg-11"),
        )
        .await;

    assert!(run.succeeded());
    assert!(
        run.node_time_ms() <= run.elapsed_ms,
        "插件耗时之和不该超过总耗时"
    );
}
