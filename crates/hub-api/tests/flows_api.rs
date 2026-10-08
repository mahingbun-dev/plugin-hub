//! 编排 HTTP 面的端到端：草稿 → 发布 → 触发 → 查执行记录。
//!
//! 这条链路决定生产流量走向，所以每一步都要求真实插件参与——用假插件测不出
//! 「编排里的节点到底有没有按声明跑起来」。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_api::{ApiState, SystemState, router};
use hub_engine::{FlowExecutor, FlowService, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_testkit as kit;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

struct Harness {
    app: Router,
    registry: Registry,
}

fn harness(store: Store) -> Harness {
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let flows = FlowService::new(
        store.clone(),
        FlowExecutor::new(registry.clone(), invoker.clone()),
    );
    let app = router(
        SystemState::without_metrics(),
        ApiState::new(store, registry.clone(), invoker, flows),
    );
    Harness { app, registry }
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.expect("请求处理失败");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体失败")
        .to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn post(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).expect("序列化失败")))
            .expect("构造请求失败"),
    )
    .await
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    send(
        app,
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("构造请求失败"),
    )
    .await
}

/// 一条 auth → validate 的两节点链。
fn chain_definition(name: &str) -> Value {
    json!({
        "name": name,
        "description": "测试编排",
        "nodes": [
            {"id": "a", "plugin": "flow-first"},
            {"id": "b", "plugin": "flow-second"}
        ],
        "edges": [{"from": "a", "to": "b"}]
    })
}

async fn register(fixture: &kit::Fixture, registry: &Registry) {
    let response = registry.register(&fixture.register_request(), None).await;
    assert!(response.accepted, "注册应通过：{:?}", response.rejections);
}

/// 起两个插件、注册、返回夹具。
async fn two_plugins(h: &Harness) -> (kit::Fixture, kit::Fixture) {
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    let second = kit::start(kit::Behavior::named("flow-second", "1.0.0")).await;
    register(&first, &h.registry).await;
    register(&second, &h.registry).await;
    (first, second)
}

