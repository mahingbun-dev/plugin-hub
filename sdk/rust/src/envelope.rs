//! 信封与载荷的读写辅助。
//!
//! 插件侧要处理的两件事：从 `Envelope` 里取业务载荷、把结果写回一个信封。
//! 直接调用（agent 经 MCP、外部系统经 HTTP）走的是 **JSON 载荷**——
//! 中台把 JSON 对象包成 `google.protobuf.Struct` 塞进信封，插件用
//! [`payload_json`] 取出来就是一个 `serde_json::Value`。

use std::time::Duration;

use prost::Message as _;
use prost_types::{value::Kind, ListValue, Struct, Value as ProstValue};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as Json};

use crate::error::HubkitError;
use crate::proto::{type_url_for, Envelope, Severity, ValidateResponse, ValidationIssue};

/// 「直接调用」载荷的类型标识。
///
/// agent 经 MCP、外部系统经 HTTP 调用插件时，中台把 JSON 对象包成
/// `google.protobuf.Struct` 放进信封；flow 内部传递的则是业务类型。
/// 插件两种都可能收到，用 [`payload_json`] 区分。
pub const STRUCT_TYPE_URL: &str = "type.googleapis.com/google.protobuf.Struct";

/// 上面那个载荷的全限定消息名。
///
/// 在 manifest 里声明 `consumes = [MessageContract { fq_name: STRUCT_FQ_NAME.into(), .. }]`
/// 表示「本插件接受直接调用的 JSON 载荷」。它是 well-known 类型，
/// 中台不要求它出现在插件自己的 descriptor 里。
pub const STRUCT_FQ_NAME: &str = "google.protobuf.Struct";

/// 判断是否属于 protobuf 平台提供的 well-known 类型。
///
/// 中台对 `google.protobuf.*` 豁免「声明必须出现在自己的 descriptor 里」这条检查；
/// 插件侧的自测套件用同一个判断，避免两边规则漂移。
pub fn is_well_known_fq_name(fq_name: &str) -> bool {
    fq_name.starts_with("google.protobuf.")
}

/// 取出信封里的 JSON 载荷。
///
/// 载荷不是 `Struct`（例如 flow 内部传的业务类型）时返回 `None`，
/// 此时插件应改为按自己的业务类型去解 `env.payload`。
///
/// **注意数值一律是 `f64`**：`google.protobuf.Struct` 只有一种数值类型。
/// 大单号这类超出 2^53 的整数请用字符串承载，别指望 JSON 数字。
pub fn payload_json(env: &Envelope) -> Option<Json> {
    let payload = env.payload.as_ref()?;
    if payload.type_url != STRUCT_TYPE_URL {
        return None;
    }
    let s = Struct::decode(payload.value.as_slice()).ok()?;
    Some(struct_to_json(&s))
}

/// 把 JSON 对象装进信封的载荷。
///
/// 返回**新信封**，原信封不被修改——链路里可能有别的持有者，
/// 就地改会让「谁在什么时候改了什么」变得不可追。
pub fn with_payload_json(env: &Envelope, payload: Json) -> Result<Envelope, HubkitError> {
    if !payload.is_object() {
        return Err(HubkitError::PayloadNotObject);
    }

    let s = json_to_struct(&payload);
    let mut value = Vec::with_capacity(s.encoded_len());
    s.encode(&mut value)
        .map_err(|e| HubkitError::EncodePayload(e.to_string()))?;

    let mut out = env.clone();
    out.payload = Some(prost_types::Any {
        type_url: STRUCT_TYPE_URL.to_string(),
        value,
    });
    out.payload_ref = None;
    Ok(out)
}

/// 把业务类型装进信封的载荷（flow 内部传递用）。
///
/// `type_url` 由消息的全限定名拼出，中台按它的末段认契约。
pub fn with_payload<M: prost::Message>(
    env: &Envelope,
    message: &M,
    fq_name: &str,
) -> Result<Envelope, HubkitError> {
    let mut value = Vec::with_capacity(message.encoded_len());
    message
        .encode(&mut value)
        .map_err(|e| HubkitError::EncodePayload(e.to_string()))?;

    let mut out = env.clone();
    out.payload = Some(prost_types::Any {
        type_url: type_url_for(fq_name),
        value,
    });
    out.payload_ref = None;
    Ok(out)
}

