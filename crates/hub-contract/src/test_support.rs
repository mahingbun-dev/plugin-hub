//! 测试辅助：手工构造 `FileDescriptorSet`。
//!
//! 不引 protoc 生成测试数据——契约校验的规则本身就是要被验证的对象，
//! 用它自己生成的产物当输入会掩盖问题。

use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FieldDescriptorProto,
    FileDescriptorProto, FileDescriptorSet,
};

/// 标量字段（可选）
pub fn scalar(name: &str, number: i32, ty: Type) -> FieldDescriptorProto {
    FieldDescriptorProto {
        name: Some(name.to_string()),
        number: Some(number),
        label: Some(Label::Optional as i32),
        r#type: Some(ty as i32),
        type_name: None,
        ..Default::default()
    }
}

/// 消息 / 枚举类型字段
pub fn typed(name: &str, number: i32, ty: Type, type_name: &str) -> FieldDescriptorProto {
    FieldDescriptorProto {
        name: Some(name.to_string()),
        number: Some(number),
        label: Some(Label::Optional as i32),
        r#type: Some(ty as i32),
        type_name: Some(type_name.to_string()),
        ..Default::default()
    }
}

/// repeated 标量字段
pub fn repeated_scalar(name: &str, number: i32, ty: Type) -> FieldDescriptorProto {
    FieldDescriptorProto {
        label: Some(Label::Repeated as i32),
        ..scalar(name, number, ty)
    }
}

/// 消息定义
pub fn message(name: &str, fields: Vec<FieldDescriptorProto>) -> DescriptorProto {
    DescriptorProto {
        name: Some(name.to_string()),
        field: fields,
        ..Default::default()
    }
}

/// 含嵌套消息的消息定义
pub fn message_with_nested(
    name: &str,
    fields: Vec<FieldDescriptorProto>,
    nested: Vec<DescriptorProto>,
) -> DescriptorProto {
    DescriptorProto {
        nested_type: nested,
        ..message(name, fields)
    }
}

/// 枚举定义
pub fn enumeration(name: &str, values: &[(&str, i32)]) -> EnumDescriptorProto {
    EnumDescriptorProto {
        name: Some(name.to_string()),
        value: values
            .iter()
            .map(|(n, v)| EnumValueDescriptorProto {
                name: Some((*n).to_string()),
                number: Some(*v),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// 组装成可提交的 descriptor 字节（插件注册时提交的正是这个）
pub fn descriptor_bytes(package: &str, messages: Vec<DescriptorProto>) -> Vec<u8> {
    use prost::Message as _;
    let set = FileDescriptorSet {
        file: vec![FileDescriptorProto {
            name: Some("test.proto".to_string()),
            package: Some(package.to_string()),
            message_type: messages,
            ..Default::default()
        }],
    };
    set.encode_to_vec()
}
