//! 从插件提交的 `FileDescriptorSet` 建立契约索引。
//!
//! 索引把 descriptor 摊平成「全限定名 → 字段列表 / 枚举值」，供兼容性检查
//! 与编排校验使用。protobuf 的**字段身份是编号而非名字**，所以字段按编号索引。

use std::collections::BTreeMap;

use prost::Message as _;
use prost_types::field_descriptor_proto::Type;
use prost_types::{DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorSet};

/// 建立索引时的失败原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    #[error("descriptor 解析失败: {0}")]
    Decode(String),
}

/// 一个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldEntry {
    /// 字段编号。protobuf 的身份标识是编号，改名不影响二进制兼容。
    pub number: i32,

    pub name: String,

    /// `FieldDescriptorProto::Type` 的枚举值
    pub type_code: i32,

    /// 消息/枚举字段的类型全限定名（已去掉前导点）；标量字段为空
    pub type_name: String,

    pub repeated: bool,
}

impl FieldEntry {
    /// 人类可读的类型描述，用于拒绝原因里说清"从什么变成了什么"
    pub fn type_desc(&self) -> String {
        let base = if self.type_name.is_empty() {
            scalar_type_name(self.type_code)
        } else {
            self.type_name.clone()
        };
        if self.repeated {
            format!("repeated {base}")
        } else {
            base
        }
    }
}

/// 一个消息类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEntry {
    pub fq_name: String,

    /// 按编号升序
    pub fields: Vec<FieldEntry>,
}

impl MessageEntry {
    pub fn field_by_number(&self, number: i32) -> Option<&FieldEntry> {
        self.fields.iter().find(|f| f.number == number)
    }
}

/// 一个枚举类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumEntry {
    pub fq_name: String,

    /// 编号 → 名称
    pub values: BTreeMap<i32, String>,
}

/// 契约索引。消息与枚举都按全限定名索引。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContractIndex {
    messages: BTreeMap<String, MessageEntry>,
    enums: BTreeMap<String, EnumEntry>,
}

impl ContractIndex {
    /// 从插件注册时提交的 descriptor 字节建立索引。
    ///
    /// **空 descriptor 是合法的**：只用 `google.protobuf.Struct` 承载 JSON 载荷的插件
    /// （也就是直接响应 agent 调用那一类）根本没有自己的 proto。它的契约由 manifest 里
    /// 声明的类型承担，而「声明的类型必须存在于 descriptor」这条检查仍然拦得住乱声明。
    pub fn from_descriptor_set(bytes: &[u8]) -> Result<Self, ContractError> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        let set = FileDescriptorSet::decode(bytes)
            .map_err(|err| ContractError::Decode(err.to_string()))?;
        Ok(Self::from_set(&set))
    }

    pub fn from_set(set: &FileDescriptorSet) -> Self {
        let mut index = Self::default();
        for file in &set.file {
            let package = file.package.as_deref().unwrap_or_default();
            index.add_messages(package, &file.message_type);
            index.add_enums(package, &file.enum_type);
        }
        index
    }

    pub fn message(&self, fq_name: &str) -> Option<&MessageEntry> {
        self.messages.get(fq_name)
    }

    /// manifest 声明的 produces/consumes 必须能在自己的 descriptor 里找到，
    /// 否则说明插件自述与实际契约不符。
    pub fn contains_message(&self, fq_name: &str) -> bool {
        self.messages.contains_key(fq_name)
    }

    pub fn messages(&self) -> impl Iterator<Item = &MessageEntry> {
        self.messages.values()
    }

    pub fn enums(&self) -> impl Iterator<Item = &EnumEntry> {
        self.enums.values()
    }

    pub fn message_names(&self) -> impl Iterator<Item = &str> {
        self.messages.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.enums.is_empty()
    }

    fn add_messages(&mut self, prefix: &str, messages: &[DescriptorProto]) {
        for msg in messages {
            let name = msg.name.as_deref().unwrap_or_default();
            if name.is_empty() {
                continue;
            }

            let fq_name = join(prefix, name);

            // map 字段会生成合成的 XxxEntry 消息，属实现细节，不该被当成契约类型
            if is_map_entry(msg) {
                continue;
            }

            let mut fields: Vec<FieldEntry> = msg.field.iter().map(field_entry).collect();
            fields.sort_by_key(|f| f.number);
            self.messages.insert(
                fq_name.clone(),
                MessageEntry {
                    fq_name: fq_name.clone(),
                    fields,
                },
            );

            // 嵌套类型的全限定名是 Outmost.Inner，继续向下递归
            self.add_messages(&fq_name, &msg.nested_type);
            self.add_enums(&fq_name, &msg.enum_type);
        }
    }

    fn add_enums(&mut self, prefix: &str, enums: &[EnumDescriptorProto]) {
        for en in enums {
            let name = en.name.as_deref().unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let fq_name = join(prefix, name);
            let values = en
                .value
                .iter()
                .filter_map(|v| {
                    let n = v.name.as_deref().unwrap_or_default();
                    v.number.map(|num| (num, n.to_string()))
                })
                .collect();
            self.enums
                .insert(fq_name.clone(), EnumEntry { fq_name, values });
        }
    }
}

