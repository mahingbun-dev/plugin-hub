//! 字段级兼容性检查。
//!
//! 判定规则（与 `docs/design.md` 的表一致）：
//!
//! | 变更 | 判定 |
//! |---|---|
//! | 新增可选字段 / 新增消息 / 新增枚举值 | 放行 |
//! | 删除字段 / 改字段类型 / 改字段编号 / 改消息名或包名 | 拒绝注册 |
//! | 字段改名（编号不变） | 放行并提示 |
//!
//! 字段改名之所以不算破坏性：protobuf 的字段身份是**编号**，名字不参与二进制
//! 编码，插件之间走的是 gRPC 二进制流，改名不会让对端读错。但它会影响以 JSON
//! 暴露的面（如 MCP 工具），所以仍作为提示报出来，不做静默。

use crate::index::{ContractIndex, EnumEntry, MessageEntry};

/// 变更的严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// 破坏性：拒绝注册
    Breaking,
    /// 提示：放行，但记录下来
    Warning,
}

impl Severity {
    pub fn is_breaking(self) -> bool {
        self == Self::Breaking
    }
}

/// 变更代码，便于调用方归类与统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Code {
    MessageRemoved,
    FieldRemoved,
    FieldTypeChanged,
    FieldRenamed,
    EnumRemoved,
    EnumValueRemoved,
    EnumValueRenamed,
}

/// 一条不兼容项。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Incompatibility {
    pub severity: Severity,
    pub code: Code,

    /// 涉及的全限定名（消息或枚举）
    pub subject: String,

    /// 人话描述，可直接作为注册拒绝原因返回给插件方
    pub detail: String,
}

impl Incompatibility {
    fn breaking(code: Code, subject: &str, detail: String) -> Self {
        Self {
            severity: Severity::Breaking,
            code,
            subject: subject.to_string(),
            detail,
        }
    }

    fn warning(code: Code, subject: &str, detail: String) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            subject: subject.to_string(),
            detail,
        }
    }
}

/// 候选契约相对基线契约是否还有可用的消息（用于判断"接得上"）。
///
/// 编排时用它校验上游 `produces` 与下游 `consumes` 的全限定名能否对上。
pub fn names_match(upstream_produces: &str, downstream_consumes: &str) -> bool {
    upstream_produces == downstream_consumes
}

/// 比对两份契约，返回全部不兼容项（按严重程度、类型、涉及对象稳定排序）。
///
/// 基线里有、候选里没有的消息/枚举/字段视为被删除；候选里新增的一律放行。
pub fn check_compatibility(
    baseline: &ContractIndex,
    candidate: &ContractIndex,
) -> Vec<Incompatibility> {
    let mut found = Vec::new();

    for base_msg in baseline.messages() {
        match candidate.message(&base_msg.fq_name) {
            Some(cand_msg) => {
                found.extend(diff_message(base_msg, cand_msg));
            }
            None => found.push(Incompatibility::breaking(
                Code::MessageRemoved,
                &base_msg.fq_name,
                format!("消息 {} 已不存在", base_msg.fq_name),
            )),
        }
    }

    for base_enum in baseline.enums() {
        match candidate.enums().find(|e| e.fq_name == base_enum.fq_name) {
            Some(cand_enum) => found.extend(diff_enum(base_enum, cand_enum)),
            None => found.push(Incompatibility::breaking(
                Code::EnumRemoved,
                &base_enum.fq_name,
                format!("枚举 {} 已不存在", base_enum.fq_name),
            )),
        }
    }

    found.sort();
    found
}

/// 是否存在破坏性变更。
pub fn has_breaking(changes: &[Incompatibility]) -> bool {
    changes.iter().any(|c| c.severity.is_breaking())
}

/// 把破坏性变更汇总成一句话，用于注册拒绝原因。
pub fn summarize(changes: &[Incompatibility]) -> Option<String> {
    let breaking: Vec<&Incompatibility> = changes
        .iter()
        .filter(|c| c.severity.is_breaking())
        .collect();
    if breaking.is_empty() {
        return None;
    }
    let head = breaking
        .first()
        .map(|c| c.detail.clone())
        .unwrap_or_default();
    if breaking.len() == 1 {
        Some(head)
    } else {
        Some(format!("{}（另有 {} 处不兼容）", head, breaking.len() - 1))
    }
}

