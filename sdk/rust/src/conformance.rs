//! 契约一致性自测套件（**L1**）。
//!
//! 插件接入中台之前必须跑通它。检查的都是「中台在注册期依赖、而等生产才发现代价太大」
//! 的约定：manifest 与 descriptor 自洽、至少声明一条契约、工具名合法。
//!
//! 它复现的就是中台注册期的那几条校验，所以本地先跑一遍能省一轮「推上去才发现被拒」。
//!
//! ```no_run
//! # struct MyPlugin;
//! # #[async_trait::async_trait]
//! # impl hubkit::Plugin for MyPlugin {
//! #   fn manifest(&self) -> hubkit::PluginManifest { unimplemented!() }
//! #   async fn validate(&self, _e: &hubkit::Envelope) -> Result<hubkit::ValidateResponse, hubkit::PluginError> { unimplemented!() }
//! #   async fn handle(&self, e: hubkit::Envelope) -> Result<hubkit::Envelope, hubkit::PluginError> { Ok(e) }
//! # }
//! let report = hubkit::conformance::check_local(&MyPlugin);
//! assert!(report.passed(), "{report}");
//! ```
//!
//! **它证明不了「能接入」**：L1 只看内存里的 manifest 对象与 proto，看不到你的网络地址。
//! `HUB_ADVERTISE_ADDR` 填成别人的地址它一样全绿，而插件在真实环境里永远接不进来。
//! 真正的分水岭是「中台接受注册」（看插件启动日志里的「已注册到中台」）。

use std::fmt;

use prost::Message as _;

use crate::envelope::is_well_known_fq_name;
use crate::error::HubkitError;
use crate::proto::PluginManifest;
use crate::rules::valid_plugin_name;
use crate::Plugin;

/// 一项检查的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// 检查项名，与 Go 侧的自测套件逐字相同。
    pub name: String,
    /// 是否通过。
    pub passed: bool,
    /// 补充说明。全绿时也常有内容（例如 `descriptor 可用` 会报字节数与类型数）。
    pub detail: String,
}

/// 一套检查的结果。
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// 被检查的对象，形如 `order-reader@0.1.0`。
    pub subject: String,
    /// 逐项结果，顺序固定。
    pub checks: Vec<Check>,
}