fn is_map_entry(msg: &DescriptorProto) -> bool {
    msg.options
        .as_ref()
        .and_then(|o| o.map_entry)
        .unwrap_or(false)
}

fn field_entry(field: &FieldDescriptorProto) -> FieldEntry {
    FieldEntry {
        number: field.number.unwrap_or_default(),
        name: field.name.as_deref().unwrap_or_default().to_string(),
        type_code: field.r#type.unwrap_or_default(),
        type_name: normalize_type_name(field.type_name.as_deref().unwrap_or_default()),
        repeated: field.label == Some(prost_types::field_descriptor_proto::Label::Repeated as i32),
    }
}

/// descriptor 里的 `type_name` 带前导点（`.wms.v1.Order`），统一去掉。
fn normalize_type_name(raw: &str) -> String {
    raw.strip_prefix('.').unwrap_or(raw).to_string()
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn scalar_type_name(code: i32) -> String {
    match Type::try_from(code) {
        Ok(Type::Double) => "double",
        Ok(Type::Float) => "float",
        Ok(Type::Int64) => "int64",
        Ok(Type::Uint64) => "uint64",
        Ok(Type::Int32) => "int32",
        Ok(Type::Fixed64) => "fixed64",
        Ok(Type::Fixed32) => "fixed32",
        Ok(Type::Bool) => "bool",
        Ok(Type::String) => "string",
        Ok(Type::Group) => "group",
        Ok(Type::Message) => "message",
        Ok(Type::Bytes) => "bytes",
        Ok(Type::Uint32) => "uint32",
        Ok(Type::Enum) => "enum",
        Ok(Type::Sfixed32) => "sfixed32",
        Ok(Type::Sfixed64) => "sfixed64",
        Ok(Type::Sint32) => "sint32",
        Ok(Type::Sint64) => "sint64",
        Err(_) => "unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn 空_descriptor_得到空索引而不是报错() {
        // 只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 proto，
        // 它的契约由 manifest 声明的类型承担
        let index = ContractIndex::from_descriptor_set(&[]).expect("空 descriptor 应被接受");
        assert!(index.is_empty());
        assert!(!index.contains_message("anything"));
    }

    #[test]
    fn 无法解析的字节被拒绝() {
        // 0xFF 不是合法的 protobuf 字段标签起始
        let err = ContractIndex::from_descriptor_set(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap_err();
        assert!(matches!(err, ContractError::Decode(_)));
    }

    #[test]
    fn 按包名限定消息名并索引字段() {
        let bytes = descriptor_bytes(
            "wms.v1",
            vec![message(
                "OrderCreated",
                vec![
                    scalar("sku", 1, Type::String),
                    scalar("qty", 2, Type::Int32),
                ],
            )],
        );
        let index = ContractIndex::from_descriptor_set(&bytes).expect("索引失败");

        assert!(index.contains_message("wms.v1.OrderCreated"));
        assert!(!index.contains_message("OrderCreated"), "必须带包名");

        let msg = index.message("wms.v1.OrderCreated").expect("消息缺失");
        assert_eq!(msg.fields.len(), 2);
        assert_eq!(msg.fields[0].name, "sku");
        assert_eq!(msg.fields[0].type_desc(), "string");
        assert_eq!(msg.field_by_number(2).map(|f| f.name.as_str()), Some("qty"));
    }

    #[test]
    fn 字段按编号升序排列() {
        let bytes = descriptor_bytes(
            "p",
            vec![message(
                "M",
                vec![
                    scalar("c", 3, Type::String),
                    scalar("a", 1, Type::String),
                    scalar("b", 2, Type::String),
                ],
            )],
        );
        let index = ContractIndex::from_descriptor_set(&bytes).unwrap();
        let nums: Vec<i32> = index
            .message("p.M")
            .unwrap()
            .fields
            .iter()
            .map(|f| f.number)
            .collect();
        assert_eq!(nums, vec![1, 2, 3]);
    }

    #[test]
    fn 嵌套消息使用点号全限定名() {
        let bytes = descriptor_bytes(
            "p",
            vec![message_with_nested(
                "Outer",
                vec![],
                vec![message("Inner", vec![scalar("x", 1, Type::Int32)])],
            )],
        );
        let index = ContractIndex::from_descriptor_set(&bytes).unwrap();
        assert!(index.contains_message("p.Outer"));
        assert!(index.contains_message("p.Outer.Inner"));
    }

    #[test]
    fn 类型字段去掉前导点() {
        let bytes = descriptor_bytes(
            "p",
            vec![message(
                "Holder",
                vec![typed("order", 1, Type::Message, ".p.Order")],
            )],
        );
        let index = ContractIndex::from_descriptor_set(&bytes).unwrap();
        let f = &index.message("p.Holder").unwrap().fields[0];
        assert_eq!(f.type_name, "p.Order");
        assert_eq!(f.type_desc(), "p.Order");
    }

    #[test]
    fn 枚举被索引且可按编号查询() {
        use prost::Message as _;
        use prost_types::{DescriptorProto, FileDescriptorProto};

        let set = FileDescriptorSet {
            file: vec![FileDescriptorProto {
                name: Some("t.proto".to_string()),
                package: Some("p".to_string()),
                enum_type: vec![enumeration("Status", &[("UNKNOWN", 0), ("OK", 1)])],
                message_type: vec![DescriptorProto::default()],
                ..Default::default()
            }],
        };
        let index = ContractIndex::from_set(&set);
        let en = index.enums().next().expect("枚举缺失");
        assert_eq!(en.fq_name, "p.Status");
        assert_eq!(en.values.get(&1).map(String::as_str), Some("OK"));
        let _ = set.encode_to_vec();
    }

    #[test]
    fn 合成的_map_entry_消息不入索引() {
        use prost::Message as _;
        use prost_types::{
            DescriptorProto, FieldDescriptorProto, FileDescriptorProto, MessageOptions,
        };

        let entry = DescriptorProto {
            name: Some("ItemsEntry".to_string()),
            field: vec![
                scalar("key", 1, Type::String),
                scalar("value", 2, Type::String),
            ],
            options: Some(MessageOptions {
                map_entry: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let set = FileDescriptorSet {
            file: vec![FileDescriptorProto {
                name: Some("t.proto".to_string()),
                package: Some("p".to_string()),
                message_type: vec![DescriptorProto {
                    name: Some("Order".to_string()),
                    field: vec![FieldDescriptorProto {
                        name: Some("items".to_string()),
                        number: Some(1),
                        label: Some(prost_types::field_descriptor_proto::Label::Repeated as i32),
                        r#type: Some(Type::Message as i32),
                        type_name: Some(".p.Order.ItemsEntry".to_string()),
                        ..Default::default()
                    }],
                    nested_type: vec![entry],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let index = ContractIndex::from_set(&set);
        assert!(index.contains_message("p.Order"));
        assert!(
            !index.contains_message("p.Order.ItemsEntry"),
            "map 合成类型是实现细节，不应作为契约类型"
        );
        let _ = set.encode_to_vec();
    }
}
