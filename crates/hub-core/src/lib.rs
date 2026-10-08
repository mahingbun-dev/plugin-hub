//! plugin-hub 核心：配置、领域模型与跨 crate 共享的基础类型。

pub mod auth;
pub mod config;
pub mod state;

pub use auth::AuthenticatedSubject;
pub use config::{Config, ConfigError};

/// 服务名。健康检查、指标标签、日志字段统一使用，避免各处字面量漂移。
pub const SERVICE_NAME: &str = "plugin-hub";
