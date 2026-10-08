//! manifest 自述校验。
//!
//! 只查「插件自己说自己」是否自洽，不查与外部的关系——契约兼容性在 `lib.rs` 里比对。

use std::collections::HashSet;

use hub_contract::{ContractIndex, undeclared_messages};
use hub_proto::v1::{PluginManifest, RejectCode, Rejection};

/// 插件名规则。与 MCP 工具前缀、容器名、URL 段都能兼容。
pub fn is_valid_plugin_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    name.len() <= 64 && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 校验 manifest 本身。
///
/// 返回的每一条都会作为注册拒绝原因回给插件方，措辞守 `lib.rs` 模块文档里那条约定：
/// `message` 点出哪里错了，`detail` 给出插件方接下来该做什么。
pub fn validate_manifest(manifest: &PluginManifest, index: &ContractIndex) -> Vec<Rejection> {
    let mut rejections = Vec::new();

    if manifest.name.is_empty() {
        rejections.push(Rejection::new(
            RejectCode::ManifestInvalid,
            "manifest 缺少插件名",
            // 规则在这里一并给出：插件名同时是 MCP 工具前缀与实例/容器标识的来源，
            // 只说「不能为空」的话，插件方填个中文名回来还是会被拒。
            "name 不能为空；请在 Manifest() 里填上插件名，只允许字母数字与 -_，最长 64 字符，\
             且以字母数字开头",
        ));
    } else if !is_valid_plugin_name(&manifest.name) {
        rejections.push(Rejection::new(
            RejectCode::ManifestInvalid,
            format!("插件名 {} 非法", manifest.name),
            "只允许字母数字与 -_，最长 64 字符，且以字母数字开头",
        ));
    }

    if manifest.version.trim().is_empty() {
        rejections.push(Rejection::new(
            RejectCode::ManifestInvalid,
            "manifest 缺少版本号",
            "version 不能为空：flow 靠它锁定实例；请在 Manifest() 里填上版本号，形如 1.0.0",
        ));
    }

    rejections.extend(validate_tools(manifest));
    rejections.extend(validate_declared_messages(manifest, index));

    rejections
}

/// 工具名在插件内唯一即可——中台聚合时会加 `插件名__` 前缀，跨插件重名不会冲突。
fn validate_tools(manifest: &PluginManifest) -> Vec<Rejection> {
    let mut rejections = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for tool in &manifest.tools {
        if tool.name.is_empty() {
            rejections.push(Rejection::new(
                RejectCode::ManifestInvalid,
                "存在没有名字的 MCP 工具",
                "tools[].name 不能为空；请给这个工具起个名（只允许字母数字与 _-），\
                 用不上就把这条声明删掉",
            ));
            continue;
        }

        if !seen.insert(tool.name.as_str()) {
            rejections.push(Rejection::new(
                RejectCode::ToolConflict,
                format!("MCP 工具 {} 重复声明", tool.name),
                "同一插件内工具名必须唯一；跨插件重名由前缀区分，不需要处理\
                 ——请把重复的那条改名或删掉",
            ));
        }

        if !tool
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            rejections.push(Rejection::new(
                RejectCode::ManifestInvalid,
                format!("MCP 工具名 {} 含非法字符", tool.name),
                "工具名只允许字母数字与 _-，它要拼进 MCP 的工具标识",
            ));
        }
    }

    rejections
}

/// 声明的消费/生产消息类型必须真的在自己的 descriptor 里。
///
/// 不查这一条的话，编排时会按一个根本不存在的类型去连线。
///
/// **例外：`google.protobuf.*` 这类 well-known 类型**。它们是平台的一部分而不是插件的
/// 契约——插件声明 `consumes: ["google.protobuf.Struct"]`（直接调用的 JSON 载荷，
/// 见 `hub_proto::json`）时，没有理由要求它把 struct.proto 也打进自己的 descriptor。
fn validate_declared_messages(manifest: &PluginManifest, index: &ContractIndex) -> Vec<Rejection> {
    let mut rejections = Vec::new();

    let produces: Vec<String> = manifest
        .produces
        .iter()
        .map(|m| m.fq_name.clone())
        .collect();
    let consumes: Vec<String> = manifest
        .consumes
        .iter()
        .map(|m| m.fq_name.clone())
        .collect();

    for (label, declared) in [("produces", &produces), ("consumes", &consumes)] {
        let missing: Vec<String> = undeclared_messages(index, declared)
            .into_iter()
            .filter(|fq| !is_well_known(fq))
            .collect();

        for fq in missing {
            rejections.push(Rejection::new(
                RejectCode::ManifestInvalid,
                format!("manifest 声明的 {label} 类型 {fq} 在 descriptor 中不存在"),
                // 两边都可能是不对的，但插件方改哪边都行——把两条路都指出来，
                // 免得他去猜是中台漏了还是自己多写了。
                "manifest 的自述必须与提交的 proto 一致；要么在 proto 里定义这个类型\
                 并重新生成提交的 descriptor，要么把这条声明从 manifest 里删掉",
            ));
        }
    }

    rejections
}

/// well-known 类型由 protobuf 平台定义，不要求出现在插件自己的 descriptor 里。
fn is_well_known(fq_name: &str) -> bool {
    fq_name.starts_with("google.protobuf.")
}

/// 给 proto 的 `Rejection` 补个便捷构造。
trait RejectionExt {
    fn new(code: RejectCode, message: impl Into<String>, detail: impl Into<String>) -> Self;
}

