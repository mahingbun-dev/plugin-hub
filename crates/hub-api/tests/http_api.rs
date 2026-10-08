//! HTTP 面的端到端验证：用**真实插件**跑通 ingress 与 admin 两组路由。
//!
//! 探活与指标不依赖业务状态，已在 `http_face.rs` 覆盖；这里只测需要
//! `Store` / `Registry` / `Invoker` 的路径。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_api::{ApiState, SystemState, router};
use hub_engine::Invoker;
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
    let flows = hub_engine::FlowService::new(
        store.clone(),
        hub_engine::FlowExecutor::new(registry.clone(), invoker.clone()),
    );
    let app = router(
        SystemState::without_metrics(),
        ApiState::new(store, registry.clone(), invoker, flows),
    );
    Harness { app, registry }
}

async fn post(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).expect("序列化失败")))
        .expect("构造请求失败");
    send(app, request).await
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("构造请求失败");
    send(app, request).await
}

async fn delete(app: &Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .expect("构造请求失败");
    send(app, request).await
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

// ---------------------------------------------------------------- ingress

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 经_ingress_调用插件并回传结果(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let payload = json!({"orderId": "SO-1", "qty": 3, "urgent": true});
    let (status, body) = post(
        &h.app,
        "/ingress/echo-plugin",
        &json!({"payload": payload, "message_id": "msg-1"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["plugin"], "echo-plugin");
    assert_eq!(body["version"], "1.0.0");
    assert_eq!(body["message_id"], "msg-1", "调用方指定的 id 应被沿用");
    assert_eq!(
        body["payload"], payload,
        "载荷应经插件往返后原样回来（含整数字段不失真）"
    );

    assert_eq!(fixture.handled_count(), 1);
    let seen = fixture.last_envelope().expect("插件应收到信封");
    assert_eq!(seen.message_id, "msg-1");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 校验器拒绝时返回_422_且插件体不执行(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let (status, body) = post(
        &h.app,
        "/ingress/echo-plugin",
        &json!({"payload": {"a": 1}, "message_id": "bad-1"}),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"], "validation_rejected");
    assert_eq!(body["issues"][0]["path"], "message_id");
    assert_eq!(fixture.handled_count(), 0, "校验不通过时插件体绝不能被执行");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 载荷不是对象时返回_400(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let (status, body) = post(&h.app, "/ingress/echo-plugin", &json!({"payload": [1, 2]})).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert_eq!(body["error"], "bad_request");
    assert_eq!(fixture.handled_count(), 0);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 调用未注册插件返回_404(pool: PgPool) {
    let h = harness(Store::from_pool(pool));

    let (status, body) = post(&h.app, "/ingress/从未注册过", &json!({"payload": {"a": 1}})).await;

    assert_eq!(status, StatusCode::NOT_FOUND, "响应: {body}");
    assert_eq!(body["error"], "not_found");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件停掉后调用返回_502(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    fixture.stop().await;

    let (status, body) = post(
        &h.app,
        "/ingress/echo-plugin",
        &json!({"payload": {"a": 1}}),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "插件不可达是下游问题，调用方可重试，不该报 500：{body}"
    );
    assert_eq!(body["error"], "plugin_unavailable");
    assert_eq!(body["plugin"], "echo-plugin");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 超时上限被拒并引导到异步(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let (status, body) = post(
        &h.app,
        "/ingress/echo-plugin",
        &json!({"payload": {"a": 1}, "timeout_ms": 999_999}),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert!(body["message"].as_str().unwrap_or("").contains("异步"));
}

// ---------------------------------------------------------------- admin

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn admin_列出插件与实例(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let (status, plugins) = get(&h.app, "/admin/plugins").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(plugins.as_array().map(Vec::len), Some(1));
    assert_eq!(plugins[0]["name"], "echo-plugin");
    assert_eq!(plugins[0]["version_count"], 1);
    assert_eq!(plugins[0]["instance_count"], 1);

    let (status, instances) = get(&h.app, "/admin/instances").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(instances.as_array().map(Vec::len), Some(1));
    assert_eq!(instances[0]["instance_id"], "instance-echo-plugin");
    assert_eq!(instances[0]["advertise_addr"], fixture.base_url());
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn admin_插件详情含契约与工具(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let (status, detail) = get(&h.app, "/admin/plugins/echo-plugin").await;
    assert_eq!(status, StatusCode::OK, "响应: {detail}");

    assert_eq!(detail["name"], "echo-plugin");
    assert_eq!(detail["owner"], "testkit");

    let versions = detail["versions"].as_array().expect("应有 versions");
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["version"], "1.0.0");
    assert_eq!(
        versions[0]["contracts"][0]["fq_name"],
        kit::MESSAGE_FQ,
        "契约应随版本一起出，控制台靠它展示"
    );
    assert_eq!(versions[0]["instances"].as_array().map(Vec::len), Some(1));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn admin_未注册插件详情返回_404(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let (status, _) = get(&h.app, "/admin/plugins/不存在").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn admin_注册拒绝留痕可查且带拒绝码名(pool: PgPool) {
    let h = harness(Store::from_pool(pool.clone()));

    // 链路前半段（注册被拒 → 落库）由 hub-registry 的集成测试覆盖；
    // 这里管 API 面：查得出来、形状对（code_name 是给人看的名字）
    hub_store::rejections::record(
        &pool,
        &hub_store::rejections::NewRejection {
            plugin_name: "sql-executor",
            instance_id: "sql-executor-1",
            code: 5,
            version: "0.2.0",
            message: "版本 0.2.0 已存在且契约或 manifest 不一致",
            detail: "同一版本号不可改变契约；请升版本号后重新注册",
            source_ip: "127.0.0.1",
        },
    )
    .await
    .expect("造拒绝记录失败");

    let (status, rows) = get(&h.app, "/admin/rejections").await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows.as_array().expect("应为数组");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["plugin_name"], "sql-executor");
    assert_eq!(rows[0]["code_name"], "VERSION_CONFLICT");
    assert_eq!(rows[0]["count"], 1);
    assert_eq!(rows[0]["instance_id"], "sql-executor-1");

    // 按插件过滤
    let (status, filtered) = get(&h.app, "/admin/rejections?plugin=sql-executor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(filtered.as_array().map(Vec::len), Some(1));
    let (status, none) = get(&h.app, "/admin/rejections?plugin=别的插件").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(none.as_array().map(Vec::len), Some(0));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn admin_删除已登记版本并级联清实例(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool.clone()));
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    // 删除前先留一条拒绝记录：删版本是「彻底重来」，旧留痕应一并销案
    hub_store::rejections::record(
        &pool,
        &hub_store::rejections::NewRejection {
            plugin_name: "echo-plugin",
            instance_id: "instance-echo-plugin",
            code: 5,
            version: "1.0.0",
            message: "版本 1.0.0 已存在且契约或 manifest 不一致",
            detail: "同一版本号不可改变契约；请升版本号后重新注册",
            source_ip: "127.0.0.1",
        },
    )
    .await
    .expect("造拒绝记录失败");

    let (status, body) = delete(&h.app, "/admin/plugins/echo-plugin/versions/1.0.0").await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["deleted"], true);
    assert_eq!(body["plugin"], "echo-plugin");
    assert_eq!(body["cleared_rejections"], 1);

    // 级联效果：版本没了、实例跟着没了，工具面立即下线
    let (_, detail) = get(&h.app, "/admin/plugins/echo-plugin").await;
    assert_eq!(detail["versions"].as_array().map(Vec::len), Some(0));

    let (_, rejections) = get(&h.app, "/admin/rejections").await;
    assert_eq!(rejections.as_array().map(Vec::len), Some(0), "删除即销案");

    // 修好后的插件可以重新登记同一版本号
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted,
        "删掉旧登记后应能重新注册"
    );

    // 删不存在的版本 → 404
    let (status, body) = delete(&h.app, "/admin/plugins/echo-plugin/versions/9.9.9").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["message"].as_str().unwrap_or("").contains("9.9.9"));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn admin_可按消息类型反查影响面(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(
        h.registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let (status, usage) = get(&h.app, &format!("/admin/messages/{}", kit::MESSAGE_FQ)).await;
    assert_eq!(status, StatusCode::OK, "响应: {usage}");

    assert_eq!(usage["fq_name"], kit::MESSAGE_FQ);
    assert_eq!(usage["producers"].as_array().map(Vec::len), Some(1));
    assert_eq!(usage["producers"][0]["plugin"], "echo-plugin");
    assert_eq!(usage["producers"][0]["version"], "1.0.0");
    // 夹具同时消费同一个类型（链路上的节点就是这样），所以两侧都该有它
    assert_eq!(
        usage["consumers"].as_array().map(Vec::len),
        Some(1),
        "消费方也应能查到：{usage}"
    );
    assert_eq!(usage["consumers"][0]["plugin"], "echo-plugin");
}