impl Report {
    /// 是否全部通过。
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// 未通过的检查。
    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    fn add(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.to_string(),
            passed,
            detail: detail.into(),
        });
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "契约一致性检查 {}", self.subject)?;
        for check in &self.checks {
            write!(
                f,
                "  {} {}",
                if check.passed { "✓" } else { "✗" },
                check.name
            )?;
            if !check.detail.is_empty() {
                write!(f, " —— {}", check.detail)?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

/// 检查插件对象自身的自洽性，不需要把它跑起来。
pub fn check_local<P: Plugin>(plugin: &P) -> Report {
    let mut report = Report::default();

    let manifest = plugin.manifest();
    report.subject = format!(
        "{}@{}",
        if manifest.name.is_empty() {
            "（未命名插件）"
        } else {
            manifest.name.as_str()
        },
        manifest.version
    );
    report.add("manifest 存在", true, "");

    let name = manifest.name.as_str();
    if name.trim().is_empty() {
        report.add("插件名合法", false, "缺少插件名 name");
    } else if !valid_plugin_name(name) {
        report.add(
            "插件名合法",
            false,
            "只允许字母数字与 -_，最长 64 字符，且以字母数字开头",
        );
    } else {
        report.add("插件名合法", true, name);
    }

    if manifest.version.trim().is_empty() {
        report.add("版本号存在", false, "缺少版本号 —— flow 靠它锁定实例");
    } else {
        report.add("版本号存在", true, manifest.version.clone());
    }

    // descriptor 是中台做字段级兼容检查的依据。
    // 空的是合法的：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 proto。
    let raw = plugin.descriptor();
    let mut messages = Vec::new();
    if raw.is_empty() {
        report.add(
            "descriptor 可用",
            true,
            "无自有 proto（只用 well-known 载荷）",
        );
    } else {
        match descriptor_messages(&raw) {
            Err(e) => {
                report.add("descriptor 可用", false, e);
                // descriptor 解析不了就没法再比对声明，直接收尾——
                // 继续比对只会报一串由同一个原因派生的假问题。
                check_tools(&mut report, &manifest);
                return report;
            }
            Ok(found) => {
                report.add(
                    "descriptor 可用",
                    true,
                    format!("{} 字节、{} 个消息类型", raw.len(), found.len()),
                );
                messages = found;
            }
        }
    }

    check_declared_messages(&mut report, &manifest, &messages);
    check_tools(&mut report, &manifest);
    report
}

/// 验证「自述与编译产物一致」。
///
/// 这是最有价值的一条：改了 proto 忘了重新生成、或者消息改名后忘了同步 manifest，
/// 都在这里被抓住，而不是等注册时被中台拒。
fn check_declared_messages(report: &mut Report, manifest: &PluginManifest, messages: &[String]) {
    let mut missing = Vec::new();
    let mut check = |direction: &str, contracts: &[crate::proto::MessageContract]| {
        for contract in contracts {
            let fq = contract.fq_name.as_str();
            if is_well_known_fq_name(fq) {
                continue;
            }
            if !messages.iter().any(|m| m == fq) {
                missing.push(format!("{direction} 里的 {fq}"));
            }
        }
    };
    check("produces", &manifest.produces);
    check("consumes", &manifest.consumes);

    if !missing.is_empty() {
        report.add(
            "声明的类型都在 descriptor 中",
            false,
            format!(
                "{} —— manifest 的自述必须与提交的 proto 一致",
                missing.join("；")
            ),
        );
        return;
    }

    if manifest.produces.is_empty() && manifest.consumes.is_empty() {
        report.add(
            "声明了契约",
            false,
            format!(
                "既没有 produces 也没有 consumes —— 至少声明一个，\
                 接受直接调用的插件请声明 {}",
                crate::envelope::STRUCT_FQ_NAME
            ),
        );
        return;
    }

    report.add(
        "声明的类型都在 descriptor 中",
        true,
        format!(
            "produces {} 个、consumes {} 个",
            manifest.produces.len(),
            manifest.consumes.len()
        ),
    );
}

fn check_tools(report: &mut Report, manifest: &PluginManifest) {
    if manifest.tools.is_empty() {
        // 不暴露工具是合法的：插件可能只参与 flow
        report.add("工具声明合法", true, "未声明工具（仅参与 flow）");
        return;
    }

    let mut seen: Vec<&str> = Vec::new();
    for tool in &manifest.tools {
        if !valid_tool_name(&tool.name) {
            report.add(
                "工具声明合法",
                false,
                format!(
                    "工具名 {:?} 含非法字符 —— 它要拼进 MCP 的工具标识",
                    tool.name
                ),
            );
            return;
        }
        if seen.contains(&tool.name.as_str()) {
            report.add(
                "工具声明合法",
                false,
                format!("工具 {} 重复声明 —— 同一插件内工具名必须唯一", tool.name),
            );
            return;
        }
        seen.push(tool.name.as_str());
    }

    report.add("工具声明合法", true, format!("{} 个工具", seen.len()));
}

/// 工具名要拼进 MCP 的工具标识，字符集比插件名更窄（不允许 `.`）。
fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| matches!(c, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-'))
}

/// 解析 `FileDescriptorSet`，返回其中的消息全限定名。
///
/// 刻意**直接遍历 descriptor 结构，而不是重建一个 descriptor pool**：后者要求
/// descriptor 自包含（能解析出所有 import），而插件提交的 descriptor 只含自己的 proto。
/// 中台侧的 Rust 实现同样只遍历不解析引用——两边必须一致，否则会出现
/// 「本地检查不过但中台接受」这种更糟的分歧。
pub fn descriptor_messages(raw: &[u8]) -> Result<Vec<String>, String> {
    let set = prost_types::FileDescriptorSet::decode(raw)
        .map_err(|e| format!("descriptor 无法解析: {e}"))?;

    let mut found = Vec::new();
    for file in &set.file {
        // prost-types 把 descriptor 里的字段一律建模成 Option（proto2 的
        // required/optional 在结构上分不出来），所以这里逐个兜底
        let package = file.package.as_deref().unwrap_or("");
        collect_messages(package, &file.message_type, &mut found);
    }
    Ok(found)
}

fn collect_messages(
    prefix: &str,
    messages: &[prost_types::DescriptorProto],
    into: &mut Vec<String>,
) {
    for message in messages {
        // map 字段会生成合成的 XxxEntry 消息，属实现细节，不算契约类型
        if message
            .options
            .as_ref()
            .and_then(|o| o.map_entry)
            .unwrap_or(false)
        {
            continue;
        }
        let name = message.name.as_deref().unwrap_or("");
        if name.is_empty() {
            continue;
        }

        let fq = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}.{name}")
        };

        into.push(fq.clone());
        collect_messages(&fq, &message.nested_type, into);
    }
}

