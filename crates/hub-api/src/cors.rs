//! CORS：嵌入场景的传输层开关。
//!
//! 嵌入方前端与 hub 不同 origin（同站不同端口/子域是常见形态）时，浏览器要求
//! 服务端应答 CORS 头，带 Cookie 的跨域 fetch 才会被放行。这一层只在
//! `HUB_CORS_ALLOWED_ORIGINS` 配了白名单时挂载（见 `router_with_extras`），
//! 且挂在鉴权**外面**——预检 OPTIONS 不带凭证，必须在这里短路应答，
//! 不能让它先撞鉴权的 401。
//!
//! 刻意不做 `*`：带凭证的 CORS 规范禁止通配 Origin
//! （`Access-Control-Allow-Credentials: true` 与 `*` 同出时浏览器直接拒绝），
//! 而嵌入要带的正是 Cookie。所以只做**精确回显**白名单里的 origin。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// 允许跨域携带的方法。管理面用得上的全集：读写都在内，
/// TRACE/CONNECT 这类用不上也不该开。
const ALLOWED_METHODS: &str = "GET, POST, PUT, DELETE, OPTIONS";

/// 允许跨域携带的请求头。
///
/// `Authorization` 是 Bearer 直传通道（嵌入场景 / MCP 客户端 headers），
/// `Content-Type` 是 JSON 体的必需项，`X-Requested-With` 是前端惯例的
/// CSRF 标识。Cookie 由浏览器按 credentials 语义自动管理，不在此列。
const ALLOWED_HEADERS: &str = "Authorization, Content-Type, X-Requested-With";

/// 预检结果让浏览器缓存十分钟：预检每次都打一趟会让嵌入页面的每个请求
/// 都多一个来回，而白名单是部署期配置，十分钟内不会变。
const PREFLIGHT_MAX_AGE: &str = "600";

/// CORS 中间件。`allowed` 是精确匹配的 Origin 白名单（协议 + 域名 + 端口全等）。
///
/// 行为分三种：
///
/// 1. 预检（OPTIONS + 有 `Access-Control-Request-Method`）且 Origin 在白名单：
///    **短路 204**，不进鉴权（预检没有凭证，进去必 401，浏览器就卡死了）。
/// 2. 非 OPTIONS 且 Origin 在白名单：放行到鉴权，应答头追加 CORS 三件套。
/// 3. Origin 不在白名单（或没带 Origin）：**不加任何 CORS 头**原样走——
///    拦截由浏览器执行（它看到没有 Allow-Origin 就拒绝把响应交给页面）。
///    中台不主动 403：那样会把「非浏览器调用方」（curl、服务端转发）也拦掉，
///    而它们根本不受 CORS 约束。
pub async fn handle(
    State(allowed): State<Arc<Vec<String>>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    else {
        // 没有 Origin 头 = 非浏览器调用（同源 fetch 也不带），与 CORS 无关
        return next.run(request).await;
    };

    if !allowed.iter().any(|o| o == &origin) {
        // 白名单外：不加 CORS 头（浏览器自行拦截），也不改写请求——
        // 服务端转发、curl 这些非浏览器调用方照常工作
        return next.run(request).await;
    }

    let preflight = request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);

    if preflight {
        // 预检短路。Vary: Origin 是缓存正确性的要求——应答随 Origin 变化，
        // 共享缓存按 URL 键存的话会把 A 站的应答吐给 B 站
        let mut response = StatusCode::NO_CONTENT.into_response();
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_value(&origin));
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static(ALLOWED_METHODS),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static(ALLOWED_HEADERS),
        );
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static(PREFLIGHT_MAX_AGE),
        );
        // 嵌入要带 Cookie，credentials 必须开；开了它就不能配通配 Origin（见模块注释）
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        return response;
    }

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_value(&origin));
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    response
}

fn origin_value(origin: &str) -> HeaderValue {
    HeaderValue::from_str(origin).unwrap_or_else(|_| HeaderValue::from_static("null"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::middleware;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        let origins = vec![
            "https://console.example.com".to_string(),
            "https://app.example.com:8443".to_string(),
        ];
        axum::Router::new()
            .route("/ping", get(|| async { "pong" }))
            .layer(middleware::from_fn_with_state(Arc::new(origins), handle))
    }

    #[tokio::test]
    async fn 预检在白名单内短路204并带全套头() {
        let response = app()
            .oneshot(
                HttpRequest::builder()
                    .method(Method::OPTIONS)
                    .uri("/ping")
                    .header(header::ORIGIN, "https://console.example.com")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(
            headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://console.example.com"
        );
        assert_eq!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
        assert_eq!(headers.get(header::VARY).unwrap(), "Origin");
        assert!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("POST")
        );
    }

    #[tokio::test]
    async fn 白名单外的origin不加cors头但请求照常处理() {
        let response = app()
            .oneshot(
                HttpRequest::builder()
                    .method(Method::OPTIONS)
                    .uri("/ping")
                    .header(header::ORIGIN, "https://evil.example.net")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }

    #[tokio::test]
    async fn 白名单内的普通请求应答带allow_origin与credentials() {
        let response = app()
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/ping")
                    .header(header::ORIGIN, "https://app.example.com:8443")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://app.example.com:8443"
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
    }

    #[tokio::test]
    async fn 无origin头的请求原样穿透() {
        let response = app()
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }
}
