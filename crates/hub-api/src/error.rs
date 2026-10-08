//! HTTP 面的错误映射。
//!
//! 状态码刻意区分「数据不合法」（422，调用方的问题）与「插件不可用」（502，下游的问题）
//! ——调用方据此决定是改数据还是重试。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),

    BadRequest(String),

    /// 校验器拒绝：语义上是数据不合法，不是服务错误
    ValidationRejected {
        plugin: String,
        issues: Vec<IssueDto>,
    },

    /// 调用插件失败：插件不可达、超时或返回错误
    Upstream {
        plugin: String,
        message: String,
    },

    /// 被实例级治理拦下：实例已熔断，或并发到顶且排队超时。
    ///
    /// 两种情况都表达「不是这份数据的错，稍后再来」，靠状态码区分：
    /// 熔断 → 503（这个实例现在不能用），背压 → 429（整体太忙）。
    /// 调用方据此决定等多久、要不要降级，比笼统的 502 有用得多。
    Governed {
        plugin: String,
        message: String,
        overloaded: bool,
    },

    /// 状态冲突：请求没毛病，但现在做不了
    Conflict(String),

    /// 总线堆积到顶（429）
    Overloaded {
        depth: usize,
        limit: usize,
    },

    /// 这个实例没开这项能力（503）。**明确说出来**比让调用方以为消息已经发出去了好。
    Unavailable(String),

    Internal(String),
}

/// 校验问题的对外形态，与 `hub.v1.ValidationIssue` 对应。
#[derive(Debug, Clone, Serialize)]
pub struct IssueDto {
    pub path: String,
    pub message: String,
    pub severity: i32,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    issues: Vec<IssueDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin: Option<String>,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    /// 请求本身没错，但当前状态下做不了（比如重复重放一条已经重放过的死信）。
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    /// 上游忙不过来。单独一个状态码（429）而不是 502：调用方对它的处置是「稍后再来
    /// 或降级」，而不是「上游坏了」。
    pub fn overloaded(depth: usize, limit: usize) -> Self {
        Self::Overloaded { depth, limit }
    }

    /// 这项能力在本实例上没开。
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message)
            | Self::BadRequest(message)
            | Self::Conflict(message)
            | Self::Unavailable(message)
            | Self::Internal(message)
            | Self::Governed { message, .. }
            | Self::Upstream { message, .. } => f.write_str(message),
            Self::ValidationRejected { plugin, issues } => write!(
                f,
                "插件 {plugin} 的校验器拒绝该数据（{} 个问题）",
                issues.len()
            ),
            Self::Overloaded { depth, limit } => {
                write!(f, "总线堆积已达上限（{depth}/{limit}），拒绝入队")
            }
        }
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::NotFound(message) => (
                StatusCode::NOT_FOUND,
                ErrorBody {
                    error: "not_found",
                    message,
                    issues: Vec::new(),
                    plugin: None,
                },
            ),
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: "bad_request",
                    message,
                    issues: Vec::new(),
                    plugin: None,
                },
            ),
            Self::ValidationRejected { plugin, issues } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorBody {
                    error: "validation_rejected",
                    message: format!("插件 {plugin} 的校验器拒绝了该数据"),
                    issues,
                    plugin: Some(plugin),
                },
            ),
            Self::Upstream { plugin, message } => (
                StatusCode::BAD_GATEWAY,
                ErrorBody {
                    error: "plugin_unavailable",
                    message,
                    issues: Vec::new(),
                    plugin: Some(plugin),
                },
            ),
            Self::Conflict(message) => (
                StatusCode::CONFLICT,
                ErrorBody {
                    error: "conflict",
                    message,
                    issues: Vec::new(),
                    plugin: None,
                },
            ),
            Self::Overloaded { depth, limit } => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorBody {
                    error: "overloaded",
                    message: format!("总线堆积已达上限（{depth}/{limit}），拒绝入队"),
                    issues: Vec::new(),
                    plugin: None,
                },
            ),
            Self::Unavailable(message) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorBody {
                    error: "unavailable",
                    message,
                    issues: Vec::new(),
                    plugin: None,
                },
            ),
            Self::Governed {
                plugin,
                message,
                overloaded,
            } => (
                if overloaded {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                },
                ErrorBody {
                    error: if overloaded {
                        "overloaded"
                    } else {
                        "circuit_open"
                    },
                    message,
                    issues: Vec::new(),
                    plugin: Some(plugin),
                },
            ),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorBody {
                    error: "internal_error",
                    message,
                    issues: Vec::new(),
                    plugin: None,
                },
            ),
        };

        (status, Json(body)).into_response()
    }
}

/// 从执行层错误映射。把「插件不可用」与「内部错误」分开：前者调用方可重试，
/// 后者重试无用。
impl From<hub_engine::InvokeError> for ApiError {
    fn from(err: hub_engine::InvokeError) -> Self {
        match err {
            hub_engine::InvokeError::Resolve(registry_err) => match registry_err {
                hub_registry::RegistryError::PluginNotFound(name) => {
                    Self::NotFound(format!("插件 {name} 未注册"))
                }
                hub_registry::RegistryError::VersionNotFound { plugin, version } => {
                    Self::NotFound(format!("插件 {plugin} 没有版本 {version}"))
                }
                hub_registry::RegistryError::NoHealthyInstance { plugin } => Self::Upstream {
                    plugin,
                    message: "该插件当前没有可用实例（全部掉线或尚未注册）".to_string(),
                },
                other => Self::Internal(other.to_string()),
            },
            hub_engine::InvokeError::Client { plugin, source, .. } => Self::Upstream {
                plugin,
                message: source.to_string(),
            },
            hub_engine::InvokeError::Timeout {
                plugin,
                stage,
                budget_ms,
            } => Self::Upstream {
                plugin,
                message: format!("{stage} 超出剩余预算（{budget_ms}ms）"),
            },
            hub_engine::InvokeError::EmptyResponse { plugin } => Self::Upstream {
                plugin,
                message: "插件未返回信封".to_string(),
            },
            hub_engine::InvokeError::Govern(govern_err) => {
                let plugin = match &govern_err {
                    hub_engine::GovernError::CircuitOpen { plugin, .. }
                    | hub_engine::GovernError::Overloaded { plugin, .. } => plugin.clone(),
                };
                Self::Governed {
                    plugin,
                    // 治理错误本身的文案已经把「失败了多少次 / 排了多久队」说清楚了，
                    // 这里不再套一层，调用方看到的越直接越好
                    message: govern_err.to_string(),
                    overloaded: matches!(govern_err, hub_engine::GovernError::Overloaded { .. }),
                }
            }
        }
    }
}

impl From<hub_store::StoreError> for ApiError {
    fn from(err: hub_store::StoreError) -> Self {
        Self::Internal(err.to_string())
    }
}
