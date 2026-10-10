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
    build_with_gate(pool, false)
}

/// 闸门形态可配的组装：`true` 时 /mcp 的无凭证请求会放行到工具面
/// （由 MCP 登录闸门接管），`false` 时维持既有行为（无凭证 401）。
fn build_with_gate(pool: PgPool, gate: bool) -> (Router, Registry) {
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

    // 闸门开关必须**两处同源**：HTTP 层（AuthzConfig，决定无凭证放不放行）
    // 与工具面（HubMcp，决定放行之后谁来接管）。只开一边的断法是：
    // HTTP 放行了、工具面却匿名直调——恰好是这两条用例要抓的断点。
    let mcp = HubMcp::new(store.clone(), invoker.clone(), flows.clone())
        .with_login_gate(gate)
        .router(CancellationToken::new());
    let api = ApiState::new(store, registry.clone(), invoker, flows).with_authz(AuthzConfig {
        plugin: "auth".to_string(),
        version: None,
        mcp_login_gate: gate,
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
    mcp_call_with_bearer(app, session, cookie, None, body).await
}

/// 带 `Authorization: Bearer` 的 MCP 调用（嵌入场景：客户端 headers 配 token）。
async fn mcp_call_with_bearer(
    app: &Router,
    session: Option<&str>,
    cookie: Option<&str>,
    bearer: Option<&str>,
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
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
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

/// **Bearer 直传的端到端**：token → 鉴权插件 → 中间件 → MCP 工具 → 插件信封。
///
/// 与上面那条 Cookie 链路对称，验的是嵌入场景的新通道。两个断言都是「插件
/// 收到了什么」：subject 来自 Bearer 验出的身份；meta 里的 `hub.mas_token`
/// 是**这次请求的 token**——即便 login 缓存里有另一个身份的 token 也不能顶替
/// （进程级单槽缓存，请求凭证才是「这次调用是谁」的答案）。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn bearer直传的身份与登录态一路传到插件(pool: PgPool) {
    let (app, registry) = build_with_gate(pool, true);

    // 鉴权插件认得这个 token（匹配规则与 cookie 一致：子串）
    let auth = kit::start(kit::Behavior {
        auth_scopes: Some(vec![(
            "tk-admin".to_string(),
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

    let echo = kit::start_default().await;
    assert!(
        registry
            .register(&echo.register_request(), None)
            .await
            .accepted,
        "回显插件应能注册"
    );

    // Bearer 握手 MCP 会话（无 Cookie）。**闸门开着**：请求凭证优先路径——
    // 中间件验过 Bearer、身份进请求扩展，login_gate_subject 应直接用它，
    // 不弹窗、不落拒卡（「请求凭证 > login 缓存」的顺序另有单测钉着，
    // 端到端这里验的是整条通道可达）。
    let (session, value) = mcp_call_with_bearer(
        &app,
        None,
        None,
        Some("tk-admin"),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "embed-client", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let session = session.unwrap_or_else(|| panic!("Bearer 应能建立会话，实际：{value}"));

    let (_, value) = mcp_call_with_bearer(
        &app,
        Some(&session),
        None,
        Some("tk-admin"),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
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

    assert_eq!(subject.id, "u-1", "身份应来自 Bearer 验出的 userCode");
    let token = seen
        .meta
        .get("hub.mas_token")
        .unwrap_or_else(|| panic!("信封 meta 应透传请求的登录态，实际 {:?}", seen.meta));
    assert_eq!(
        token, "tk-admin",
        "meta 里必须是**这次请求**的 token，不是 login 缓存里别人的"
    );
}

/// **无凭证的 MCP 请求在闸门开启时不再死锁在 HTTP 401**。
///
/// 改造前：/mcp 无 Cookie → 中间件 401 → 客户端连 initialize 都发不出去，
/// login 工具永远调不到——闸门形同虚设。改造后：无凭证放行到工具面，
/// 插件调用被闸门拦下并给出**指引卡**（next_action 指向 login），
/// agent 照着做就能建立身份。闸门关闭时行为不变（另一条用例钉着 401）。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 闸门开启时无凭证的mcp请求拿到指引卡而不是401(pool: PgPool) {
    let (app, registry) = build_with_gate(pool, true);

    // 不注册任何插件也要成立：指引卡在调插件之前就给出
    let _unused = registry;

    let (session, value) = mcp_call(
        &app,
        None,
        None,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "gateless-client", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let session =
        session.unwrap_or_else(|| panic!("闸门开启时无凭证也应能建立会话，实际：{value}"));

    let (_, value) = mcp_call(
        &app,
        Some(&session),
        None,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "invoke_plugin",
                "arguments": { "plugin": "anything", "payload": {} }
            }
        }),
    )
    .await;

    // 拒卡经 json_result 渲染成 content[0].text 的 JSON 字符串
    let card: serde_json::Value = serde_json::from_str(
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default(),
    )
    .unwrap_or(serde_json::Value::Null);
    assert_eq!(
        card["login"]["required"], true,
        "插件调用应被闸门拦下并给指引卡，实际：{card}"
    );
    assert_eq!(
        card["next_action"]["tool"], "login",
        "指引卡要告诉 agent 下一步调什么，实际：{card}"
    );
}
