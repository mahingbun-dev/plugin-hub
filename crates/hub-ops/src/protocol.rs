//! 运维通道的协议定义。
//!
//! 请求与响应都是一行 JSON。命令名用 kebab-case，便于手敲。

use serde::{Deserialize, Serialize};

/// 一条运维指令。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum OpsRequest {
    /// 中台自检：数据库连通性、插件与实例计数、关键配置摘要
    Status,

    ListPlugins,

    ListInstances,

    /// 摘掉一个卡住的实例（插件进程已消失但心跳还没超时）
    RemoveInstance {
        instance_id: String,
    },

    /// 删除一个坏的插件版本。级联删掉它的契约、工具与实例，**不可逆**。
    RemoveVersion {
        plugin: String,
        version: String,
    },

    /// 删除整个插件。**不可逆**。
    DeletePlugin {
        name: String,
    },
}

impl OpsRequest {
    /// 该指令是否会改变状态。破坏性指令在 CLI 上要显式确认。
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::RemoveInstance { .. } | Self::RemoveVersion { .. } | Self::DeletePlugin { .. }
        )
    }

    pub fn command_name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::ListPlugins => "list-plugins",
            Self::ListInstances => "list-instances",
            Self::RemoveInstance { .. } => "remove-instance",
            Self::RemoveVersion { .. } => "remove-version",
            Self::DeletePlugin { .. } => "delete-plugin",
        }
    }
}

/// 一条运维响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpsResponse {
    pub ok: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl OpsResponse {
    pub fn ok(data: impl Into<serde_json::Value>) -> Self {
        Self {
            ok: true,
            data: Some(data.into()),
            error: None,
        }
    }

    pub fn empty_ok() -> Self {
        Self {
            ok: true,
            data: None,
            error: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 指令编码为带_command_的单个对象() {
        let json = serde_json::to_string(&OpsRequest::Status).expect("序列化失败");
        assert_eq!(json, r#"{"command":"status"}"#);

        let json = serde_json::to_string(&OpsRequest::RemoveVersion {
            plugin: "auth".to_string(),
            version: "1.2.0".to_string(),
        })
        .expect("序列化失败");
        assert!(
            json.contains(r#""command":"remove-version""#),
            "实际 {json}"
        );
        assert!(json.contains(r#""plugin":"auth""#));
    }

    #[test]
    fn 指令可从_json_还原() {
        let parsed: OpsRequest =
            serde_json::from_str(r#"{"command":"remove-instance","instance_id":"i-1"}"#)
                .expect("反序列化失败");
        assert_eq!(
            parsed,
            OpsRequest::RemoveInstance {
                instance_id: "i-1".to_string()
            }
        );
    }

    #[test]
    fn 未知指令被拒绝而不是静默忽略() {
        let err = serde_json::from_str::<OpsRequest>(r#"{"command":"drop-database"}"#);
        assert!(err.is_err(), "手敲错了必须报错，不能当成什么都不做");
    }

    #[test]
    fn 破坏性指令被标出() {
        assert!(!OpsRequest::Status.is_destructive());
        assert!(!OpsRequest::ListPlugins.is_destructive());
        assert!(
            OpsRequest::DeletePlugin {
                name: "x".to_string()
            }
            .is_destructive()
        );
    }

    #[test]
    fn 响应省略空字段() {
        let json = serde_json::to_string(&OpsResponse::empty_ok()).expect("序列化失败");
        assert_eq!(json, r#"{"ok":true}"#);
    }
}