/// 从信封里解出业务类型。
///
/// `fq_name` 不匹配时返回 `None`——用业务类型解 JSON 载荷会得到一堆空字段，
/// 那比「明确不是这个类型」难查得多。
pub fn payload_as<M: prost::Message + Default>(env: &Envelope, fq_name: &str) -> Option<M> {
    let payload = env.payload.as_ref()?;
    let actual = crate::proto::fq_name_from_type_url(&payload.type_url)?;
    if actual != fq_name {
        return None;
    }
    M::decode(payload.value.as_slice()).ok()
}

/// 距离信封截止时间还剩多久。未设置时返回 `None`；已过期时返回 `Some(0)`。
///
/// deadline 逐跳递减：插件应据此**提前放弃**，而不是把时间耗光后让上层的超时兜底——
/// 那样调用方连「为什么慢」都看不出来。
pub fn budget(env: &Envelope) -> Option<Duration> {
    if env.deadline_ms <= 0 {
        return None;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let left = env.deadline_ms - now_ms;
    Some(if left < 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(left as u64)
    })
}

/// 信封是否已过截止时间。
pub fn expired(env: &Envelope) -> bool {
    matches!(budget(env), Some(d) if d.is_zero())
}

/// 构造「校验通过」的响应。
pub fn valid() -> ValidateResponse {
    ValidateResponse {
        valid: true,
        issues: Vec::new(),
    }
}

/// 构造「校验不通过」的响应。
///
/// 每个 issue 的 `path` 要能定位到具体字段（例如 `payload.items[2].sku`），
/// 中台会原样把它回给调用方，agent 靠它改数据重试。
pub fn invalid(issues: Vec<ValidationIssue>) -> ValidateResponse {
    ValidateResponse {
        valid: false,
        issues,
    }
}

/// 构造一条错误级校验问题。
pub fn issue(path: impl Into<String>, message: impl Into<String>) -> ValidationIssue {
    ValidationIssue {
        path: path.into(),
        message: message.into(),
        severity: Severity::Error as i32,
    }
}

/// 构造一条警告级校验问题。
///
/// 警告**不会**让校验失败——用它标记「能放行但值得记一笔」的情况。
pub fn warn_issue(path: impl Into<String>, message: impl Into<String>) -> ValidationIssue {
    ValidationIssue {
        path: path.into(),
        message: message.into(),
        severity: Severity::Warning as i32,
    }
}

// ------------------------------------------------------------ JSON ↔ Struct

/// `serde_json::Value` → `google.protobuf.Struct`。
///
/// 手写而不是找一个转换 crate：映射本身很短，而多一个依赖就多一份随 SDK 发出去的
/// 版本风险；且这里要的语义是钉死的（数值一律 f64），不需要通用库的灵活度。
pub fn json_to_struct(value: &Json) -> Struct {
    match value {
        Json::Object(map) => Struct {
            fields: map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_prost_value(v)))
                .collect(),
        },
        // 非对象值没有 Struct 能装它，包成 `{"value": ...}`。
        // 走到这里说明调用方没先检查，`with_payload_json` 会在更早的地方报错。
        other => Struct {
            fields: [("value".to_string(), json_to_prost_value(other))]
                .into_iter()
                .collect(),
        },
    }
}

fn json_to_prost_value(value: &Json) -> ProstValue {
    let kind = match value {
        Json::Null => Kind::NullValue(0),
        Json::Bool(b) => Kind::BoolValue(*b),
        Json::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        Json::String(s) => Kind::StringValue(s.clone()),
        Json::Array(items) => Kind::ListValue(ListValue {
            values: items.iter().map(json_to_prost_value).collect(),
        }),
        Json::Object(map) => Kind::StructValue(Struct {
            fields: map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_prost_value(v)))
                .collect(),
        }),
    };
    ProstValue { kind: Some(kind) }
}

