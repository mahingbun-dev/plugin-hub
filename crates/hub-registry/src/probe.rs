//! 插件可达性探测。
//!
//! 插件可能部署在任何主机上（见 `docs/design.md` 的网络拓扑），注册时**必须先探一次**：
//! 地址写错时若不探测，就会出现「注册成功但永远调不通」这种最难排查的状态——
//! 控制台上一切正常，直到有业务流量打过去才暴雷。
//!
//! 抽成 trait 是为了让注册流程能在单测里跑：生产实现是 gRPC 客户端，
//! 测试实现可以随意伪造健康 / 不健康 / 不可达三种结果。

use async_trait::async_trait;

/// 探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// 连得上且插件自报健康
    Healthy { message: String },

    /// 连得上但插件自报不健康
    Unhealthy { message: String },

    /// 连不上（网络不通、端口没开、不是 gRPC 端点等）
    Unreachable { message: String },
}

impl ProbeOutcome {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy { .. })
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Healthy { message }
            | Self::Unhealthy { message }
            | Self::Unreachable { message } => message,
        }
    }
}

#[async_trait]
pub trait PluginProbe: Send + Sync + 'static {
    /// 按插件自报的 `advertise_addr` 调它的 `Health`。
    async fn health(&self, advertise_addr: &str) -> ProbeOutcome;
}

/// 总是探测成功的实现。只在明确知道不需要探测的场合使用（如单测里不关心可达性）。
pub struct AlwaysHealthy;

#[async_trait]
impl PluginProbe for AlwaysHealthy {
    async fn health(&self, _advertise_addr: &str) -> ProbeOutcome {
        ProbeOutcome::Healthy {
            message: "未启用探测".to_string(),
        }
    }
}