/// 把 [`check_local`] 的结果变成 `Result`，方便在 `main` 或测试里直接 `?`。
pub fn ensure_local<P: Plugin>(plugin: &P) -> Result<Report, HubkitError> {
    let report = check_local(plugin);
    if report.passed() {
        Ok(report)
    } else {
        Err(HubkitError::Conformance(report.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{invalid, issue, valid};
    use crate::proto::{Envelope, MessageContract, ToolDecl, ValidateResponse};
    use crate::PluginError;
    use async_trait::async_trait;

    struct Demo {
        manifest: PluginManifest,
        descriptor: Vec<u8>,
    }

    #[async_trait]
    impl Plugin for Demo {
        fn manifest(&self) -> PluginManifest {
            self.manifest.clone()
        }
        fn descriptor(&self) -> Vec<u8> {
            self.descriptor.clone()
        }
        async fn validate(&self, _e: &Envelope) -> Result<ValidateResponse, PluginError> {
            Ok(ValidateResponse::default())
        }
        async fn handle(&self, e: Envelope) -> Result<Envelope, PluginError> {
            Ok(e)
        }
    }

    fn struct_plugin() -> Demo {
        Demo {
            manifest: PluginManifest {
                name: "order-reader".into(),
                version: "0.1.0".into(),
                description: "读订单".into(),
                owner: "团队".into(),
                consumes: vec![MessageContract {
                    fq_name: crate::envelope::STRUCT_FQ_NAME.into(),
                    description: String::new(),
                }],
                tools: vec![ToolDecl {
                    name: "echo".into(),
                    description: "回显".into(),
                    input_schema_json: r#"{"type":"object"}"#.into(),
                    requires_approval: false,
                }],
                ..Default::default()
            },
            descriptor: Vec::new(),
        }
    }

    #[test]
    fn 合规的_struct_插件六项全绿() {
        let report = check_local(&struct_plugin());
        assert!(report.passed(), "{report}");
        assert!(valid().valid);
        assert_eq!(report.subject, "order-reader@0.1.0");
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "manifest 存在",
                "插件名合法",
                "版本号存在",
                "descriptor 可用",
                "声明的类型都在 descriptor 中",
                "工具声明合法",
            ]
        );
    }

    #[test]
    fn 一条契约都不声明会被拦下() {
        let mut plugin = struct_plugin();
        plugin.manifest.consumes.clear();
        let report = check_local(&plugin);
        assert!(!report.passed());
        assert!(
            report
                .failures()
                .iter()
                .any(|c| c.name == "声明了契约" && c.detail.contains("google.protobuf.Struct")),
            "{report}"
        );
    }

    #[test]
    fn 工具名带点会被拦下() {
        let mut plugin = struct_plugin();
        plugin.manifest.tools[0].name = "a.b".into();
        let report = check_local(&plugin);
        assert!(!report.passed());
        assert!(report.failures().iter().any(|c| c.name == "工具声明合法"));
    }

    #[test]
    fn 工具重名会被拦下() {
        let mut plugin = struct_plugin();
        let tool = plugin.manifest.tools[0].clone();
        plugin.manifest.tools.push(tool);
        let report = check_local(&plugin);
        assert!(!report.passed());
        assert!(report.failures().iter().any(|c| c.name == "工具声明合法"));
    }

    #[test]
    fn 声明了自有类型而_proto_里没有会被拦下() {
        let mut plugin = struct_plugin();
        plugin.manifest.produces = vec![MessageContract {
            fq_name: "wms.v1.Order".into(),
            description: String::new(),
        }];
        let report = check_local(&plugin);
        assert!(!report.passed());
        let failure = report
            .failures()
            .into_iter()
            .find(|c| c.name == "声明的类型都在 descriptor 中")
            .expect("应当报「声明的类型都在 descriptor 中」");
        assert!(failure.detail.contains("wms.v1.Order"), "{failure:?}");
    }

    #[test]
    fn descriptor_解析不了时不再继续比对() {
        let mut plugin = struct_plugin();
        plugin.descriptor = vec![0xff, 0xff, 0xff];
        let report = check_local(&plugin);
        assert!(!report.passed());
        assert!(report
            .failures()
            .iter()
            .any(|c| c.name == "descriptor 可用"));
    }

    #[test]
    fn 生成的_descriptor_能被解析出消息名() {
        // 手工拼一个最小 FileDescriptorSet：package demo.v1，一个消息 Order，内含一个
        // 嵌套消息 Item 与一个 map 字段（map entry 必须被跳过）。
        let set = prost_types::FileDescriptorSet {
            file: vec![prost_types::FileDescriptorProto {
                name: Some("demo.proto".into()),
                package: Some("demo.v1".into()),
                message_type: vec![prost_types::DescriptorProto {
                    name: Some("Order".into()),
                    nested_type: vec![
                        prost_types::DescriptorProto {
                            name: Some("Item".into()),
                            ..Default::default()
                        },
                        prost_types::DescriptorProto {
                            name: Some("LabelsEntry".into()),
                            options: Some(prost_types::MessageOptions {
                                map_entry: Some(true),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let raw = set.encode_to_vec();

        let found = descriptor_messages(&raw).unwrap();
        assert!(found.contains(&"demo.v1.Order".to_string()), "{found:?}");
        assert!(
            found.contains(&"demo.v1.Order.Item".to_string()),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|m| m.contains("LabelsEntry")),
            "map entry 是实现细节，不该算契约类型：{found:?}"
        );
    }

    #[test]
    fn ensure_local_失败时把报告塞进错误() {
        let mut plugin = struct_plugin();
        plugin.manifest.name = "-坏名字".into();
        let err = ensure_local(&plugin).unwrap_err();
        assert!(err.to_string().contains("契约一致性检查"), "{err}");
    }

    #[test]
    fn 辅助构造出来的校验响应形状正确() {
        let bad = invalid(vec![issue("payload.text", "缺字段")]);
        assert!(!bad.valid);
        assert_eq!(bad.issues[0].path, "payload.text");
        assert_eq!(bad.issues[0].severity, crate::proto::Severity::Error as i32);
    }
}
