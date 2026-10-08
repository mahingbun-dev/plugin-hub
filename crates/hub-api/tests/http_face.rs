//! 探活与指标的行为测试。
//!
//! 这些断言同时锁住了部署链路依赖的契约：CI 的部署门禁靠 `/health` 返回
//! `"status":"ok"` 判定成功，改动响应结构会直接让流水线回滚。
//!
//! 只用 `system_router`：探活与指标不该依赖数据库，测试它们也不该需要数据库。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_api::{SystemState, system_router};
use tower::ServiceExt as _;

async fn get(uri: &str) -> (StatusCode, Vec<u8>) {
    let app = system_router(SystemState::without_metrics());
    let res = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("构造请求失败"),
        )
        .await
        .expect("路由处理失败");
    let status = res.status();
    let body = res
        .into_body()
        .collect()
        .await
        .expect("读取响应体失败")
        .to_bytes()
        .to_vec();
    (status, body)
}

#[tokio::test]
async fn 健康检查返回_ok_与版本信息() {
    let (status, body) = get("/health").await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_slice(&body).expect("响应不是合法 JSON");
    // 部署门禁按这个字段判定，必须保持
    assert_eq!(json["status"], "ok");
    assert_eq!(json["name"], "plugin-hub");
    assert!(json["version"].is_string());
    assert!(json["uptime_seconds"].is_u64());
}

#[tokio::test]
async fn 未安装指标句柄时_metrics_返回不可用而非报错() {
    let (status, body) = get("/metrics").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        String::from_utf8_lossy(&body).contains("未安装"),
        "应说明原因，实际响应: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn 未知路径返回_404() {
    let (status, _) = get("/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// 没配 `HUB_MCP_PUBLIC_ENDPOINT` 时，`mcp` 必须是显式的 `null` 而不是缺字段——
/// 控制台靠这个值决定「显示配置的接入地址」还是「退回浏览器推导的地址」。
#[tokio::test]
async fn 未配置时_endpoints_的_mcp_为_null() {
    let (status, body) = get("/endpoints").await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_slice(&body).expect("响应不是合法 JSON");
    assert!(json["mcp"].is_null(), "实际响应: {json}");
}

#[tokio::test]
async fn 配置的对外端点经_endpoints_原样下发() {
    let app = system_router(SystemState {
        metrics: None,
        mcp_endpoint: Some("https://203.0.113.10:8081/mcp".to_string()),
    });
    let res = app
        .oneshot(
            Request::builder()
                .uri("/endpoints")
                .body(Body::empty())
                .expect("构造请求失败"),
        )
        .await
        .expect("路由处理失败");
    assert_eq!(res.status(), StatusCode::OK);

    let body = res
        .into_body()
        .collect()
        .await
        .expect("读取响应体失败")
        .to_bytes()
        .to_vec();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("响应不是合法 JSON");
    assert_eq!(json["mcp"], "https://203.0.113.10:8081/mcp");
}