/// `google.protobuf.Struct` → `serde_json::Value`。
pub fn struct_to_json(s: &Struct) -> Json {
    Json::Object(
        s.fields
            .iter()
            .map(|(k, v)| (k.clone(), prost_value_to_json(v)))
            .collect::<JsonMap<String, Json>>(),
    )
}

fn prost_value_to_json(value: &ProstValue) -> Json {
    match value.kind.as_ref() {
        None | Some(Kind::NullValue(_)) => Json::Null,
        Some(Kind::BoolValue(b)) => Json::Bool(*b),
        // Struct 只有 f64，NaN / Infinity 在 JSON 里没有对应表示，降级成 null
        // 而不是伪造一个数——伪造出来的数字会被下游当真。
        Some(Kind::NumberValue(n)) => JsonNumber::from_f64(*n)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Some(Kind::StringValue(s)) => Json::String(s.clone()),
        Some(Kind::ListValue(list)) => {
            Json::Array(list.values.iter().map(prost_value_to_json).collect())
        }
        Some(Kind::StructValue(inner)) => struct_to_json(inner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Envelope;

    #[test]
    fn json_载荷往返() {
        let env = Envelope {
            message_id: "01J0TEST".into(),
            ..Default::default()
        };
        // 数值一律写 f64：google.protobuf.Struct 只有一种数值类型，
        // 整数走一个来回会变成 3.0——这是**契约决定的**，不是精度问题。
        let payload = serde_json::json!({
            "text": "你好",
            "n": 3.0,
            "ok": true,
            "nested": {"a": [1.0, 2.0, null]},
        });

        let out = with_payload_json(&env, payload.clone()).unwrap();
        assert_eq!(out.payload.as_ref().unwrap().type_url, STRUCT_TYPE_URL);
        assert_eq!(payload_json(&out).unwrap(), payload);
        // 原信封不被修改
        assert!(env.payload.is_none());

        // 把「整数会变成浮点」这件事钉住：它是对使用者的承诺，也是个坑
        let ints = with_payload_json(&env, serde_json::json!({"n": 3})).unwrap();
        assert_eq!(payload_json(&ints).unwrap(), serde_json::json!({"n": 3.0}));
    }

    #[test]
    fn 非_json_载荷的插件收到_none() {
        let env = Envelope {
            payload: Some(prost_types::Any {
                type_url: type_url_for("wms.v1.OrderCreated"),
                value: vec![1, 2, 3],
            }),
            ..Default::default()
        };
        assert!(payload_json(&env).is_none());
    }

    #[test]
    fn 顶层不是对象时拒绝打包() {
        let env = Envelope::default();
        assert!(with_payload_json(&env, serde_json::json!([1, 2])).is_err());
        assert!(with_payload_json(&env, serde_json::json!("x")).is_err());
    }

    #[test]
    fn 预算未设置时是_none_过期时是零() {
        let mut env = Envelope::default();
        assert_eq!(budget(&env), None);
        assert!(!expired(&env));

        env.deadline_ms = 1; // 1970 年 1 毫秒，必然已过
        assert_eq!(budget(&env), Some(Duration::ZERO));
        assert!(expired(&env));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        env.deadline_ms = now + 60_000;
        assert!(budget(&env).unwrap() > Duration::from_secs(50));
        assert!(!expired(&env));
    }

    #[test]
    fn 业务类型载荷按全限定名区分() {
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct Small {
            #[prost(string, tag = "1")]
            name: String,
        }

        let env = Envelope::default();
        let out = with_payload(&env, &Small { name: "x".into() }, "demo.v1.Small").unwrap();

        let decoded: Small = payload_as(&out, "demo.v1.Small").unwrap();
        assert_eq!(decoded.name, "x");
        // 名字对不上就该是 None，而不是「解出一堆空字段」
        assert!(payload_as::<Small>(&out, "demo.v1.Other").is_none());
        assert!(payload_json(&out).is_none());
    }
}
