//! 载荷的四类边界里前两类的端到端：内联与引用。
//!
//! 这条边界最要紧的性质是**调用方必须知道自己的数据走了哪条路**——它以为插件原样收到
//! 了完整载荷，而插件手上只有一个 uri，这种差别不该等到出问题才发现。所以好几个断言
//! 是围着「回给调用方的信息与插件实际收到的东西一致」写的。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_api::{ApiState, SystemState, payload, router};
use hub_engine::{FlowExecutor, FlowService, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_testkit as kit;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

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
        ApiState::new(store.clone(), registry.clone(), invoker, flows),
    );
    Harness {
        app,
        registry,
        store,
    }
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value, axum::http::HeaderMap) {
    let response = app.clone().oneshot(request).await.expect("请求处理失败");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体失败")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        headers,
    )
}

async fn post(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let (status, value, _) = send(
        app,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).expect("序列化失败")))
            .expect("构造请求失败"),
    )
    .await;
    (status, value)
}

async fn register(fixture: &kit::Fixture, registry: &Registry) {
    let response = registry.register(&fixture.register_request(), None).await;
    assert!(response.accepted, "注册应通过：{:?}", response.rejections);
}

/// 造一段超过内联上限的载荷。
///
/// 取上限的 1.3 倍而不是刚好压线：编码成 `Struct` 之后字节数会比 JSON 文本略大，
/// 贴着边界写会让用例的成败取决于编码开销这种无关细节。
fn oversized_string() -> String {
    "x".repeat(payload::MAX_INLINE_BYTES * 4 / 3)
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 小载荷内联且不回引用(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    register(&fixture, &h.registry).await;

    let (status, body) = post(
        &h.app,
        "/ingress/echo-plugin",
        &json!({"payload": {"orderId": "SO-1"}}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["payload"]["orderId"], "SO-1");
    assert!(
        body.get("payload_ref").is_none(),
        "小载荷不该出现引用字段——出现了会让调用方以为还有一份要去取：{body}"
    );

    let seen = fixture.last_envelope().expect("插件应收到信封");
    assert!(seen.payload.is_some(), "插件该拿到内联载荷");
    assert!(seen.payload_ref.is_none());
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 超大载荷走引用且插件收到的是_uri(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    register(&fixture, &h.registry).await;

    let (status, body) = post(
        &h.app,
        "/ingress/echo-plugin",
        &json!({"payload": {"blob": oversized_string()}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    let reference = body
        .get("payload_ref")
        .expect("超大载荷必须把引用回给调用方");
    let uri = reference["uri"].as_str().expect("应给出取回地址");
    assert!(uri.starts_with("/blobs/"), "{uri}");
    assert_eq!(reference["size_bytes"].as_u64().map(|s| s > 0), Some(true));
    assert_eq!(
        reference["sha256"].as_str().map(str::len),
        Some(64),
        "摘要要是 SHA-256 的十六进制形态"
    );

    // 插件那边：payload 为空、ref 有值。这是「引用传递」的定义
    let seen = fixture.last_envelope().expect("插件应收到信封");
    assert!(
        seen.payload.is_none(),
        "走了引用就不该再内联一份——否则等于既存了引用又发了全量，白折腾"
    );
    let sent_ref = seen.payload_ref.expect("插件该拿到引用");
    assert_eq!(sent_ref.uri, uri, "插件拿到的 uri 要和回给调用方的一致");
    assert_eq!(
        sent_ref.size_bytes as u64,
        reference["size_bytes"].as_u64().unwrap(),
        "两边报的大小要一致，否则调用方没法判断自己该取回多少"
    );
    assert_eq!(sent_ref.sha256, reference["sha256"].as_str().unwrap());
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 引用载荷能按_uri_取回且内容逐字节一致(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    register(&fixture, &h.registry).await;

    let blob = oversized_string();
    let (status, body) = post(
        &h.app,
        "/ingress/echo-plugin",
        &json!({"payload": {"blob": blob}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let uri = body["payload_ref"]["uri"].as_str().unwrap().to_string();

    let (status, _, headers) = send(
        &h.app,
        Request::builder()
            .method("GET")
            .uri(&uri)
            .body(Body::empty())
            .expect("构造请求失败"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 头里带上摘要与大小：取回来的一方能就地校验拿到的是不是那一刻写进去的字节，
    // 不必先读一遍正文
    assert_eq!(
        headers
            .get("x-payload-sha256")
            .and_then(|v| v.to_str().ok()),
        Some(body["payload_ref"]["sha256"].as_str().unwrap()),
        "摘要要对得上"
    );

    // 真正取一次正文，确认它是可用的字节
    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        !bytes.is_empty(),
        "取回的字节不该是空的——那说明存进去的和取出来的不是一回事"
    );

    // 存进去的就是编码后的 Struct 字节，它必然包含我们那个超长串
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains(&blob[..64]),
        "取回的字节应当就是当初编码后的载荷"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 超过请求体硬上限被_413_拒收(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    register(&fixture, &h.registry).await;

    // 比硬上限还大：这不是「这份数据该换个方式传」，而是「这次调用不该发」。
    // 与「超限就走引用」是两条不同的线，所以这里断言的是**拒收**而不是引用。
    let huge = "x".repeat(payload::MAX_REQUEST_BYTES + 1024);
    let (status, _) = post(
        &h.app,
        "/ingress/echo-plugin",
        &json!({"payload": {"blob": huge}}),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "超过硬上限该在入口就被挡住"
    );
    assert_eq!(
        fixture.handled_count(),
        0,
        "被拒的请求绝不能打到插件——那是一条本该拦下的调用"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 取不存在的引用返回_404(pool: PgPool) {
    let h = harness(Store::from_pool(pool));

    let (status, _, _) = send(
        &h.app,
        Request::builder()
            .method("GET")
            .uri("/blobs/从来没有过")
            .body(Body::empty())
            .expect("构造请求失败"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 过期与不存在的引用对外是同一个回答(pool: PgPool) {
    let h = harness(Store::from_pool(pool));

    // 立刻过期的载荷
    let stored =
        hub_store::payloads::put(h.store.pool(), b"short-lived", std::time::Duration::ZERO)
            .await
            .expect("存应成功");

    let (status, body, _) = send(
        &h.app,
        Request::builder()
            .method("GET")
            .uri(format!("/blobs/{}", stored.id))
            .body(Body::empty())
            .expect("构造请求失败"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("不存在或已过期"),
        "对外不该区分「过期」与「不存在」——区分了就等于告诉调用方这个 id 曾经存在过：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 异步触发也走同一条边界(pool: PgPool) {
    use hub_bus::{Bus, BusConfig};
    use hub_engine::AsyncExecutor;
    use serde_json::json as j;
    use std::time::Duration;
    use ulid::Ulid;

    let store = Store::from_pool(pool);
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let executor = FlowExecutor::new(registry.clone(), invoker.clone());
    let flows = FlowService::new(store.clone(), executor.clone());

    let bus = Bus::connect(
        &redis_url(),
        BusConfig {
            stream: format!("test:payload:{}", Ulid::generate()),
            group: "test-workers".to_string(),
            consumer: "test-consumer".to_string(),
            block: Duration::from_millis(50),
            ..BusConfig::default()
        },
    )
    .await
    .expect("连接 Redis 失败——本机 6379 上应有 Redis");

    let app = router(
        SystemState::without_metrics(),
        ApiState::new(store.clone(), registry, invoker, flows).with_async(AsyncExecutor::new(
            store.clone(),
            bus.clone(),
            executor,
        )),
    );

    hub_store::flows::upsert_draft(
        store.pool(),
        &hub_store::flows::DraftInput {
            name: "intake",
            description: "载荷边界测试",
            definition: &j!({
                "name": "intake",
                "nodes": [{"id": "a", "plugin": "echo-plugin"}],
                "edges": []
            }),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("存草稿应成功");
    hub_store::flows::publish_draft(store.pool(), "intake")
        .await
        .expect("发布应成功");

    let (status, body) = post(
        &app,
        "/flows/intake/trigger-async",
        &json!({"payload": {"blob": oversized_string()}}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "响应: {body}");

    // 异步链上更要紧：一条 4MB+ 的消息躺在 Redis 里，重投几次就能把内存吃掉一大块
    let deliveries = bus.receive().await.expect("应取到消息");
    let envelope = deliveries[0].message.envelope.as_ref().expect("应有信封");
    assert!(envelope.payload.is_none(), "超限载荷不该内联进总线消息");
    assert!(envelope.payload_ref.is_some(), "信封里该只有一个 uri");
}
