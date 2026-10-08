//! anc-hub 服务端的可测试部分。
//!
//! 进程装配（`main.rs`）本身没有单测价值——它只有真的起一次进程才有意义。但装配起来
//! 的**后台任务**有：留存巡检决定「哪些数据该消失」，那里出错的后果是静默丢数据或者
//! 存储无限增长，两种都不会在启动日志里报错。
//!
//! 所以把任务拆到这里，让它们能被集成测试直接调。
//!
//! [`http_app`] 也是同一个理由：**路由与中间件的挂载顺序**读代码看不出对错，
//! 只有真的发一个请求才验得出来。

use axum::Router;
use hub_api::{ApiState, SystemState};

/// 组装 HTTP 面：系统面（探活 / 指标）+ 业务与管理面 + MCP 面。
///
/// MCP 面由调用方传进来（它由 `hub-mcp` 自己构造），**这里负责把它接进整条链**——
/// 关键是走 [`hub_api::router_with_extras`] 而不是自己 `merge`：鉴权挂在那个函数的
/// 最后一步，才能覆盖到 merge 进来的面。
pub fn http_app(system: SystemState, api: ApiState, mcp: Router) -> Router {
    hub_api::router_with_extras(system, api, mcp)
}

pub mod scheduler;
pub mod tasks;
