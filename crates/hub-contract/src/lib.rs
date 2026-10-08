//! anc-hub 契约校验层。
//!
//! 插件的契约以 `google.protobuf.FileDescriptorSet` 形式随注册提交，本 crate 负责：
//!
//! 1. **建索引**（[`ContractIndex`]）——把 descriptor 摊平成「全限定名 → 字段/枚举值」
//! 2. **字段级兼容检查**（[`check_compatibility`]）——新版本相对基线是否有破坏性变更
//! 3. **manifest 自述核对**（[`undeclared_messages`]）——声明的消息类型必须真的存在
//!
//! 契约标识是**消息的全限定名**（如 `wms.v1.OrderCreated`），编排时上下游比对的
//! 就是它；[`names_match`] 是这条规则的唯一实现处。

pub mod diff;
pub mod index;

#[cfg(test)]
mod test_support;

pub use diff::{
    Code, Incompatibility, Severity, check_compatibility, has_breaking, names_match, summarize,
};
pub use index::{ContractError, ContractIndex, EnumEntry, FieldEntry, MessageEntry};

/// 找出 manifest 声明了、但 descriptor 里并不存在的消息类型。
///
/// 返回非空即说明插件自述与实际契约不符，注册应被拒绝（`REJECT_CODE_MANIFEST_INVALID`）——
/// 否则编排时会按一个根本不存在的类型去连线。
pub fn undeclared_messages(index: &ContractIndex, declared: &[String]) -> Vec<String> {
    declared
        .iter()
        .filter(|fq| !index.contains_message(fq))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{descriptor_bytes, message, scalar};
    use prost_types::field_descriptor_proto::Type;

    #[test]
    fn 声明了不存在的消息类型会被指出() {
        let bytes = descriptor_bytes("p", vec![message("A", vec![scalar("x", 1, Type::Int32)])]);
        let index = ContractIndex::from_descriptor_set(&bytes).unwrap();

        let missing = undeclared_messages(
            &index,
            &[
                "p.A".to_string(),
                "p.Nope".to_string(),
                "q.AlsoNope".to_string(),
            ],
        );
        assert_eq!(
            missing,
            vec!["p.Nope".to_string(), "q.AlsoNope".to_string()]
        );
    }

    #[test]
    fn 全部声明都能对上时返回空() {
        let bytes = descriptor_bytes("p", vec![message("A", vec![])]);
        let index = ContractIndex::from_descriptor_set(&bytes).unwrap();
        assert!(undeclared_messages(&index, &["p.A".to_string()]).is_empty());
    }
}
