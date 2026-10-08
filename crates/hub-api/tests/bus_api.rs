//! 总线 HTTP 面的端到端：异步触发、死信查看与重放。
//!
//! 用**真实 Redis**：异步触发的返回值里那个 run_id 到底有没有换来一条真实的排队消息，
//! 只有把消息取出来看才算数。死信重放同理——它的价值在于「真的产生了新的一次执行」。

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_api::{ApiState, SystemState, router};
use hub_bus::{Bus, BusConfig};
use hub_engine::{AsyncExecutor, FlowExecutor, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_store::dead_letters;
use hub_store::flows::{self, DraftInput};
use hub_testkit as kit;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;
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
    app: Router,
    registry: Registry,
    store: Store,
    bus: Bus,
}

/// `with_async` 为假时不装配总线——用来验证「能力没开」时的回答。
async fn harness(pool: PgPool, tag: &str, with_async: bool) -> Harness {
    let store = Store::from_pool(pool);
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let executor = FlowExecutor::new(registry.clone(), invoker.clone());
    let flows = hub_engine::FlowService::new(store.clone(), executor.clone());

    let bus = Bus::connect(
        &redis_url(),
        BusConfig {
            stream: format!("test:busapi:{tag}:{}", Ulid::generate()),
            group: "test-workers".to_string(),
            consumer: "test-consumer".to_string(),
            block: Duration::from_millis(100),
            ..BusConfig::default()
        },
    )
    .await
    .expect("连接 Redis 失败——本机 6379 上应有 Redis");

    let mut state = ApiState::new(store.clone(), registry.clone(), invoker, flows);
    if with_async {
        state = state.with_async(AsyncExecutor::new(store.clone(), bus.clone(), executor));
    }

    Harness {
        app: router(SystemState::without_metrics(), state),
        registry,
        store,
        bus,
    }
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
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

async fn register(fixture: &kit::Fixture, registry: &Registry) {
    let response = registry.register(&fixture.register_request(), None).await;
    assert!(response.accepted, "注册应通过：{:?}", response.rejections);
}

/// 建一条 a → b 的链并发布。
async fn publish_chain(store: &Store, name: &str) {
    flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name,
            description: "总线面测试",
            definition: &json!({
                "name": name,
                "nodes": [
                    {"id": "a", "plugin": "flow-first"},
                    {"id": "b", "plugin": "flow-second"}
                ],
                "edges": [{"from": "a", "to": "b"}]
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

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 异步触发返回_202_并真的排上了队(pool: PgPool) {
    let h = harness(pool, "trigger", true).await;
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    let second = kit::start(kit::Behavior::named("flow-second", "1.0.0")).await;
    register(&first, &h.registry).await;
    register(&second, &h.registry).await;
    publish_chain(&h.store, "intake").await;

    let (status, body) = post(
        &h.app,
        "/flows/intake/trigger-async",
        &json!({"payload": {"orderId": "SO-1"}, "message_id": "msg-1"}),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "202 而不是 200：这次请求**没有**跑完编排，只是把它排上了队：{body}"
    );
    let run_id = body["run_id"].as_str().expect("应有 run_id");
    assert_eq!(body["trace_id"].as_str().map(str::len), Some(32));
    assert!(
        body["traceparent"]
            .as_str()
            .unwrap_or("")
            .starts_with("00-"),
        "回给调用方的 traceparent 要能直接接进它自己的链路：{body}"
    );

    // 入队那一刻 run 就该在库里
    let run = hub_store::runs::find_run(h.store.pool(), run_id)
        .await
        .unwrap()
        .expect("入队后就该有 run 行");
    assert_eq!(run.status, "queued");

    // 而且总线上真的有一条消息，内容对得上
    let deliveries = h.bus.receive().await.expect("应取到消息");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].message.run_id, run_id);
    assert_eq!(deliveries[0].message.node_id, "a", "先投入口节点");
    assert_eq!(
        deliveries[0].message.flow_revision, 1,
        "消息里锁定了版本：不锁会让同一次执行的前半段跑 v1、后半段跑 v2"
    );

    let envelope = deliveries[0].message.envelope.as_ref().expect("应有信封");
    assert_eq!(envelope.message_id, "msg-1", "调用方指定的 id 应被沿用");
    assert_eq!(envelope.run_id, run_id);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 未装配总线时异步触发返回_503(pool: PgPool) {
    let h = harness(pool, "noasync", false).await;

    let (status, body) = post(
        &h.app,
        "/flows/intake/trigger-async",
        &json!({"payload": {"a": 1}}),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "响应: {body}");
    assert_eq!(
        body["error"], "unavailable",
        "要说清是「这个能力没开」，而不是让调用方以为消息已经发出去了：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 异步触发未发布的_flow_返回_400(pool: PgPool) {
    let h = harness(pool, "unpublished", true).await;

    let (status, body) = post(
        &h.app,
        "/flows/从来没发布过/trigger-async",
        &json!({"payload": {"a": 1}}),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("从来没发布过"),
        "要说清是哪个 flow：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 死信可查且载荷摘要是对象(pool: PgPool) {
    let h = harness(pool, "deadletter", true).await;

    let id = dead_letters::insert(
        h.store.pool(),
        &dead_letters::NewDeadLetter {
            stream: "hub:flows",
            stream_id: "1-1",
            run_id: Some("run-x"),
            flow_name: Some("intake"),
            node_id: Some("b"),
            attempts: 5,
            error: "插件不可达",
            payload_summary: Some(r#"{"payload_bytes":42}"#),
        },
    )
    .await
    .expect("写死信应成功");

    let (status, list) = get(&h.app, "/dead-letters").await;
    assert_eq!(status, StatusCode::OK, "响应: {list}");
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["flow_name"], "intake");
    assert_eq!(list[0]["attempts"], 5);
    assert_eq!(
        list[0]["payload_summary"]["payload_bytes"], 42,
        "摘要要以对象形态给出去，控制台不该自己去 JSON.parse 一个字符串"
    );

    let (status, detail) = get(&h.app, &format!("/dead-letters/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["error"], "插件不可达");
    assert_eq!(detail["replayed_at"], Value::Null, "还没重放过");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 重放产生新执行并留下追踪(pool: PgPool) {
    let h = harness(pool, "replay", true).await;
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    let second = kit::start(kit::Behavior::named("flow-second", "1.0.0")).await;
    register(&first, &h.registry).await;
    register(&second, &h.registry).await;
    publish_chain(&h.store, "intake").await;

    let id = dead_letters::insert(
        h.store.pool(),
        &dead_letters::NewDeadLetter {
            stream: "hub:flows",
            stream_id: "2-1",
            run_id: Some("run-original"),
            flow_name: Some("intake"),
            node_id: Some("b"),
            attempts: 5,
            error: "插件不可达",
            payload_summary: None,
        },
    )
    .await
    .expect("写死信应成功");

    let (status, body) = post(
        &h.app,
        &format!("/dead-letters/{id}/replay"),
        &json!({"payload": {"orderId": "SO-9"}}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "响应: {body}");

    let new_run = body["run_id"].as_str().expect("应返回新 run_id");
    assert_ne!(new_run, "run-original");

    // 死信上要留下「重放成了哪次新执行」——否则重放之后就没法追踪了
    let letter = dead_letters::find(h.store.pool(), id)
        .await
        .unwrap()
        .expect("应查得到");
    assert_eq!(letter.replayed_run_id.as_deref(), Some(new_run));
    assert!(letter.replayed_at.is_some());

    // 新执行真的排上了队
    let deliveries = h.bus.receive().await.expect("应取到消息");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].message.run_id, new_run);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 重放要换新的_message_id(pool: PgPool) {
    let h = harness(pool, "replayid", true).await;
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    let second = kit::start(kit::Behavior::named("flow-second", "1.0.0")).await;
    register(&first, &h.registry).await;
    register(&second, &h.registry).await;
    publish_chain(&h.store, "intake").await;

    // 原始执行：先入队一次，记下它的 message_id
    let (_, original) = post(
        &h.app,
        "/flows/intake/trigger-async",
        &json!({"payload": {"orderId": "SO-1"}, "message_id": "msg-original"}),
    )
    .await;
    let original_run = original["run_id"].as_str().unwrap().to_string();
    let deliveries = h.bus.receive().await.expect("应取到原消息");
    let original_message_id = deliveries[0]
        .message
        .envelope
        .as_ref()
        .unwrap()
        .message_id
        .clone();
    assert_eq!(original_message_id, "msg-original");

    let id = dead_letters::insert(
        h.store.pool(),
        &dead_letters::NewDeadLetter {
            stream: "hub:flows",
            stream_id: "3-1",
            run_id: Some(&original_run),
            flow_name: Some("intake"),
            node_id: Some("b"),
            attempts: 5,
            error: "插件不可达",
            payload_summary: None,
        },
    )
    .await
    .expect("写死信应成功");

    let (status, _) = post(
        &h.app,
        &format!("/dead-letters/{id}/replay"),
        &json!({"payload": {"orderId": "SO-1"}}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let deliveries = h.bus.receive().await.expect("应取到重放的消息");
    let replayed_message_id = deliveries[0]
        .message
        .envelope
        .as_ref()
        .unwrap()
        .message_id
        .clone();

    assert_ne!(
        replayed_message_id, "msg-original",
        "重放**必须换一个新的 message_id**：message_id 是数据的幂等键，\
         沿用旧的会让下游插件把这次重放当成「那条数据又来了」按幂等跳过，重放就白做了"
    );
    assert!(!replayed_message_id.is_empty());
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 重复重放被拒(pool: PgPool) {
    let h = harness(pool, "twice", true).await;
    let first = kit::start(kit::Behavior::named("flow-first", "1.0.0")).await;
    let second = kit::start(kit::Behavior::named("flow-second", "1.0.0")).await;
    register(&first, &h.registry).await;
    register(&second, &h.registry).await;
    publish_chain(&h.store, "intake").await;

    let id = dead_letters::insert(
        h.store.pool(),
        &dead_letters::NewDeadLetter {
            stream: "hub:flows",
            stream_id: "4-1",
            run_id: None,
            flow_name: Some("intake"),
            node_id: None,
            attempts: 5,
            error: "e",
            payload_summary: None,
        },
    )
    .await
    .unwrap();

    let body = json!({"payload": {"a": 1}});
    let (first_status, _) = post(&h.app, &format!("/dead-letters/{id}/replay"), &body).await;
    assert_eq!(first_status, StatusCode::ACCEPTED);

    let (status, response) = post(&h.app, &format!("/dead-letters/{id}/replay"), &body).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "重放按钮是最容易被连点的那种，第二次不该再产生一次执行：{response}"
    );
    assert_eq!(response["error"], "conflict");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 重放不存在的死信返回_404(pool: PgPool) {
    let h = harness(pool, "missing", true).await;
    let (status, _) = post(
        &h.app,
        "/dead-letters/999999/replay",
        &json!({"payload": {"a": 1}}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 没有编排归属的死信无法重放(pool: PgPool) {
    let h = harness(pool, "noflow", true).await;

    let id = dead_letters::insert(
        h.store.pool(),
        &dead_letters::NewDeadLetter {
            stream: "hub:flows",
            stream_id: "5-1",
            run_id: None,
            // 没有 flow_name：这条消息当时不知道属于哪条编排
            flow_name: None,
            node_id: None,
            attempts: 5,
            error: "e",
            payload_summary: None,
        },
    )
    .await
    .unwrap();

    let (status, body) = post(
        &h.app,
        &format!("/dead-letters/{id}/replay"),
        &json!({"payload": {"a": 1}}),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("编排"),
        "要说清为什么放不了：{body}"
    );
}