fn diff_message(baseline: &MessageEntry, candidate: &MessageEntry) -> Vec<Incompatibility> {
    let mut found = Vec::new();

    // 字段按编号配对：编号是身份，名字不是
    for base_field in &baseline.fields {
        let Some(cand_field) = candidate.field_by_number(base_field.number) else {
            found.push(Incompatibility::breaking(
                Code::FieldRemoved,
                &baseline.fq_name,
                format!(
                    "{}.{}（编号 {}，类型 {}）已被删除",
                    baseline.fq_name,
                    base_field.name,
                    base_field.number,
                    base_field.type_desc()
                ),
            ));
            continue;
        };

        if base_field.type_desc() != cand_field.type_desc() {
            found.push(Incompatibility::breaking(
                Code::FieldTypeChanged,
                &baseline.fq_name,
                format!(
                    "{} 字段 {}（编号 {}）类型由 {} 改为 {}",
                    baseline.fq_name,
                    base_field.name,
                    base_field.number,
                    base_field.type_desc(),
                    cand_field.type_desc()
                ),
            ));
            continue;
        }

        if base_field.name != cand_field.name {
            found.push(Incompatibility::warning(
                Code::FieldRenamed,
                &baseline.fq_name,
                format!(
                    "{} 编号 {} 的字段由 {} 改名为 {}（二进制兼容，但 JSON 面与 MCP 工具受影响）",
                    baseline.fq_name, base_field.number, base_field.name, cand_field.name
                ),
            ));
        }
    }

    found
}

