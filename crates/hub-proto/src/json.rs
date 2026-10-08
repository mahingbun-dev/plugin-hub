//! JSON 载荷 ↔ `google.protobuf.Struct`。
//!
//! **直接调用**（agent 经 MCP、外部系统经 HTTP）的载荷走这条路：调用方传 JSON 对象，
//! 中台把它包成 `google.protobuf.Struct` 放进信封的 `Any`。
//!
//! 为什么不是按 descriptor 做类型化映射：MCP 与 HTTP 都是 JSON 协议，而 protobuf 的
//! 标准 JSON 映射有不少反直觉的规矩（字段名大小写、64 位整数必须写成字符串、枚举用名字
//! 而非值），要求插件团队同时维护「自己写的 MCP 工具 schema」与「proto 的 JSON 映射」
//! 两套一致性，成本高于收益。
//!
//! 契约系统不受影响：`google.protobuf.Struct` 本身就是个全限定消息名，插件在 manifest
//! 里声明 `consumes: ["google.protobuf.Struct"]` 即可，校验逻辑一套不变。
//!
//! **flow 内部的载荷仍是业务类型**（生产方产出的具体消息），那是 M2 编排的契约主线。

use prost::Message as _;
use prost_types::value::Kind;
use prost_types::{ListValue, Struct, Value};
use serde_json::{Map, Number, Value as Json};

/// `google.protobuf.Struct` 的标准 type_url。
pub const STRUCT_TYPE_URL: &str = "type.googleapis.com/google.protobuf.Struct";

/// `google.protobuf.Struct` 的全限定消息名（manifest 里声明 consumes 用它）。
pub const STRUCT_FQ_NAME: &str = "google.protobuf.Struct";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsonPayloadError {
    #[error("载荷不是 JSON 对象：直接调用的顶层必须是对象，实际是 {actual}")]
    NotAnObject { actual: &'static str },

    #[error("JSON 序列化失败: {0}")]
    Encode(String),
}

/// 把 JSON 对象编码成信封的载荷。
///
/// 顶层要求是对象：信封的载荷语义上是一组具名字段，数组或标量没有承载契约的位置。
pub fn encode_payload(value: &Json) -> Result<prost_types::Any, JsonPayloadError> {
    let Json::Object(map) = value else {
        return Err(JsonPayloadError::NotAnObject {
            actual: json_kind_name(value),
        });
    };

    let strukt = Struct {
        fields: map.iter().map(|(k, v)| (k.clone(), to_proto(v))).collect(),
    };

    Ok(prost_types::Any {
        type_url: STRUCT_TYPE_URL.to_string(),
        value: strukt.encode_to_vec(),
    })
}

/// 从载荷还原 JSON。
///
/// 返回 `None` 表示载荷不是 `Struct`——调用方应据此把原始字节与 `type_url` 一并回给
/// 请求方，而不是伪造成 JSON。这种情形在 flow 内部传递业务类型时是正常的。
pub fn decode_payload(any: &prost_types::Any) -> Option<Json> {
    if any.type_url != STRUCT_TYPE_URL {
        return None;
    }
    let strukt = Struct::decode(any.value.as_slice()).ok()?;
    Some(Json::Object(
        strukt
            .fields
            .into_iter()
            .map(|(k, v)| (k, from_proto(v)))
            .collect::<Map<String, Json>>(),
    ))
}

fn to_proto(value: &Json) -> Value {
    let kind = match value {
        Json::Null => Kind::NullValue(0),
        // Struct 只有 f64 一种数值类型：超出 2^53 的整数会丢精度。
        // 物流场景的大单号请用字符串承载，这一点在插件规范里要写明。
        Json::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or_default()),
        Json::String(s) => Kind::StringValue(s.clone()),
        Json::Bool(b) => Kind::BoolValue(*b),
        Json::Array(items) => Kind::ListValue(ListValue {
            values: items.iter().map(to_proto).collect(),
        }),
        Json::Object(map) => Kind::StructValue(Struct {
            fields: map.iter().map(|(k, v)| (k.clone(), to_proto(v))).collect(),
        }),
    };
    Value { kind: Some(kind) }
}

fn from_proto(value: Value) -> Json {
    match value.kind {
        Some(Kind::NullValue(_)) | None => Json::Null,
        Some(Kind::NumberValue(n)) => from_f64(n),
        Some(Kind::StringValue(s)) => Json::String(s),
        Some(Kind::BoolValue(b)) => Json::Bool(b),
        Some(Kind::ListValue(list)) => {
            Json::Array(list.values.into_iter().map(from_proto).collect())
        }
        Some(Kind::StructValue(strukt)) => Json::Object(
            strukt
                .fields
                .into_iter()
                .map(|(k, v)| (k, from_proto(v)))
                .collect(),
        ),
    }
}

