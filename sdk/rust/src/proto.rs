//! 契约代码。
//!
//! 生成产物 `proto/hub.v1.rs` **提交进仓库**，构建期不生成——理由见
//! `sdk/rust/README.md` 的「proto 产物为什么是提交的」。重新生成：
//!
//! ```console
//! cargo run --manifest-path sdk/rust/protogen/Cargo.toml
//! ```

/// 生成代码。允许 lint 例外：prost / tonic 的产物不受本项目风格约束。
#[allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all)]
pub mod v1 {
    include!("proto/hub.v1.rs");
}

pub use v1::{
    DescribeRequest, Envelope, FailureAction, HandleRequest, HandleResponse, HealthRequest,
    HealthResponse, HeartbeatRequest, HeartbeatResponse, MessageContract, PayloadRef, PayloadType,
    PluginManifest, RegisterRequest, RegisterResponse, RejectCode, Rejection, Severity, Subject,
    SubjectKind, ToolDecl, UnregisterRequest, UnregisterResponse, ValidateRequest,
    ValidateResponse, ValidationIssue, ValidationPolicy,
};

/// 插件发现与互调（PluginGateway）的消息。
///
/// 客户端在 [`crate::gateway`]——四个 RPC 全部凭状态凭证鉴权，凭证仍由骨架注入。
pub use v1::{
    DescribeMessageRequest, DescribeMessageResponse, GetContractRequest, GetContractResponse,
    InvokeOutcome, InvokeRequest, InvokeResponse, ListPluginsRequest, ListPluginsResponse,
    MessageEndpoint, PluginSummary,
};

/// HubState（外置状态）的消息。
///
/// 客户端在 [`crate::state`]，`publish()` 也在那里（触发下游 flow 的出口）。
/// `PublishRequest` / `PublishResponse` 同时被服务端 trait 与客户端用到——
/// 伪造一个状态面（测试、mock）要用前者，插件发起投递用后者。
pub use v1::{
    KvDeleteRequest, KvDeleteResponse, KvEntry, KvGetRequest, KvGetResponse, KvKey, KvPutRequest,
    KvPutResponse, KvScanRequest, KvScanResponse, PublishRequest, PublishResponse,
};

// 生成的服务模块。插件侧要用的都在这几个里：
//   - plugin_runtime_server  —— 插件**实现**的服务（PluginRuntime）
//   - plugin_registry_client —— 插件**调用**的服务（PluginRegistry：注册 / 心跳 / 注销）
//   - plugin_registry_server —— 测试与 mock 用（伪造一个中台）
//   - plugin_runtime_client  —— 自测与调试用（直连插件打它的五个方法）
//   - hub_state_client       —— 插件**调用**的服务（HubState：外置状态），由骨架构造
//   - hub_state_server       —— 测试与 mock 用（伪造一个状态面）
//   - plugin_gateway_client  —— 插件**调用**的服务（PluginGateway：发现与互调），由骨架构造
//   - plugin_gateway_server  —— 测试与 mock 用（伪造一个网关面）
pub use v1::{
    hub_state_client, hub_state_server, plugin_gateway_client, plugin_gateway_server,
    plugin_registry_client, plugin_registry_server, plugin_runtime_client, plugin_runtime_server,
};

/// 契约包名，与 proto 中的 `package hub.v1;` 对应。
pub const PACKAGE: &str = "hub.v1";

/// 构造 `google.protobuf.Any.type_url`。
///
/// 统一走这个函数，避免各处手拼前缀导致契约标识格式漂移——中台按 `type_url` 的
/// 末段认契约，拼错一个字符就是「类型对不上」。
pub fn type_url_for(fq_name: &str) -> String {
    format!("type.googleapis.com/{fq_name}")
}

/// 从 `google.protobuf.Any.type_url` 中取出全限定消息名。
///
/// 接受两种写法：带前缀的完整 `type_url`，以及裸全限定名。
/// 返回 `None` 表示取不到消息名（空串、纯空白，或以 `/` 结尾）。
pub fn fq_name_from_type_url(type_url: &str) -> Option<&str> {
    let name = match type_url.rsplit_once('/') {
        Some((_, tail)) => tail,
        None => type_url,
    };
    let name = name.trim();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 构造与解析_type_url_互为逆运算() {
        let fq = "wms.v1.OrderCreated";
        assert_eq!(fq_name_from_type_url(&type_url_for(fq)), Some(fq));
    }

    #[test]
    fn 裸全限定名原样返回() {
        assert_eq!(
            fq_name_from_type_url("wms.v1.OrderCreated"),
            Some("wms.v1.OrderCreated")
        );
    }

    #[test]
    fn 无效的_type_url_返回_none() {
        assert_eq!(fq_name_from_type_url(""), None);
        assert_eq!(fq_name_from_type_url("   "), None);
        assert_eq!(fq_name_from_type_url("type.googleapis.com/"), None);
    }

    #[test]
    fn 契约包名与_proto_声明一致() {
        assert_eq!(PACKAGE, "hub.v1");
    }
}
