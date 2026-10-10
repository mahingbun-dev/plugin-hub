//! 管理面鉴权的端到端。
//!
//! 走**真实的 gRPC auth 插件**（`hub-testkit` 的 auth 模式）而不是打桩：这一层
//! 的判据是「中台怎么用插件给的那几个权限位」，打桩会把被测的那一段换成桩的行为。
//!
//! 最要紧的两条：
//! - **插件不可达时必须 503，不能降级成匿名放行**——降级意味着一次 auth 插件抖动
//!   会让整个管理面变成无守卫，那正好是这一层想避免的事
//! - **发布与改草稿是两位**：只有 edit 的人不该发得出去

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_api::authz::{
    AuthzConfig, SCOPE_ADMIN, SCOPE_EDIT, SCOPE_INVOKE, SCOPE_PUBLISH, SCOPE_READ,
};
use hub_api::{ApiState, SystemState, router};
use hub_engine::{FlowExecutor, FlowService, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_store::flows::{self, DraftInput};
use hub_testkit as kit;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

/// auth 夹具认得的凭证。cookie 里出现这个子串就给出对应的权限位。
const COOKIE_ADMIN: &str = "ck-admin";
const COOKIE_EDITOR: &str = "ck-editor";

fn auth_fixture_behavior() -> kit::Behavior {
    kit::Behavior {
        auth_scopes: Some(vec![
            (
                COOKIE_ADMIN.to_string(),
                vec![
                    SCOPE_READ.to_string(),
                    // `invoke` 曾经漏在这里。真插件的管理员拿的是全部五个位
                    // （见 examples/auth-plugin 的 scopesFor），少这一位会让「管理员
                    // 反而触发不了编排、调不了 MCP」——而那条路当时没有用例覆盖，
                    // 所以一直没暴露。补上它，夹具才与真插件同形。
                    SCOPE_INVOKE.to_string(),
                    SCOPE_EDIT.to_string(),
                    SCOPE_PUBLISH.to_string(),
                    SCOPE_ADMIN.to_string(),
                ],
            ),
            // 只有编辑权：能改草稿，但发不出去
            (
                COOKIE_EDITOR.to_string(),
                vec![SCOPE_READ.to_string(), SCOPE_EDIT.to_string()],
            ),
        ]),
        ..kit::Behavior::named("auth", "1.0.0")
    }
}

struct Harness {
    app: Router,
    registry: Registry,
}

fn build(store: Store, authz: Option<AuthzConfig>) -> Harness {
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

    let mut state = ApiState::new(store, registry.clone(), invoker, flows);
    if let Some(config) = authz {
        state = state.with_authz(config);
    }

    Harness {
        app: router(SystemState::without_metrics(), state),
        registry,
    }
}

/// 起 auth 夹具并注册。
async fn with_auth_plugin(h: &Harness) -> kit::Fixture {
    let fixture = kit::start(auth_fixture_behavior()).await;
    let response = h.registry.register(&fixture.register_request(), None).await;
    assert!(
        response.accepted,
        "auth 插件应能注册：{:?}",
        response.rejections
    );
    fixture
}

fn authz_config() -> AuthzConfig {
    AuthzConfig {
        plugin: "auth".to_string(),
        version: None,
        // 集成测试里的闸门形态按用例需要开：默认关 = 与既有用例行为一致
        mcp_login_gate: false,
    }
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(value).expect("序列化失败")))
            .expect("构造请求失败"),
        None => builder.body(Body::empty()).expect("构造请求失败"),
    };

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

