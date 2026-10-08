//! Prometheus 指标出口。
//!
//! 插件自身的指标不在这里暴露：插件是独立进程，中台在**调用点**统一打点，
//! 标签里带 plugin / version / instance，避免各插件往中台指标里写脏标签。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::SystemState;

pub async fn metrics(State(state): State<SystemState>) -> Response {
    match &state.metrics {
        Some(handle) => (
            StatusCode::OK,
            [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
            handle.render(),
        )
            .into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "metrics recorder 未安装\n").into_response(),
    }
}