/// Struct 的数值一律是 f64，但 JSON 本身不区分整数与浮点。
///
/// 整数值还原成整数，调用方写下的 `12` 才会原样回来而不是变成 `12.0`——
/// 否则插件作者会在「明明传的是整数，怎么变成小数了」上反复踩坑。
/// 超出 f64 精确表示范围（2^53）时不还原：那时的值本来就已经失真了。
fn from_f64(n: f64) -> Json {
    if !n.is_finite() {
        // JSON 表示不了 NaN / Infinity，只能落到 null
        return Json::Null;
    }
    const MAX_EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    if n.fract() == 0.0 && n.abs() <= MAX_EXACT {
        return Json::Number(Number::from(n as i64));
    }
    Number::from_f64(n).map_or(Json::Null, Json::Number)
}

fn json_kind_name(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "布尔值",
        Json::Number(_) => "数值",
        Json::String(_) => "字符串",
        Json::Array(_) => "数组",
        Json::Object(_) => "对象",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(value: Json) -> Json {
        let payload = encode_payload(&value).expect("编码失败");
        decode_payload(&payload).expect("解码失败")
    }

    #[test]
    fn 对象可无损往返() {
        let value = json!({
            "orderId": "SO-2026-0001",
            "qty": 12,
            "urgent": true,
            "note": null,
            "items": [{"sku": "A1", "qty": 2}, {"sku": "B2", "qty": 3}],
            "nested": {"deep": {"deeper": "值"}}
        });
        assert_eq!(roundtrip(value.clone()), value);
    }

    #[test]
    fn 编码出的_type_url_是_struct() {
        let payload = encode_payload(&json!({"a": 1})).expect("编码失败");
        assert_eq!(payload.type_url, STRUCT_TYPE_URL);
        assert!(!payload.value.is_empty());
    }

    #[test]
    fn 非_struct_载荷解码返回_none() {
        let any = prost_types::Any {
            type_url: "type.googleapis.com/wms.v1.OrderCreated".to_string(),
            value: vec![1, 2, 3],
        };
        assert_eq!(
            decode_payload(&any),
            None,
            "flow 内部传业务类型是正常的，不能伪装成 JSON"
        );
    }

    #[test]
    fn 顶层非对象被拒绝() {
        for value in [json!([1, 2, 3]), json!("字符串"), json!(42), json!(null)] {
            let err = encode_payload(&value).expect_err("应拒绝");
            assert!(
                matches!(err, JsonPayloadError::NotAnObject { .. }),
                "实际 {err:?}"
            );
        }
    }

    #[test]
    fn 顶层空对象可用() {
        assert_eq!(roundtrip(json!({})), json!({}));
    }

    #[test]
    fn 超出_f64_精度的大整数会丢精度() {
        // 这是 google.protobuf.Struct 的固有性质，不是实现缺陷；
        // 测试把它钉住，避免日后有人以为能承载大单号
        let big = json!(9007199254740993i64); // 2^53 + 1
        let wrapped = json!({ "id": big });
        let back = roundtrip(wrapped);
        assert_ne!(
            back["id"], big,
            "Struct 只有 f64，超出 2^53 的整数必然失真——大单号请用字符串"
        );
    }

    #[test]
    fn 整数往返后仍是整数而非小数() {
        // Struct 的数值是 f64，若不还原，调用方传的 12 会变成 12.0
        let value = json!({"qty": 12, "zero": 0, "negative": -7});
        let back = roundtrip(value.clone());
        assert_eq!(back, value);
        assert!(back["qty"].is_i64(), "应为整数，实际 {:?}", back["qty"]);
        assert!(back["zero"].is_i64());
        assert!(back["negative"].is_i64());
    }

    #[test]
    fn 真正的小数保持小数() {
        let value = json!({"ratio": 0.125, "delta": -3.5});
        let back = roundtrip(value.clone());
        assert_eq!(back, value);
        assert!(back["ratio"].is_f64());
    }

    #[test]
    fn 边界上的整数仍可还原() {
        // 2^53 恰好还在精确表示范围内
        let value = json!({"n": 9007199254740992i64});
        assert_eq!(roundtrip(value.clone()), value);
    }

    #[test]
    fn 浮点数与负数正常() {
        let value = json!({"ratio": 0.125, "delta": -3.5});
        assert_eq!(roundtrip(value.clone()), value);
    }

    #[test]
    fn 编码失败时给出可读原因() {
        let err = encode_payload(&json!([1])).expect_err("应拒绝");
        assert!(err.to_string().contains("数组"), "实际 {err}");
    }
}