async fn seed_flow(store: &Store, name: &str) {
    flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name,
            description: "鉴权用例",
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

// ---------------------------------------------------------------- 没配插件时

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 没配_auth_插件时管理面照常放行(pool: PgPool) {
    // 这是过渡期形态：auth 插件还没部署时，管理面维持原样。
    // 断言它，是为了让「启用鉴权」这件事必须是一个显式的动作
    let h = build(Store::from_pool(pool), None);

    let (status, _) = send(&h.app, "GET", "/admin/plugins", None, None).await;
    assert_eq!(status, StatusCode::OK, "没配 auth 插件时不该有守卫");
}

// ---------------------------------------------------------------- 401

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 没带凭证返回_401(pool: PgPool) {
    let h = build(Store::from_pool(pool), Some(authz_config()));
    let _fixture = with_auth_plugin(&h).await;

    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/plugins")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("请求处理失败");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // 明确告诉调用方去哪个认证入口——没有它，前端只能猜是跳登录还是刷新凭证
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Cookie")
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 凭证不被认返回_401(pool: PgPool) {
    let h = build(Store::from_pool(pool), Some(authz_config()));
    let _fixture = with_auth_plugin(&h).await;

    let (status, body) = send(
        &h.app,
        "GET",
        "/admin/plugins",
        Some("SESSION=somebody-else"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "响应: {body}");
    assert_eq!(body["error"], "unauthorized");
}

// ---------------------------------------------------------------- 403

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 缺少权限位返回_403_并说清缺哪个(pool: PgPool) {
    let store = Store::from_pool(pool);
    let h = build(store, Some(authz_config()));
    let _fixture = with_auth_plugin(&h).await;

    // 编辑者能被认出来，但没有 admin 位——管不了治理与插件目录
    let (status, body) = send(
        &h.app,
        "GET",
        "/admin/governance",
        Some(COOKIE_EDITOR),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "响应: {body}");
    assert_eq!(body["error"], "forbidden");
    assert!(
        body["message"].as_str().unwrap_or("").contains(SCOPE_ADMIN),
        "要说清缺的是哪一个位、当前有哪些，否则调用方只能去猜：{body}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 只有编辑权的人发不出去(pool: PgPool) {
    // 把发布与改草稿并成一个位，等于让所有编辑者都能发布。
    // 这条用例守的就是那个区分
    let store = Store::from_pool(pool);
    seed_flow(&store, "intake").await;
    let h = build(store, Some(authz_config()));
    let _fixture = with_auth_plugin(&h).await;

    // 改草稿可以
    let (status, body) = send(
        &h.app,
        "POST",
        "/flows/intake/draft",
        Some(COOKIE_EDITOR),
        Some(&json!({
            "description": "编辑者改的",
            "definition": {"name": "intake", "nodes": [{"id": "a", "plugin": "whatever"}], "edges": []}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "编辑者应当能改草稿：{body}");

    // 发布不行——他没有 hub:publish
    let (status, body) = send(
        &h.app,
        "POST",
        "/flows/intake/publish",
        Some(COOKIE_EDITOR),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "响应: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains(SCOPE_PUBLISH),
        "要说清缺的是发布位：{body}"
    );
}

// ---------------------------------------------------------------- 放行

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 权限够就放行(pool: PgPool) {
    let store = Store::from_pool(pool);
    seed_flow(&store, "intake").await;
    let h = build(store, Some(authz_config()));
    let _fixture = with_auth_plugin(&h).await;

    // 顺手注册一个 `whatever`：发布要过编排校验，而校验会查插件在不在。
    // 不注册的话发布会被业务校验拦下（400）——那与「鉴权放没放行」是两件事，
    // 混在一起会让这条用例看起来在验鉴权、实际上验的是编排校验
    let worker = kit::start(kit::Behavior::named("whatever", "1.0.0")).await;
    let response = h.registry.register(&worker.register_request(), None).await;
    assert!(response.accepted, "插件应能注册：{:?}", response.rejections);

    // 管理面
    let (status, body) = send(&h.app, "GET", "/admin/governance", Some(COOKIE_ADMIN), None).await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    // 读写编排
    let (status, _) = send(&h.app, "GET", "/flows", Some(COOKIE_ADMIN), None).await;
    assert_eq!(status, StatusCode::OK);

    // 发布
    let (status, body) = send(
        &h.app,
        "POST",
        "/flows/intake/publish",
        Some(COOKIE_ADMIN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    drop(worker);
}

// ---------------------------------------------------------------- 业务入口

/// **业务入口（`/ingress`）要 `hub:invoke`**。
///
/// 这条改动的由来：只要有插件要求「每次调用可归因到具体的人」（SQL 执行器是第一个），
/// 原设计「ingress 不鉴权、由插件自己承担」就撑不住——插件自己认凭证意味着每个插件
/// 都要重复实现一遍鉴权，还拿不到中台已经算好的权限位。
///
/// 这里验的是**三层**：无凭证拦在中间件、有权限放行到下一层、权限不够说清缺哪一位。
/// 只断言「无凭证 401」是不够的——那只证明了中间件在跑，证明不了它没把合法请求也拦掉。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 业务入口要鉴权(pool: PgPool) {
    let h = build(Store::from_pool(pool), Some(authz_config()));
    let _fixture = with_auth_plugin(&h).await;
    let body = json!({ "payload": { "a": 1 } });

    let (status, _) = send(&h.app, "POST", "/ingress/whatever", None, Some(&body)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "业务入口没带凭证应 401");

    // 管理员有全部五个位，应当放行——随后因为 "whatever" 没注册而 404。
    // 断言 404 而不是「不是 401/403」，是为了确保鉴权这一段真的过去了。
    let (status, body) = send(
        &h.app,
        "POST",
        "/ingress/whatever",
        Some(COOKIE_ADMIN),
        Some(&body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "鉴权通过后应前进到「插件未注册」：{body}"
    );
    assert_eq!(body["error"], "not_found");

    // 只有编辑权的人：能改草稿，但不该调得动插件
    let (status, body) = send(
        &h.app,
        "POST",
        "/ingress/whatever",
        Some(COOKIE_EDITOR),
        Some(&body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "只有 edit 的人不该调得动插件：{body}"
    );
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains(SCOPE_INVOKE)),
        "403 要说清缺哪个位，实际：{body}"
    );
}

// ---------------------------------------------------------------- 身份透传

/// **已认证的身份要真的落进信封**。
///
/// `envelope.proto` 说「subject 由鉴权插件填充，中台只负责透传、审计与限流」——
/// 这条用例验的就是那个透传：身份源于中间件已经验过的凭证，随信封抵达插件。
///
/// 没有它，插件只能靠载荷里自报的字段猜「谁在调我」，而自报的字段谁都能写。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 业务入口把已认证的身份送进信封(pool: PgPool) {
    let h = build(Store::from_pool(pool), Some(authz_config()));
    let _auth = with_auth_plugin(&h).await;

    // 另起一个回显插件当被调方。auth 插件自己回答的是「这是谁」，
    // 而不是「收到信封的人看到了什么」——两者不能是同一个。
    let echo = kit::start_default().await;
    let response = h.registry.register(&echo.register_request(), None).await;
    assert!(
        response.accepted,
        "回显插件应能注册：{:?}",
        response.rejections
    );

    let (status, body) = send(
        &h.app,
        "POST",
        "/ingress/echo-plugin",
        Some(COOKIE_ADMIN),
        Some(&json!({ "payload": { "a": 1 } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    let seen = echo.last_envelope().expect("插件应收到信封");
    let subject = seen.subject.expect("信封里应带上调用主体");

    // auth 夹具对认得出来的凭证固定回 "u-1"
    assert_eq!(
        subject.id, "u-1",
        "subject.id 应来自鉴权插件给出的 userCode"
    );
    assert_eq!(
        subject.kind,
        hub_proto::v1::SubjectKind::Human as i32,
        "凭证是人的登录态，发起主体就是人"
    );
    assert!(
        subject.scopes.iter().any(|s| s == SCOPE_INVOKE),
        "权限位要一并带上——插件靠它做业务级鉴权；实际 {:?}",
        subject.scopes
    );
}

// ---------------------------------------------------------------- 不受影响的路径

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 探活与指标不受鉴权影响(pool: PgPool) {
    // 给它们加鉴权会让「中台是不是活着」变成需要凭证才能回答的问题；
    // 而 nginx / Prometheus 要能直接探。
    //
    // 注意这里**不含 /ingress**：它现在要 `hub:invoke`（见 src/authz.rs 里的说明）。
    // 这条用例曾经叫「探活与业务入口不受鉴权影响」却从没测过 ingress，名字会误导人。
    let h = build(Store::from_pool(pool), Some(authz_config()));

    let (status, _) = send(&h.app, "GET", "/health", None, None).await;
    assert_eq!(status, StatusCode::OK, "探活不该要凭证");

    let (status, _) = send(&h.app, "GET", "/metrics", None, None).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
        "指标不该要凭证（测试里没装 recorder，所以可能是 503），实际 {status}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 路径写错了是_404_不是_403(pool: PgPool) {
    // 认不出来的路径放行、交给 axum 的 404。在这里凭空发明一个权限位，
    // 会让「路径写错了」表现为 403，把真正的问题藏起来
    let h = build(Store::from_pool(pool), Some(authz_config()));

    let (status, _) = send(
        &h.app,
        "GET",
        "/never-heard-of-it",
        Some(COOKIE_ADMIN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- 基础设施故障

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn auth_插件不可达时返回_503_而不是降级(pool: PgPool) {
    // **这条是这一层最要紧的性质**：降级成匿名放行意味着一次 auth 插件抖动
    // 会让整个管理面变成无守卫，那正好是这一层想避免的事。
    // 这里刻意**不注册** auth 插件——调用会因「没有可用实例」而失败
    let h = build(Store::from_pool(pool), Some(authz_config()));

    let (status, body) = send(&h.app, "GET", "/admin/plugins", Some(COOKIE_ADMIN), None).await;

    assert_ne!(
        status,
        StatusCode::OK,
        "插件不可达时绝不能放行——那等于管理面无守卫：{body}"
    );
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "应当明确说是鉴权服务不可用（可重试），而不是 401（凭证有问题）或 403（权限不够）：{body}"
    );
    assert_eq!(body["error"], "auth_unavailable");
}

// ---------------------------------------------------------------- Bearer 直传

/// 带 `Authorization: Bearer` 的请求。凭证匹配规则与 cookie 相同（子串）。
async fn send_bearer(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, Value, std::collections::HashMap<String, String>) {
    use axum::http::header;

    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder.body(Body::empty()).expect("构造请求失败");

    let response = app.clone().oneshot(request).await.expect("请求处理失败");
    let status = response.status();
    let headers: std::collections::HashMap<String, String> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();
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

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn bearer_token直传_有效token认证成功(pool: PgPool) {
    // 嵌入场景的新通道：调用方显式带平台登录态 token（MCP 客户端 headers 配置、
    // 服务端转发），不必非得有浏览器 Cookie
    let h = build(Store::from_pool(pool), Some(authz_config()));
    let _fixture = with_auth_plugin(&h).await;

    let (status, _, _headers) =
        send_bearer(&h.app, "GET", "/admin/plugins", Some(COOKIE_ADMIN)).await;
    assert_eq!(status, StatusCode::OK, "有效的 Bearer token 应认证成功");

    // 无效 token：401 且 WWW-Authenticate 指到 Bearer——调用方才知道该换哪条通道
    let (status, body, headers) =
        send_bearer(&h.app, "GET", "/admin/plugins", Some("expired-token")).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "无效 token 应 401：{body}"
    );
    assert_eq!(body["error"], "unauthorized");
    assert_eq!(
        headers.get("www-authenticate").map(String::as_str),
        Some("Bearer"),
        "401 必须指明认证入口是 Bearer，否则调用方分不清该换 Cookie 还是换 token"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn cookie与bearer同现时bearer优先(pool: PgPool) {
    let h = build(Store::from_pool(pool), Some(authz_config()));
    let _fixture = with_auth_plugin(&h).await;

    // cookie 是编辑者、bearer 是管理员：听 bearer（显式传 token 意图明确，
    // 是嵌入场景的新通道；cookie 是存量通道，两者同现听新的）
    let request = Request::builder()
        .method("GET")
        .uri("/admin/plugins")
        .header("cookie", format!("SESSION={COOKIE_EDITOR}"))
        .header("authorization", format!("Bearer {COOKIE_ADMIN}"))
        .body(Body::empty())
        .expect("构造请求失败");
    let response = h.app.clone().oneshot(request).await.expect("请求处理失败");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Bearer 与 Cookie 同现时应听 Bearer（管理员通过）"
    );
}
