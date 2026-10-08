//! 触发器 HTTP 面。
//!
//! 与仓储层的用例（`hub-store/tests/triggers.rs`）分开：那边验的是校验规则与查询本身，
//! 这里验的是**接口层**——状态码对不对、错误体有没有把原因说清楚、启停与删除的
//! 「不存在」返回的是不是 404。这两层出问题的方式不一样，混在一条用例里会看不出
//! 是哪一层坏了。

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
use hub_store::flows::{self, DraftInput};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

fn harness(store: Store) -> Router {
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
    router(
        SystemState::without_metrics(),
        ApiState::new(store, registry, invoker, flows),
    )
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

async fn delete(app: &Router, uri: &str) -> (StatusCode, Value) {
    send(
        app,
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(Body::empty())
            .expect("构造请求失败"),
    )
    .await
}

/// 建一条 flow，触发器要挂在它上面
async fn seed_flow(store: &Store, name: &str) {
    flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name,
            description: "触发器用例",
            definition: &json!({
                "name": name,
                "nodes": [{"id": "a", "plugin": "whatever"}],
                "edges": []
            }),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("存草稿应成功");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 登记一条_cron_触发器并在列表里看到它(pool: PgPool) {
    let store = Store::from_pool(pool);
    seed_flow(&store, "intake").await;
    let app = harness(store);

    let (status, body) = post(
        &app,
        "/flows/intake/triggers",
        &json!({"kind": "cron", "name": "nightly", "config": {"expr": "0 0 2 * * *"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    let (status, body) = get(&app, "/triggers").await;
    assert_eq!(status, StatusCode::OK);
    let list = body.as_array().expect("应当是数组");
    assert_eq!(list.len(), 1, "响应: {body}");
    assert_eq!(list[0]["flow_name"], "intake", "列表要带上所属 flow 的名字");
    assert_eq!(list[0]["kind"], "cron");
    assert_eq!(list[0]["name"], "nightly");
    assert_eq!(list[0]["config"]["expr"], "0 0 2 * * *");
    assert_eq!(list[0]["enabled"], true, "新登记的触发器是启用的");
    assert_eq!(list[0]["fired_count"], 0);
    assert!(list[0]["last_fired_at"].is_null(), "还没触发过");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 缺必填配置返回_400_并点出缺哪个字段(pool: PgPool) {
    let store = Store::from_pool(pool);
    seed_flow(&store, "intake").await;
    let app = harness(store);

    // 仓储层的 `Invalid` 要映射成 400 而不是 500——
    // 「这次请求不成立」与服务故障是两件事，调用方据此决定改请求还是重试
    let (status, body) = post(
        &app,
        "/flows/intake/triggers",
        &json!({"kind": "cron", "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert_eq!(body["error"], "bad_request");
    assert!(
        body["message"].as_str().unwrap_or("").contains("expr"),
        "错误信息要点出缺的是哪个字段，实际：{body}"
    );

    let (status, _) = post(
        &app,
        "/flows/intake/triggers",
        &json!({"kind": "mq", "config": {"stream": "  "}}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "空白串与缺字段是同一件事——调度器读到的都是空"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 未定义的触发器类型被拒(pool: PgPool) {
    let store = Store::from_pool(pool);
    seed_flow(&store, "intake").await;
    let app = harness(store);

    let (status, body) = post(
        &app,
        "/flows/intake/triggers",
        &json!({"kind": "webhook", "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"].as_str().unwrap_or("").contains("webhook"),
        "要把收到的类型说出来，实际：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 挂在不存在的_flow_上返回_400(pool: PgPool) {
    let store = Store::from_pool(pool);
    let app = harness(store);

    let (status, body) = post(
        &app,
        "/flows/never-existed/triggers",
        &json!({"kind": "cron", "config": {"expr": "0 0 2 * * *"}}),
    )
    .await;
    // 是 400 而不是 404：请求本身没毛病，是它引用的东西不对。
    // 仓储层用 `INSERT ... SELECT FROM flows` 让「flow 不存在」体现为「一行没插」，
    // 再在这里报一句人话，而不是把外键约束错误抛出去
    assert_eq!(status, StatusCode::BAD_REQUEST, "响应: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("never-existed"),
        "要把 flow 名说出来，实际：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 启停与删除(pool: PgPool) {
    let store = Store::from_pool(pool);
    seed_flow(&store, "intake").await;
    let app = harness(store);

    let (_, created) = post(
        &app,
        "/flows/intake/triggers",
        &json!({"kind": "cron", "name": "nightly", "config": {"expr": "0 0 2 * * *"}}),
    )
    .await;
    let id = created["id"].as_i64().expect("应当有 id");

    // 停用
    let (status, body) = post(
        &app,
        &format!("/triggers/{id}/enabled"),
        &json!({"enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["enabled"], false,
        "要回显生效后的值——这是个可重复点击的开关"
    );

    let (_, list) = get(&app, "/triggers").await;
    assert_eq!(list[0]["enabled"], false, "列表要反映出来");

    // 重新登记会把它置回启用：登记一份配置的意图就是「让它按这个跑」
    let (status, _) = post(
        &app,
        "/flows/intake/triggers",
        &json!({"kind": "cron", "name": "nightly", "config": {"expr": "0 0 3 * * *"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = get(&app, "/triggers").await;
    let list = list.as_array().expect("应当是数组");
    assert_eq!(list.len(), 1, "同名是改不是新增");
    assert_eq!(list[0]["enabled"], true);
    assert_eq!(list[0]["config"]["expr"], "0 0 3 * * *");

    // 删除
    let (status, _) = delete(&app, &format!("/triggers/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = get(&app, "/triggers").await;
    assert!(list.as_array().expect("数组").is_empty());
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 对不存在的触发器操作返回_404(pool: PgPool) {
    let store = Store::from_pool(pool);
    let app = harness(store);

    let (status, body) = post(&app, "/triggers/999999/enabled", &json!({"enabled": true})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "响应: {body}");
    assert_eq!(body["error"], "not_found");

    let (status, _) = delete(&app, "/triggers/999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 列表带上已停用的并排在前面(pool: PgPool) {
    let store = Store::from_pool(pool);
    seed_flow(&store, "intake").await;
    seed_flow(&store, "shipping").await;
    let app = harness(store);

    let (_, a) = post(
        &app,
        "/flows/intake/triggers",
        &json!({"kind": "cron", "name": "nightly", "config": {"expr": "0 0 2 * * *"}}),
    )
    .await;
    post(
        &app,
        "/flows/shipping/triggers",
        &json!({"kind": "cron", "name": "hourly", "config": {"expr": "0 0 * * * *"}}),
    )
    .await;
    post(
        &app,
        &format!("/triggers/{}/enabled", a["id"].as_i64().unwrap()),
        &json!({"enabled": false}),
    )
    .await;

    let (_, list) = get(&app, "/triggers").await;
    let list = list.as_array().expect("数组");
    assert_eq!(list.len(), 2, "停用的那条也要在列表里");
    assert_eq!(
        list[0]["name"], "nightly",
        "停用的排最前——排障时问「它为什么没跑」，第一个答案往往是「它被关了」"
    );
    assert_eq!(list[0]["enabled"], false);
}