impl RejectionExt for Rejection {
    fn new(code: RejectCode, message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code as i32,
            message: message.into(),
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_proto::v1::{MessageContract, ToolDecl};
    use prost::Message as _;
    use prost_types::field_descriptor_proto::Type;
    use prost_types::{
        DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
    };

    fn index_with(package: &str, messages: &[&str]) -> ContractIndex {
        let set = FileDescriptorSet {
            file: vec![FileDescriptorProto {
                name: Some("t.proto".to_string()),
                package: Some(package.to_string()),
                message_type: messages
                    .iter()
                    .map(|m| DescriptorProto {
                        name: Some((*m).to_string()),
                        field: vec![FieldDescriptorProto {
                            name: Some("x".to_string()),
                            number: Some(1),
                            label: Some(
                                prost_types::field_descriptor_proto::Label::Optional as i32,
                            ),
                            r#type: Some(Type::String as i32),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
        };
        ContractIndex::from_set(&set)
    }

    fn manifest(name: &str, version: &str) -> PluginManifest {
        PluginManifest {
            name: name.to_string(),
            version: version.to_string(),
            ..Default::default()
        }
    }

    fn contract(fq: &str) -> MessageContract {
        MessageContract {
            fq_name: fq.to_string(),
            description: String::new(),
        }
    }

    #[test]
    fn 合法插件名通过() {
        for ok in ["wms-reader", "wms_reader", "a", "A1"] {
            assert!(is_valid_plugin_name(ok), "{ok} 应合法");
        }
    }

    #[test]
    fn 非法插件名被拒() {
        assert!(!is_valid_plugin_name(""));
        assert!(!is_valid_plugin_name("-lead"), "不能以连字符开头");
        assert!(!is_valid_plugin_name("has space"));
        assert!(!is_valid_plugin_name("中文名"));
        assert!(!is_valid_plugin_name(&"a".repeat(65)), "超长");
        assert!(is_valid_plugin_name(&"a".repeat(64)), "恰好在边界上");
    }

    #[test]
    fn 完整合法的_manifest_无拒绝() {
        let index = index_with("wms.v1", &["OrderCreated"]);
        let mut m = manifest("wms-reader", "1.0.0");
        m.produces = vec![contract("wms.v1.OrderCreated")];
        m.tools = vec![ToolDecl {
            name: "query_order".to_string(),
            description: "查单".to_string(),
            input_schema_json: "{}".to_string(),
            requires_approval: false,
        }];
        assert!(validate_manifest(&m, &index).is_empty());
    }

    #[test]
    fn 缺少名字与版本都会被指出() {
        let index = index_with("p", &[]);
        let rejections = validate_manifest(&manifest("", "  "), &index);
        assert_eq!(rejections.len(), 2);
        assert!(
            rejections
                .iter()
                .all(|r| r.code == RejectCode::ManifestInvalid as i32)
        );
    }

    #[test]
    fn 同一插件内工具重名被拒() {
        let index = index_with("p", &[]);
        let mut m = manifest("p", "1.0.0");
        let tool = ToolDecl {
            name: "dup".to_string(),
            ..Default::default()
        };
        m.tools = vec![tool.clone(), tool];

        let rejections = validate_manifest(&m, &index);
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].code, RejectCode::ToolConflict as i32);
    }

    #[test]
    fn 工具名含非法字符被拒() {
        let index = index_with("p", &[]);
        let mut m = manifest("p", "1.0.0");
        m.tools = vec![ToolDecl {
            name: "查询订单".to_string(),
            ..Default::default()
        }];
        let rejections = validate_manifest(&m, &index);
        assert_eq!(rejections.len(), 1);
        assert!(rejections[0].message.contains("非法字符"));
    }

    #[test]
    fn 声明了_descriptor_里没有的消息类型被拒() {
        let index = index_with("wms.v1", &["OrderCreated"]);
        let mut m = manifest("p", "1.0.0");
        m.produces = vec![contract("wms.v1.OrderCreated"), contract("wms.v1.不存在")];
        m.consumes = vec![contract("other.v1.也不存在")];

        let rejections = validate_manifest(&m, &index);
        assert_eq!(rejections.len(), 2);
        let text = rejections
            .iter()
            .map(|r| r.message.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(text.contains("不存在"));
        assert!(text.contains("produces"));
        assert!(text.contains("consumes"));
    }

    #[test]
    fn well_known_类型不要求在_descriptor_里() {
        let index = index_with("p", &[]);
        let mut m = manifest("p", "1.0.0");
        // 直接调用的载荷就是 Struct，见 hub_proto::json
        m.consumes = vec![contract("google.protobuf.Struct")];

        assert!(
            validate_manifest(&m, &index).is_empty(),
            "well-known 类型属于平台，不该要求插件把 struct.proto 也打进自己的 descriptor"
        );
    }

    #[test]
    fn 豁免只覆盖_google_protobuf_前缀() {
        let index = index_with("p", &[]);
        let mut m = manifest("p", "1.0.0");
        // google.type.* 不是 protobuf 核心的 well-known 类型，仍需出现在自己的 descriptor 里
        m.consumes = vec![contract("google.type.Money")];

        assert_eq!(
            validate_manifest(&m, &index).len(),
            1,
            "豁免范围不能扩大到整个 google.*"
        );
    }

    #[test]
    fn 拒绝信息带可选编解码能力() {
        let index = index_with("p", &[]);
        let rejections = validate_manifest(&manifest("", ""), &index);
        // 回给插件方时要走 proto 编码，确认类型可用
        let bytes = rejections[0].encode_to_vec();
        assert!(!bytes.is_empty());
    }
}
