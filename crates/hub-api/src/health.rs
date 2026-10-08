//! 健康检查：供 nginx、容器 healthcheck 与部署门禁探活。
//!
//! 该端点不鉴权——它是运维探活入口，且管理面按设计是全插件化的。
//! CI 的部署门禁依赖它返回 `"status":"ok"`，改动响应结构会同时影响部署脚本。

use std::sync::OnceLock;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use hub_core::SERVICE_NAME;
use serde::Serialize;

use crate::SystemState;

/// 进程启动时刻。由 [`mark_started`] 在启动时写入一次。
static STARTED_AT: OnceLock<Instant> = OnceLock::new();

/// 记录进程启动时刻；重复调用无副作用。
pub fn mark_started() {
    let _ = STARTED_AT.set(Instant::now());
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    pub uptime_seconds: u64,
}

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        name: SERVICE_NAME,
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: STARTED_AT.get().map_or(0, |t| t.elapsed().as_secs()),
    })
}

/// 对外接入端点。控制台的「插件目录」页据此显示 MCP 接入地址。
#[derive(Debug, Serialize)]
pub struct EndpointsResponse {
    /// MCP 面对外的完整端点 URL（`HUB_MCP_PUBLIC_ENDPOINT`）。
    ///
    /// 为 `None` = 部署没配：中台不知道自己对外是什么地址（它监听的是回环地址），
    /// **不猜**——控制台退回从浏览器地址推导的域名形态，行为与没有这个接口时一致。
    pub mcp: Option<String>,
}

pub async fn endpoints(State(state): State<SystemState>) -> Json<EndpointsResponse> {
    Json(EndpointsResponse {
        mcp: state.mcp_endpoint.clone(),
    })
}
