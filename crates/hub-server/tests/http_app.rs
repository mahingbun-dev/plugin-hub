//! HTTP 面的组装：路由与中间件的**挂载顺序**。
//!
//! 这一层读代码看不出对错——`Router::layer` 只作用于调用它时**已经存在**的路由，
//! 之后 `merge` 进来的不受影响（axum 文档原话："Additional routes added after
//! `layer` is called will not have the middleware added"）。
//!
//! 顺序错了的表现是「某个面悄悄没有鉴权」：不报错、不打日志、单测全绿，
//! 只有真的发一个请求才验得出来。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_api::authz::AuthzConfig;
use hub_api::{ApiState, SystemState};
use hub_engine::{FlowExecutor, FlowService, Invoker};
use hub_mcp::HubMcp;
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_server::http_app;
use hub_store::Store;
use hub_testkit as kit;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

/// 组装一个真实的中台 HTTP 面（含 MCP 面），鉴权指向 `auth` 插件。
///
/// 返回 registry 一并交出来，需要注册插件的用例才能用——「鉴权闸门在不在」
/// 那类用例不需要插件（中间件在没带凭证时直接 401，不会去问插件）。
fn build(pool: PgPool) -> (Router, Registry) {
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
    );

    let mcp =
        HubMcp::new(store.clone(), invoker.clone(), flows.clone()).router(CancellationToken::new());
    let api = ApiState::new(store, registry.clone(), invoker, flows).with_authz(AuthzConfig {
        plugin: "auth".to_string(),
        version: None,
    });

    (http_app(SystemState::without_metrics(), api, mcp), registry)
}

/// 发一个请求，只关心状态码。
///
/// `POST` 走 MCP 的 `initialize`；`GET` 不带体（`/health` 是 GET 路由，
/// 用 POST 打它会得到 405，那是另一回事，不该混进鉴权的判据里）。
async fn call(app: &Router, method: &str, uri: &str, cookie: Option<&str>) -> StatusCode {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        // MCP 的 Streamable HTTP 服务端要求 Host 头；oneshot 没有真实连接
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }

    let body = if method == "GET" {
        Body::empty()
    } else {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1.0" }
            }
        });
        Body::from(serde_json::to_vec(&payload).expect("序列化失败"))
    };

    let request = builder.body(body).expect("构造请求失败");

    let response = app.clone().oneshot(request).await.expect("请求失败");
    let status = response.status();
    // 读干净响应体，免得连接被半读状态影响后续断言
    let _ = response.into_body().collect().await;
    status
}

/// **MCP 面必须过鉴权**。
///
/// 它曾经不过：`hub-server` 把 MCP 面 `merge` 进来，而鉴权挂在内层 `api_router` 上，
/// 于是 `required_scope` 里那条 `/mcp → hub:invoke` 是死代码——写在那儿、看着对、
/// 实际不生效。三条断言一起才说明问题：
///
/// - `/mcp` 没凭证 → 401（这道闸在不在）
/// - `/admin/plugins` 没凭证 → 401（对照组：已知有闸的路径，证明中间件确实在工作）
/// - `/health` 没凭证 → 200（对照组：证明不是一刀切把所有请求都拦了）
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn mcp_面也要鉴权(pool: PgPool) {
    let (app, _registry) = build(pool);

    assert_eq!(
        call(&app, "POST", "/mcp", None).await,
        StatusCode::UNAUTHORIZED,
        "MCP 面没带凭证应 401——它和 /admin 一样是要 hub:invoke 的管理面路径"
    );

    assert_eq!(
        call(&app, "GET", "/admin/plugins", None).await,
        StatusCode::UNAUTHORIZED,
        "对照组：管理面本来就该 401"
    );

    assert_eq!(
        call(&app, "GET", "/health", None).await,
        StatusCode::OK,
        "对照组：探活不该要凭证，否则「中台是不是活着」变成需要凭证才能回答的问题"
    );
}

// ---------------------------------------------------------------- 身份链路

/// 发一个 MCP 请求，返回 `(会话 id, 响应 JSON)`。
async fn mcp_call(
    app: &Router,
    session: Option<&str>,
    cookie: Option<&str>,
    body: serde_json::Value,
) -> (Option<String>, serde_json::Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(session) = session {
        builder = builder.header("mcp-session-id", session);
    }
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }

    let request = builder
        .body(Body::from(serde_json::to_vec(&body).expect("序列化失败")))
        .expect("构造请求失败");

    let response = app.clone().oneshot(request).await.expect("请求失败");
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应失败")
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();

    // 响应可能是 SSE（data: 行）或裸 JSON，两种都接受
    let payload = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .find(|chunk| chunk.starts_with('{'))
        .map(str::to_string)
        .unwrap_or_else(|| text.clone());

    (
        session_id,
        serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
    )
}

/// **完整身份链路**：凭证 → 鉴权插件 → 中间件 → MCP 工具 → 插件收到的信封。
///
/// 这一段必须端到端验。拆开看每一段都对——`/mcp` 有闸门、有身份时会装信封——
/// 但接起来仍可能断，比如 rmcp 根本没把 HTTP 的 `Parts` 递给工具处理函数。
/// 那种断法不报错、不打日志，只是插件收到的 `subject` 永远是空的：
/// 「这次调用是谁发起的」就此消失，审计里只剩下「有人做了这件事」。
///
/// 断言刻意落在**插件收到的信封**上，而不是中台的返回值上——中台完全可能
/// 自己算对了身份却没往信封里放，那正是这条用例要防的。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 身份从凭证一路传到插件(pool: PgPool) {
    let (app, registry) = build(pool);

    // 鉴权插件：认得这个 cookie，并给出调 MCP 需要的 hub:invoke
    let auth = kit::start(kit::Behavior {
        auth_scopes: Some(vec![(
            "ck-admin".to_string(),
            vec!["hub:invoke".to_string()],
        )]),
        ..kit::Behavior::named("auth", "1.0.0")
    })
    .await;
    assert!(
        registry
            .register(&auth.register_request(), None)
            .await
            .accepted,
        "鉴权插件应能注册"
    );

    // 被调用的插件：回显，供检查它到底收到了什么
    let echo = kit::start_default().await;
    assert!(
        registry
            .register(&echo.register_request(), None)
            .await
            .accepted,
        "回显插件应能注册"
    );

    // 开一个 MCP 会话。**必须带凭证**——闸门就在这一层
    let (session, value) = mcp_call(
        &app,
        None,
        Some("ck-admin"),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let session = session.unwrap_or_else(|| panic!("initialize 应返回会话 id，实际：{value}"));

    // 经 MCP 调插件
    let (_, value) = mcp_call(
        &app,
        Some(&session),
        Some("ck-admin"),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "invoke_plugin",
                "arguments": { "plugin": "echo-plugin", "payload": { "a": 1 } }
            }
        }),
    )
    .await;

    let seen = echo.last_envelope().expect("插件应收到信封");
    let subject = seen
        .subject
        .unwrap_or_else(|| panic!("信封里应带上身份，MCP 响应：{value}"));

    // 鉴权夹具对认得出来的凭证固定回 "u-1"
    assert_eq!(subject.id, "u-1", "身份应来自鉴权插件给出的 userCode");
    assert!(
        subject.scopes.iter().any(|s| s == "hub:invoke"),
        "权限位要一并带上，实际 {:?}",
        subject.scopes
    );
}
