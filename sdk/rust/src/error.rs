//! 错误类型。
//!
//! 手写 `Display` 而不是引 `thiserror`：SDK 要随包发给插件团队，能不加的依赖就不加，
//! 而这里要呈现的错误不到十条。代价是 `source()` 要自己接——值得。

use std::fmt;

use crate::config::ConfigError;
use crate::proto::{RejectCode, Rejection};

/// 骨架自身抛出的错误。
#[derive(Debug)]
pub enum HubkitError {
    /// 配置不合法。
    Config(ConfigError),

    /// `Plugin::manifest()` 返回了空插件名。
    ManifestMissingName,
    /// `Plugin::manifest()` 返回了空版本号——flow 靠它锁定实例，缺了它插件接不进来。
    ManifestMissingVersion,
    /// manifest 声明了自有类型，但 `Plugin::descriptor()` 是空的。
    ///
    /// 中台找不到那些类型的出处会拒；本地先炸能省一轮网络往返与排查。
    OwnTypeWithoutDescriptor(String),

    /// 监听失败。多半是端口被占。
    Bind {
        /// 想绑但没绑上的地址。
        addr: String,
        /// 底层原因（多半是端口被占）。
        source: std::io::Error,
    },

    /// 连不上中台 / 地址不合法。
    Connect {
        /// 中台地址。
        addr: String,
        /// 底层原因。
        source: Box<tonic::transport::Error>,
    },

    /// gRPC 服务异常退出。
    Serve(String),

    /// 中台拒绝了注册。
    RegistrationRejected(Vec<Rejection>),

    /// 本地契约自检未通过。里面是已经排好版的报告，直接打给人看。
    Conformance(String),

    /// 载荷不是 JSON 对象。
    PayloadNotObject,
    /// 载荷编码失败。
    EncodePayload(String),
}

impl fmt::Display for HubkitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubkitError::Config(e) => write!(f, "{e}"),
            HubkitError::ManifestMissingName => {
                write!(f, "hubkit: manifest 缺少插件名（name）")
            }
            HubkitError::ManifestMissingVersion => write!(
                f,
                "hubkit: manifest 缺少版本号（version）——flow 靠它锁定实例"
            ),
            HubkitError::OwnTypeWithoutDescriptor(fq) => write!(
                f,
                "hubkit: manifest 声明了自有类型 {fq}，但 Plugin::descriptor() 返回空——\
                 要么把它从 produces/consumes 里去掉，要么提供它的 proto"
            ),
            HubkitError::Bind { addr, source } => {
                write!(f, "hubkit: 监听 {addr} 失败: {source}")
            }
            HubkitError::Connect { addr, source } => {
                write!(f, "hubkit: 连接中台 {addr} 失败: {source}")
            }
            HubkitError::Serve(e) => write!(f, "hubkit: gRPC 服务异常退出: {e}"),
            HubkitError::RegistrationRejected(rejections) => {
                write!(f, "{}", format_rejections(rejections))
            }
            HubkitError::Conformance(report) => write!(f, "{report}"),
            HubkitError::PayloadNotObject => {
                write!(f, "hubkit: 载荷必须是 JSON 对象")
            }
            HubkitError::EncodePayload(e) => write!(f, "hubkit: 打包载荷失败: {e}"),
        }
    }
}

impl std::error::Error for HubkitError {}

impl From<ConfigError> for HubkitError {
    fn from(e: ConfigError) -> Self {
        HubkitError::Config(e)
    }
}

/// 把中台的拒绝原因排成人能直接照着改的多行文本。
///
/// 多行是有意的：给 `fmt::Display`、错误链、`main` 的兜底打印用，那里多行合适。
/// **日志是另一个呈现渠道**——走 `registrar` 的结构化逐条输出，不经过这里。
/// 一整段多行文本塞进 JSON 日志会被转义成 `\n`，一行里糊着 N 条原因，
/// 读日志的人得靠脑内反解析，所以两条路刻意分开。
pub fn format_rejections(rejections: &[Rejection]) -> String {
    let mut out = String::from("中台拒绝了注册");
    if rejections.is_empty() {
        out.push_str("（未给出原因）");
        return out;
    }
    out.push('：');
    for r in rejections {
        out.push_str(&format!(
            "\n  - {}: {}",
            reject_code_name(r.code),
            r.message
        ));
        if !r.detail.is_empty() {
            out.push_str(&format!("\n      {}", r.detail));
        }
    }
    out
}

/// 把拒绝码翻成人话。
///
/// 去掉 `REJECT_CODE_` 前缀：日志里 `UNREACHABLE` 比 `REJECT_CODE_UNREACHABLE` 好读，
/// 而前缀是 proto 枚举的编码约定，不是给人看的。认不出的码原样带上数字，
/// **不吞掉**——吞掉会让「中台加了新码」这件事在日志里看不出来。
pub fn reject_code_name(code: i32) -> String {
    match RejectCode::try_from(code) {
        Ok(known) => known
            .as_str_name()
            .strip_prefix("REJECT_CODE_")
            .unwrap_or(known.as_str_name())
            .to_string(),
        Err(_) => format!("未知拒绝码({code})"),
    }
}

/// 插件自己的错误。
///
/// 用一个把 `Box<dyn Error>` 包起来的类型，是为了让插件作者能直接 `?`——
/// `impl<E: Error + Send + Sync + 'static> From<E>` 之后，
/// `serde_json::from_str(...)?`、`std::fs::read(...)?` 都能自动转换，
/// 不必为每种错误手写一次 `map_err`。
///
/// 它**不实现** `std::error::Error`：那会与上面那条 `From` 撞车（`From<T> for T`）。
/// 插件要把它交给别人的错误链时，用 [`PluginError::into_boxed`]。
pub struct PluginError(Box<dyn std::error::Error + Send + Sync + 'static>);

impl PluginError {
    /// 用一句话构造。
    pub fn msg(message: impl Into<String>) -> Self {
        Self(message.into().into())
    }

    /// 取出内部的错误对象，接到别人的错误链上。
    pub fn into_boxed(self) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self.0
    }
}

impl<E> From<E> for PluginError
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(e: E) -> Self {
        Self(Box::new(e))
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拒绝码去掉前缀且未知码不吞掉() {
        assert_eq!(
            reject_code_name(RejectCode::Unreachable as i32),
            "UNREACHABLE"
        );
        assert_eq!(
            reject_code_name(RejectCode::InstanceConflict as i32),
            "INSTANCE_CONFLICT"
        );
        assert_eq!(reject_code_name(999), "未知拒绝码(999)");
    }

    #[test]
    fn 拒绝原因逐条排开且带上_detail() {
        let text = format_rejections(&[Rejection {
            code: RejectCode::Unreachable as i32,
            message: "插件地址不可达".into(),
            detail: "请确认 HUB_ADVERTISE_ADDR 填的是中台视角下可达的地址".into(),
        }]);
        assert!(text.contains("UNREACHABLE: 插件地址不可达"), "{text}");
        assert!(text.contains("中台视角"), "{text}");
    }

    #[test]
    fn 插件错误可以直接从任意_error_转换() {
        let e: PluginError = std::io::Error::new(std::io::ErrorKind::NotFound, "没了").into();
        assert!(e.to_string().contains("没了"));

        let e = PluginError::msg("自定义");
        assert_eq!(e.to_string(), "自定义");
        assert!(e.into_boxed().to_string().contains("自定义"));
    }
}
