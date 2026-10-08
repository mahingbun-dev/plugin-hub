//! anc-hub 契约层：protobuf 定义与生成代码。
//!
//! 这里是**唯一的契约来源**——插件 manifest、MCP 工具入参 schema、编排时的
//! 兼容性校验，全部由这些 proto 派生，不维护第二套 schema。
//!
//! proto 源文件在 `proto/hub/v1/`，构建期由 `build.rs` 调用 vendored protoc 生成。

/// 契约包名，与 proto 中的 `package hub.v1;` 对应。
pub const PACKAGE: &str = "hub.v1";

pub mod json;

pub use json::{JsonPayloadError, STRUCT_FQ_NAME, STRUCT_TYPE_URL, decode_payload, encode_payload};

/// 生成代码。允许 lint 例外：prost/tonic 的产物不受本项目风格约束。
#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/hub.v1.rs"));
}

pub use v1::{
    BusMessage, DescribeRequest, Envelope, HandleRequest, HandleResponse, HealthRequest,
    HealthResponse, MessageContract, PayloadRef, PayloadType, PluginManifest, Subject, SubjectKind,
    ToolDecl, ValidateRequest, ValidateResponse, ValidationIssue, ValidationPolicy,
};

/// 构造 `google.protobuf.Any.type_url`。
///
/// 统一走这个函数，避免各处手拼前缀导致契约标识格式漂移。
pub fn type_url_for(fq_name: &str) -> String {
    format!("type.googleapis.com/{fq_name}")
}

/// 从 `google.protobuf.Any.type_url` 中取出全限定消息名。
///
/// **契约标识就是它**：编排时比对上下游的全限定名能否对上，运行时校验实际类型
/// 是否落在插件 manifest 声明的 `produces`/`consumes` 内。
///
/// 接受两种写法：
/// - 带前缀的完整 type_url：`type.googleapis.com/wms.v1.OrderCreated` → `wms.v1.OrderCreated`
/// - 裸全限定名：`wms.v1.OrderCreated` → `wms.v1.OrderCreated`
///
/// 返回 `None` 表示取不到消息名（空串、纯空白，或以 `/` 结尾）。
pub fn fq_name_from_type_url(type_url: &str) -> Option<&str> {
    let name = match type_url.rsplit_once('/') {
        Some((_, tail)) => tail,
        None => type_url,
    };
    let name = name.trim();
    (!name.is_empty()).then_some(name)
}

/// [`v1::RejectCode`] 枚举值的展示名（`VERSION_CONFLICT` 等）。
///
/// 拒绝码在库里与协议里以 i32 流转，控制台与 MCP 响应里给的应该是名字。
/// 映射集中在这一个函数：枚举长在 hub-proto，api 面与 MCP 面各抄一份
/// match 就是等着它们漂移。未知值（新码出现在旧数据里）回落到 `CODE_{n}`，
/// 而不是 panic 或丢字段——枚举可以演进了再补名字。
pub fn rejection_code_name(code: i32) -> String {
    use v1::RejectCode;
    match RejectCode::try_from(code) {
        Ok(RejectCode::Unreachable) => "UNREACHABLE".to_string(),
        Ok(RejectCode::DescriptorInvalid) => "DESCRIPTOR_INVALID".to_string(),
        Ok(RejectCode::BreakingChange) => "BREAKING_CHANGE".to_string(),
        Ok(RejectCode::ToolConflict) => "TOOL_CONFLICT".to_string(),
        Ok(RejectCode::VersionConflict) => "VERSION_CONFLICT".to_string(),
        Ok(RejectCode::ManifestInvalid) => "MANIFEST_INVALID".to_string(),
        Ok(RejectCode::Internal) => "INTERNAL".to_string(),
        Ok(RejectCode::InstanceConflict) => "INSTANCE_CONFLICT".to_string(),
        _ => format!("CODE_{code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 从带前缀的_type_url_取出全限定名() {
        assert_eq!(
            fq_name_from_type_url("type.googleapis.com/wms.v1.OrderCreated"),
            Some("wms.v1.OrderCreated")
        );
    }

    #[test]
    fn 裸全限定名原样返回() {
        assert_eq!(
            fq_name_from_type_url("wms.v1.OrderCreated"),
            Some("wms.v1.OrderCreated")
        );
    }

    #[test]
    fn 空或无效的_type_url_返回_none() {
        assert_eq!(fq_name_from_type_url(""), None);
        assert_eq!(fq_name_from_type_url("   "), None);
        assert_eq!(fq_name_from_type_url("type.googleapis.com/"), None);
        assert_eq!(fq_name_from_type_url("type.googleapis.com/   "), None);
    }

    #[test]
    fn 构造与解析互为逆运算() {
        let fq = "wms.v1.OrderCreated";
        assert_eq!(fq_name_from_type_url(&type_url_for(fq)), Some(fq));
    }

    #[test]
    fn 契约包名与_proto_声明一致() {
        assert_eq!(PACKAGE, "hub.v1");
    }

    #[test]
    fn 生成的类型可正常构造与编码() {
        let env = Envelope {
            message_id: "01J0TEST".to_string(),
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_string(),
            payload: Some(prost_types::Any {
                type_url: type_url_for("wms.v1.OrderCreated"),
                value: vec![1, 2, 3],
            }),
            ..Default::default()
        };

        let fq = fq_name_from_type_url(&env.payload.as_ref().unwrap().type_url);
        assert_eq!(fq, Some("wms.v1.OrderCreated"));

        // proto 编解码往返，确认生成的类型真的可用
        use prost::Message as _;
        let mut buf = Vec::new();
        env.encode(&mut buf).expect("编码失败");
        let decoded = Envelope::decode(buf.as_slice()).expect("解码失败");
        assert_eq!(decoded.message_id, env.message_id);
        assert_eq!(decoded.trace_id, env.trace_id);
    }
}