// ---------------------------------------------------------------- 用例

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 保存草稿返回校验结果(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    let (status, body) = post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake"), "created_by": "tester"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["revision"]["revision"], 1);
    assert_eq!(body["revision"]["status"], "draft");
    assert_eq!(body["blocked"], false, "合规的编排不该被拦：{body}");
    assert!(body["issues"].as_array().is_some_and(Vec::is_empty));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 契约接不上时草稿能存但被标为阻断(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    // 故意引用一个没注册的插件
    let broken = json!({
        "name": "broken",
        "nodes": [{"id": "a", "plugin": "flow-first"}, {"id": "b", "plugin": "从未注册过"}],
        "edges": [{"from": "a", "to": "b"}]
    });

    let (status, body) = post(
        &h.app,
        "/flows/broken/draft",
        &json!({"definition": broken}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "有问题的草稿也该能存下来：{body}");
    assert_eq!(body["blocked"], true);
    let issues = body["issues"].as_array().expect("应给出问题列表");
    assert!(
        issues
            .iter()
            .any(|i| i["detail"].as_str().unwrap_or("").contains("从未注册过")),
        "应指出是哪个插件没注册：{issues:?}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 阻断性问题的草稿不能发布(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    let broken = json!({
        "name": "broken",
        "nodes": [{"id": "a", "plugin": "flow-first"}, {"id": "b", "plugin": "从未注册过"}],
        "edges": [{"from": "a", "to": "b"}]
    });
    post(
        &h.app,
        "/flows/broken/draft",
        &json!({"definition": broken}),
    )
    .await;

    let (status, body) = post(&h.app, "/flows/broken/publish", &json!({})).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("校验"),
        "应说清是校验没过：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 发布后再触发能跑通整条链(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (first, second) = two_plugins(&h).await;

    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;
    let (status, published) = post(&h.app, "/flows/intake/publish", &json!({})).await;
    assert_eq!(status, StatusCode::OK, "发布应成功：{published}");
    assert_eq!(published["status"], "published");

    let (status, body) = post(
        &h.app,
        "/flows/intake/trigger",
        &json!({"payload": {"orderId": "SO-1"}, "message_id": "msg-1"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["status"], "succeeded", "响应: {body}");
    assert_eq!(body["flow_revision"], 1);
    assert_eq!(body["nodes"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["nodes"][0]["node_id"], "a");
    assert_eq!(body["nodes"][1]["node_id"], "b");
    assert_eq!(body["payload"]["orderId"], "SO-1", "载荷要一路传下去");

    assert_eq!(first.handled_count(), 1);
    assert_eq!(second.handled_count(), 1);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 触发未发布的_flow_被拒(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;

    let (status, body) = post(
        &h.app,
        "/flows/intake/trigger",
        &json!({"payload": {"a": 1}}),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("尚未发布"),
        "应说清是没发布：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 触发未定义的_flow_被拒(pool: PgPool) {
    let h = harness(Store::from_pool(pool));

    let (status, body) = post(
        &h.app,
        "/flows/从未定义/trigger",
        &json!({"payload": {"a": 1}}),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert!(body["message"].as_str().unwrap_or("").contains("未定义"));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 执行记录可查且含节点明细(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;
    post(&h.app, "/flows/intake/publish", &json!({})).await;
    let (_status, triggered) = post(
        &h.app,
        "/flows/intake/trigger",
        &json!({"payload": {"orderId": "SO-2"}}),
    )
    .await;
    let run_id = triggered["run_id"]
        .as_str()
        .expect("应有 run_id")
        .to_string();

    // 列表
    let (status, runs) = get(&h.app, "/runs?flow=intake").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(runs.as_array().map(Vec::len), Some(1));
    assert_eq!(runs[0]["status"], "succeeded");

    // 详情：控制台靠它回答「这条数据卡在哪一跳」
    let (status, detail) = get(&h.app, &format!("/runs/{run_id}")).await;
    assert_eq!(status, StatusCode::OK, "响应: {detail}");
    let nodes = detail["nodes"].as_array().expect("应有节点明细");
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0]["node_id"], "a");
    assert_eq!(nodes[0]["plugin"], "flow-first");
    assert_eq!(nodes[0]["status"], "succeeded");
    assert!(
        nodes[0]["duration_ms"].is_number(),
        "要记耗时才能定位慢在哪"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 中间节点失败时错误指向具体节点(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    let failing = kit::start(kit::Behavior {
        fail_first: 99,
        ..kit::Behavior::named("flow-second", "1.0.0")
    })
    .await;
    register(&first, &h.registry).await;
    register(&failing, &h.registry).await;

    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;
    post(&h.app, "/flows/intake/publish", &json!({})).await;

    let (status, body) = post(
        &h.app,
        "/flows/intake/trigger",
        &json!({"payload": {"orderId": "SO-3"}}),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "触发本身成功，失败体现在结果里：{body}"
    );
    assert_eq!(body["status"], "failed");
    assert!(
        body["error"].as_str().unwrap_or("").contains("b"),
        "错误要指明是哪个节点：{body}"
    );
    assert_eq!(body["nodes"][1]["node_id"], "b");
    assert_eq!(body["nodes"][1]["status"], "failed");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 调用方的_traceparent_被沿用(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;
    post(&h.app, "/flows/intake/publish", &json!({})).await;

    let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let (status, body) = send(
        &h.app,
        Request::builder()
            .method("POST")
            .uri("/flows/intake/trigger")
            .header("content-type", "application/json")
            .header("traceparent", incoming)
            .body(Body::from(
                serde_json::to_vec(&json!({"payload": {"a": 1}})).expect("序列化失败"),
            ))
            .expect("构造请求失败"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(
        body["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736",
        "调用方带了 trace 就该沿用——否则同一个请求在两个系统里是两个 id"
    );
    assert!(
        body["traceparent"]
            .as_str()
            .unwrap_or("")
            .starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"),
        "回给调用方的 traceparent 应带上同一个 trace-id：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 非法_traceparent_退化为自己生成(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;
    post(&h.app, "/flows/intake/publish", &json!({})).await;

    let (status, body) = send(
        &h.app,
        Request::builder()
            .method("POST")
            .uri("/flows/intake/trigger")
            .header("content-type", "application/json")
            .header("traceparent", "这是个坏头")
            .body(Body::from(
                serde_json::to_vec(&json!({"payload": {"a": 1}})).expect("序列化失败"),
            ))
            .expect("构造请求失败"),
    )
    .await;

    // 一个坏 header 不该让请求失败
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(
        body["trace_id"].as_str().map(str::len),
        Some(32),
        "应退化成自己生成的 32 位十六进制 trace id：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 编排列表与详情可查(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake"), "description": "订单接入"}),
    )
    .await;
    post(&h.app, "/flows/intake/publish", &json!({})).await;
    // 发布后再改草稿：应当开出新的一版
    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;

    let (status, list) = get(&h.app, "/flows").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["published_revision"], 1);
    assert_eq!(list[0]["has_draft"], true, "发布后又改了草稿");

    let (status, detail) = get(&h.app, "/flows/intake").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        detail["published"].is_object(),
        "要能看到当前生效的那一版：{detail}"
    );
    assert!(detail["draft"].is_object(), "也要能看到草稿");
    assert_eq!(
        detail["revisions"].as_array().map(Vec::len),
        Some(2),
        "两版修订都要留档"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 查询未定义的_flow_返回_404(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (status, _) = get(&h.app, "/flows/不存在").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 调用链可按_trace_查询(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;
    post(&h.app, "/flows/intake/publish", &json!({})).await;
    let (_status, triggered) = post(
        &h.app,
        "/flows/intake/trigger",
        &json!({"payload": {"orderId": "SO-4"}}),
    )
    .await;
    let trace_id = triggered["trace_id"]
        .as_str()
        .expect("应有 trace_id")
        .to_string();

    // 列表：按 trace 聚合，一次执行只占一行
    let (status, traces) = get(&h.app, "/traces").await;
    assert_eq!(status, StatusCode::OK, "响应: {traces}");
    assert_eq!(traces.as_array().map(Vec::len), Some(1), "响应: {traces}");
    assert_eq!(traces[0]["trace_id"], trace_id);
    assert_eq!(
        traces[0]["span_count"], 3,
        "一个根 span + 两个节点 span：{traces}"
    );
    assert_eq!(traces[0]["error_count"], 0);

    // 详情：按开始时间排开的 span 列表
    let (status, detail) = get(&h.app, &format!("/traces/{trace_id}")).await;
    assert_eq!(status, StatusCode::OK, "响应: {detail}");

    let spans = detail["spans"].as_array().expect("应有 span 列表");
    assert_eq!(spans.len(), 3);

    let root = spans
        .iter()
        .find(|s| s["name"] == "flow:intake")
        .expect("应有根 span");
    assert_eq!(root["parent_span_id"], Value::Null, "根 span 没有父");
    assert!(root["attributes"]["node_count"].is_number());

    let node_spans: Vec<&Value> = spans.iter().filter(|s| s["node_id"].is_string()).collect();
    assert_eq!(node_spans.len(), 2, "两个节点各一个 span");
    assert!(
        node_spans
            .iter()
            .all(|s| s["parent_span_id"] == root["span_id"]),
        "节点 span 应挂在根 span 下"
    );
    assert!(
        node_spans
            .iter()
            .any(|s| s["attributes"]["plugin"] == "flow-first"),
        "span 上要能看到是哪个插件"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 失败的调用链在概览里被计入_error_count(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    let failing = kit::start(kit::Behavior {
        fail_first: 99,
        ..kit::Behavior::named("flow-second", "1.0.0")
    })
    .await;
    register(&first, &h.registry).await;
    register(&failing, &h.registry).await;

    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;
    post(&h.app, "/flows/intake/publish", &json!({})).await;
    post(
        &h.app,
        "/flows/intake/trigger",
        &json!({"payload": {"orderId": "SO-5"}}),
    )
    .await;

    let (status, traces) = get(&h.app, "/traces").await;
    assert_eq!(status, StatusCode::OK, "响应: {traces}");
    assert_eq!(
        traces[0]["error_count"], 2,
        "根 span 与失败的那个节点 span 都应计入：{traces}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 查询不存在的_trace_返回_404(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (status, _) = get(&h.app, "/traces/不存在的trace").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 配了_otlp_端点时_span_被推给_collector(pool: PgPool) {
    // 一个只记录收到什么的 mock collector
    #[derive(Clone, Default)]
    struct Collector {
        received: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    async fn collect(
        axum::extract::State(state): axum::extract::State<Collector>,
        axum::Json(body): axum::Json<Value>,
    ) -> &'static str {
        state.received.lock().expect("锁中毒").push(body);
        ""
    }

    let collector = Collector::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("监听失败");
    let addr = listener.local_addr().expect("取地址失败");
    let app = Router::new()
        .route("/v1/traces", axum::routing::post(collect))
        .with_state(collector.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // 带导出器的 harness
    let store = Store::from_pool(pool);
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let flows = FlowService::new(
        store.clone(),
        FlowExecutor::new(registry.clone(), invoker.clone()),
    )
    .with_exporter(hub_observe::exporter_for(Some(&format!(
        "http://{addr}/v1/traces"
    ))));
    let h = Harness {
        app: router(
            SystemState::without_metrics(),
            ApiState::new(store, registry.clone(), invoker, flows),
        ),
        registry,
    };

    let (_first, _second) = two_plugins(&h).await;
    post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake")}),
    )
    .await;
    post(&h.app, "/flows/intake/publish", &json!({})).await;
    post(
        &h.app,
        "/flows/intake/trigger",
        &json!({"payload": {"orderId": "SO-6"}}),
    )
    .await;

    // 导出是 spawn 出去的（旁路不该拖慢请求），所以这里等一会儿
    let mut payloads = Vec::new();
    for _ in 0..100 {
        payloads = collector.received.lock().expect("锁中毒").clone();
        if !payloads.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert_eq!(payloads.len(), 1, "应收到一批 span");
    let spans = payloads[0]["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .expect("应有 span 列表");
    assert_eq!(spans.len(), 3, "根 span + 两个节点 span");
    assert!(
        spans.iter().any(|s| s["name"] == "flow:intake"),
        "应包含根 span：{spans:?}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 未发布的_flow_可经_rename_改名(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    let (status, body) = post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake"), "created_by": "tester"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    let (status, body) = post(
        &h.app,
        "/flows/intake/rename",
        &json!({"name": "order-intake"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["name"], "order-intake");
    assert_eq!(body["published_revision"], 0);

    // 新名字下草稿还在，旧名字彻底查不到
    let (status, body) = get(&h.app, "/flows/order-intake").await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert!(body["draft"].is_object(), "草稿应跟过来: {body}");
    let (status, body) = get(&h.app, "/flows/intake").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "响应: {body}");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 发布过的_flow_rename_被拒(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (first, _second) = two_plugins(&h).await;

    let (status, body) = post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake"), "created_by": "tester"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    register(&first, &h.registry).await;
    let (status, body) = post(&h.app, "/flows/intake/publish", &json!({})).await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    let (status, body) = post(&h.app, "/flows/intake/rename", &json!({"name": "renamed"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("已发布过"),
        "应说明拒绝原因: {body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn rename_新名字非法被拒(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    let (status, _) = post(
        &h.app,
        "/flows/intake/draft",
        &json!({"definition": chain_definition("intake"), "created_by": "tester"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for bad in ["带 空格", "", "-开头", "工具名/路径"] {
        let (status, body) = post(&h.app, "/flows/intake/rename", &json!({"name": bad})).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "名字 {bad:?} 应被拒: {body}"
        );
    }
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn rename_不存在的_flow_返回_400_未定义(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (_first, _second) = two_plugins(&h).await;

    let (status, body) = post(
        &h.app,
        "/flows/从未定义/rename",
        &json!({"name": "other-name"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("未定义"),
        "响应: {body}"
    );
}