fn diff_enum(baseline: &EnumEntry, candidate: &EnumEntry) -> Vec<Incompatibility> {
    let mut found = Vec::new();

    for (number, name) in &baseline.values {
        match candidate.values.get(number) {
            // 枚举值被删：编号是身份，名字不是
            None => found.push(Incompatibility::breaking(
                Code::EnumValueRemoved,
                &baseline.fq_name,
                format!(
                    "{} 的枚举值 {} = {} 已被删除",
                    baseline.fq_name, name, number
                ),
            )),
            Some(cand_name) if cand_name != name => found.push(Incompatibility::warning(
                Code::EnumValueRenamed,
                &baseline.fq_name,
                format!(
                    "{} 编号 {} 的枚举值由 {} 改名为 {}",
                    baseline.fq_name, number, name, cand_name
                ),
            )),
            Some(_) => {}
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use prost::Message as _;
    use prost_types::field_descriptor_proto::Type;
    use prost_types::{
        DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FileDescriptorProto,
        FileDescriptorSet,
    };

    fn index_of(package: &str, messages: Vec<DescriptorProto>) -> ContractIndex {
        ContractIndex::from_descriptor_set(&descriptor_bytes(package, messages)).expect("索引失败")
    }

    fn order_with(fields: Vec<prost_types::FieldDescriptorProto>) -> ContractIndex {
        index_of("wms.v1", vec![message("OrderCreated", fields)])
    }

    fn codes(changes: &[Incompatibility]) -> Vec<Code> {
        changes.iter().map(|c| c.code).collect()
    }

    #[test]
    fn 契约未变时无任何发现() {
        let baseline = order_with(vec![scalar("sku", 1, Type::String)]);
        let candidate = order_with(vec![scalar("sku", 1, Type::String)]);
        assert!(check_compatibility(&baseline, &candidate).is_empty());
    }

    #[test]
    fn 新增可选字段放行() {
        let baseline = order_with(vec![scalar("sku", 1, Type::String)]);
        let candidate = order_with(vec![
            scalar("sku", 1, Type::String),
            scalar("note", 2, Type::String),
        ]);
        assert!(check_compatibility(&baseline, &candidate).is_empty());
    }

    #[test]
    fn 新增消息放行() {
        let baseline = index_of("p", vec![message("A", vec![])]);
        let candidate = index_of("p", vec![message("A", vec![]), message("B", vec![])]);
        assert!(check_compatibility(&baseline, &candidate).is_empty());
    }

    #[test]
    fn 新增枚举值放行() {
        let mk = |values: &[(&str, i32)]| {
            ContractIndex::from_set(&FileDescriptorSet {
                file: vec![FileDescriptorProto {
                    name: Some("t.proto".to_string()),
                    package: Some("p".to_string()),
                    enum_type: vec![enumeration("Status", values)],
                    ..Default::default()
                }],
            })
        };
        let baseline = mk(&[("UNKNOWN", 0), ("OK", 1)]);
        let candidate = mk(&[("UNKNOWN", 0), ("OK", 1), ("FAILED", 2)]);
        assert!(check_compatibility(&baseline, &candidate).is_empty());
    }

    #[test]
    fn 删除字段被拒绝() {
        let baseline = order_with(vec![
            scalar("sku", 1, Type::String),
            scalar("qty", 2, Type::Int32),
        ]);
        let candidate = order_with(vec![scalar("sku", 1, Type::String)]);

        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(codes(&changes), vec![Code::FieldRemoved]);
        assert!(has_breaking(&changes));
        assert!(changes[0].detail.contains("qty"));
        assert!(changes[0].detail.contains("编号 2"));
    }

    #[test]
    fn 字段类型变更被拒绝() {
        let baseline = order_with(vec![scalar("qty", 1, Type::Int32)]);
        let candidate = order_with(vec![scalar("qty", 1, Type::String)]);

        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(codes(&changes), vec![Code::FieldTypeChanged]);
        assert!(changes[0].detail.contains("int32"));
        assert!(changes[0].detail.contains("string"));
    }

    #[test]
    fn 消息类型字段换类型被拒绝() {
        let baseline = index_of(
            "p",
            vec![message(
                "Holder",
                vec![typed("ref", 1, Type::Message, ".p.A")],
            )],
        );
        let candidate = index_of(
            "p",
            vec![
                message("Holder", vec![typed("ref", 1, Type::Message, ".p.B")]),
                message("A", vec![]),
                message("B", vec![]),
            ],
        );
        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(codes(&changes), vec![Code::FieldTypeChanged]);
        assert!(changes[0].detail.contains("p.A"));
        assert!(changes[0].detail.contains("p.B"));
    }

    #[test]
    fn 标量改_repeated_被拒绝() {
        let baseline = order_with(vec![scalar("sku", 1, Type::String)]);
        let candidate = order_with(vec![repeated_scalar("sku", 1, Type::String)]);
        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(codes(&changes), vec![Code::FieldTypeChanged]);
    }

    #[test]
    fn 字段改名放行但给出提示() {
        let baseline = order_with(vec![scalar("sku", 1, Type::String)]);
        let candidate = order_with(vec![scalar("itemSku", 1, Type::String)]);

        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(codes(&changes), vec![Code::FieldRenamed]);
        assert!(!has_breaking(&changes), "编号未变，二进制仍兼容");
        assert_eq!(changes[0].severity, Severity::Warning);
    }

    #[test]
    fn 字段编号变更等价于删除旧字段() {
        let baseline = order_with(vec![scalar("sku", 1, Type::String)]);
        let candidate = order_with(vec![scalar("sku", 2, Type::String)]);

        let changes = check_compatibility(&baseline, &candidate);
        assert!(has_breaking(&changes));
        assert_eq!(codes(&changes), vec![Code::FieldRemoved]);
        assert!(changes[0].detail.contains("编号 1"));
    }

    #[test]
    fn 删除消息被拒绝() {
        let baseline = index_of("p", vec![message("A", vec![]), message("B", vec![])]);
        let candidate = index_of("p", vec![message("A", vec![])]);

        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(codes(&changes), vec![Code::MessageRemoved]);
        assert_eq!(changes[0].subject, "p.B");
    }

    #[test]
    fn 消息改名等价于删除加新增并被拒绝() {
        let baseline = index_of(
            "p",
            vec![message("Order", vec![scalar("id", 1, Type::String)])],
        );
        let candidate = index_of(
            "p",
            vec![message("Purchase", vec![scalar("id", 1, Type::String)])],
        );

        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(codes(&changes), vec![Code::MessageRemoved]);
        assert_eq!(changes[0].subject, "p.Order");
    }

    #[test]
    fn 嵌套消息的破坏性变更会向下递归() {
        let baseline = index_of(
            "p",
            vec![message_with_nested(
                "Outer",
                vec![],
                vec![message("Inner", vec![scalar("x", 1, Type::Int32)])],
            )],
        );
        let candidate = index_of(
            "p",
            vec![message_with_nested(
                "Outer",
                vec![],
                vec![message("Inner", vec![])],
            )],
        );

        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(codes(&changes), vec![Code::FieldRemoved]);
        assert_eq!(changes[0].subject, "p.Outer.Inner");
    }

    #[test]
    fn 删除枚举值被拒绝() {
        let mk = |values: &[(&str, i32)]| {
            ContractIndex::from_set(&FileDescriptorSet {
                file: vec![FileDescriptorProto {
                    name: Some("t.proto".to_string()),
                    package: Some("p".to_string()),
                    enum_type: vec![enumeration("Status", values)],
                    ..Default::default()
                }],
            })
        };
        let baseline = mk(&[("UNKNOWN", 0), ("OK", 1), ("FAILED", 2)]);
        let candidate = mk(&[("UNKNOWN", 0), ("OK", 1)]);

        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(codes(&changes), vec![Code::EnumValueRemoved]);
        assert!(changes[0].detail.contains("FAILED"));
    }

    #[test]
    fn 删除枚举被拒绝() {
        let mk = |with_enum: bool| {
            ContractIndex::from_set(&FileDescriptorSet {
                file: vec![FileDescriptorProto {
                    name: Some("t.proto".to_string()),
                    package: Some("p".to_string()),
                    enum_type: if with_enum {
                        vec![enumeration("Status", &[("UNKNOWN", 0)])]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                }],
            })
        };
        let changes = check_compatibility(&mk(true), &mk(false));
        assert_eq!(codes(&changes), vec![Code::EnumRemoved]);
    }

    #[test]
    fn 枚举值改名放行但给出提示() {
        let mk = |name: &str| {
            ContractIndex::from_set(&FileDescriptorSet {
                file: vec![FileDescriptorProto {
                    name: Some("t.proto".to_string()),
                    package: Some("p".to_string()),
                    enum_type: vec![EnumDescriptorProto {
                        name: Some("Status".to_string()),
                        value: vec![EnumValueDescriptorProto {
                            name: Some(name.to_string()),
                            number: Some(0),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            })
        };
        let changes = check_compatibility(&mk("UNKNOWN"), &mk("UNDEFINED"));
        assert_eq!(codes(&changes), vec![Code::EnumValueRenamed]);
        assert!(!has_breaking(&changes));
    }

    #[test]
    fn 多处问题全部报出且顺序稳定() {
        let baseline = order_with(vec![
            scalar("sku", 1, Type::String),
            scalar("qty", 2, Type::Int32),
            scalar("note", 3, Type::String),
        ]);
        let candidate = order_with(vec![
            scalar("sku", 1, Type::Int32),   // 类型变了
            scalar("note", 3, Type::String), // qty 被删
        ]);

        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(changes.len(), 2);

        let again = check_compatibility(&baseline, &candidate);
        assert_eq!(changes, again, "排序必须稳定，否则拒绝信息会随机漂移");
    }

    #[test]
    fn 汇总信息可用于注册拒绝原因() {
        let baseline = order_with(vec![
            scalar("a", 1, Type::String),
            scalar("b", 2, Type::String),
        ]);
        let candidate = order_with(vec![scalar("a", 1, Type::String)]);

        let summary = summarize(&check_compatibility(&baseline, &candidate)).expect("应有汇总");
        assert!(summary.contains("b"));
        assert!(
            !summary.contains("另有"),
            "只有一条时不应出现计数尾巴: {summary}"
        );

        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn 多条破坏性变更时汇总带计数() {
        let baseline = order_with(vec![
            scalar("a", 1, Type::String),
            scalar("b", 2, Type::String),
            scalar("c", 3, Type::String),
        ]);
        let candidate = order_with(vec![scalar("a", 1, Type::String)]);

        let changes = check_compatibility(&baseline, &candidate);
        assert_eq!(changes.len(), 2, "b 与 c 都被删");

        let summary = summarize(&changes).expect("应有汇总");
        assert!(summary.contains("另有 1"), "两条时应带计数: {summary}");
    }

    #[test]
    fn 汇总只统计破坏性变更() {
        let baseline = order_with(vec![scalar("sku", 1, Type::String)]);
        let candidate = order_with(vec![scalar("itemSku", 1, Type::String)]);
        assert!(summarize(&check_compatibility(&baseline, &candidate)).is_none());
    }

    #[test]
    fn 上下游全限定名必须完全一致() {
        assert!(names_match("wms.v1.OrderCreated", "wms.v1.OrderCreated"));
        assert!(!names_match("wms.v1.OrderCreated", "wms.v1.OrderCreate"));
        assert!(
            !names_match("wms.v1.OrderCreated", "OrderCreated"),
            "包名不同不算同一个契约"
        );
    }

    #[test]
    fn 编解码往返后索引等价() {
        // 校验的是"从字节建索引"这条真实路径，而不是直接构造结构体
        let bytes = descriptor_bytes("p", vec![message("M", vec![scalar("x", 1, Type::Int32)])]);
        let decoded = FileDescriptorSet::decode(bytes.as_slice()).unwrap();
        let from_bytes = ContractIndex::from_descriptor_set(&bytes).unwrap();
        let from_set = ContractIndex::from_set(&decoded);
        assert_eq!(from_bytes, from_set);
    }
}
