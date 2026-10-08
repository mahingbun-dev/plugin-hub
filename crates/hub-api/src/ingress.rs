//! 业务数据入口：`POST /ingress/{plugin}`。
//!
//! 这个面要 `hub:invoke` 权限位（见 `crate::authz`）——**这里改过一次**：原设计是
//! 「不鉴权，鉴权由插件承担」，但只要有一个插件要求「每次调用可归因到具体的人」，
//! 那条设计就撑不住。中台在这里做四件事：
//! 认身份 → 装配信封 → 调插件（校验器 + 插件体）→ 把结果回给调用方。
//!
//! 载荷形态：调用方传 JSON 对象，中台包成 `google.protobuf.Struct` 放进信封的 `Any`。
//! 见 `hub_proto::json` 里对这条选择的说明。插件若返回业务类型（非 Struct），
//! 中台如实回原始字节与类型名，**不伪造成 JSON**。

use std::collections::HashMap;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use base64::Engine as _;
use chrono::Utc;
use hub_engine::InvokeOutcome;
use hub_proto::v1::{Envelope, PayloadType, Subject, SubjectKind};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::ApiState;
use crate::authz::AuthenticatedSubject;
use crate::error::ApiError;

/// 从请求头取 trace：调用方带了 `traceparent` 就沿用它的 trace-id，否则新生成一条。
fn trace_id_from(headers: &HeaderMap) -> String {
    headers
        .get(crate::trace::TRACEPARENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(crate::trace::parse_traceparent)
        .map_or_else(crate::trace::new_trace_id, |ctx| ctx.trace_id)
}

/// 调用方未指定超时时的默认预算
const DEFAULT_TIMEOUT_MS: i64 = 30_000;

/// 超时上限。再长就不该走同步调用了，应该编排成异步 flow。
const MAX_TIMEOUT_MS: i64 = 300_000;

#[derive(Debug, Deserialize)]
pub struct IngressRequest {
    /// 业务载荷，必须是 JSON 对象
    pub payload: serde_json::Value,

    /// 目标插件版本；缺省取最新版本
    #[serde(default)]
    pub version: Option<String>,

    /// 调用方指定的消息 id。
    ///
    /// 它是幂等键（总线是 at-least-once），调用方必须能自己指定才能在重试时去重；
    /// 缺省由中台生成 ULID。
    #[serde(default)]
    pub message_id: Option<String>,

    /// 附加到信封 `meta` 的键值
    #[serde(default)]
    pub meta: HashMap<String, String>,

    /// 本次调用的超时预算（毫秒）
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct IngressResponse {
    pub message_id: String,
    pub trace_id: String,

    /// 回给调用方的 W3C traceparent——把它带到下一个系统，链路就串得起来
    pub traceparent: String,

    pub plugin: String,
    pub version: String,
    pub instance_id: String,
    pub elapsed_ms: u64,

    /// 插件返回 Struct 载荷时给出 JSON
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,

    /// 插件返回业务类型时给出它的全限定名与原始字节（base64）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_type_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_base64: Option<String>,

    /// 载荷超过内联上限时，这里给出它在引用通道里的地址。
    ///
    /// **必须回给调用方**：不回的话，调用方会以为插件原样收到了它的数据，而实际上
    /// 插件拿到的是一个 uri。这个差别在出问题的时候才被发现，那就太晚了。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_ref: Option<crate::payload::PayloadRefInfo>,
}

pub async fn ingress(
    State(state): State<ApiState>,
    Path(plugin): Path<String>,
    headers: HeaderMap,
    // 配了 `HUB_AUTH_PLUGIN` 时由鉴权中间件放进请求扩展；没配就是 None。
    // 用 `Option` 而不是必需：未配鉴权是过渡期的正常形态，不是错误。
    authenticated: Option<Extension<Arc<AuthenticatedSubject>>>,
    Json(request): Json<IngressRequest>,
) -> Result<Json<IngressResponse>, ApiError> {
    let timeout_ms = resolve_timeout(request.timeout_ms)?;

    let payload = hub_proto::encode_payload(&request.payload)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    let message_id = request
        .message_id
        .clone()
        .unwrap_or_else(|| Ulid::generate().to_string());

    // 调用方带了 traceparent 就沿用——排障时最怕「同一个请求在两个系统里是两个 id」
    let trace_id = trace_id_from(&headers);
    let traceparent = crate::trace::format_traceparent(&trace_id, &crate::trace::new_span_id());

    let mut envelope = Envelope {
        message_id: message_id.clone(),
        trace_id: trace_id.clone(),
        deadline_ms: Utc::now().timestamp_millis() + timeout_ms,
        r#type: PayloadType::Request as i32,
        meta: request.meta.clone(),
        // 调用主体。`envelope.proto` 说「subject 由鉴权插件填充，中台只负责透传、
        // 审计与限流」——这里就是那个填充点：身份来自中间件已经验过的凭证，
        // 不是调用方自报的字段（自报的可以随便写，那正是要避免的）。
        //
        // **kind 恒为 HUMAN**：凭证是某个人的登录态，即便发起调用的是脚本或 agent，
        // 「这次调用是谁发起的」答案仍然是那个人。
        //
        // 未配鉴权时这里是空的。插件必须把空 subject 当作匿名来对待（收紧限流），
        // 不能当作「没有限制」——否则过渡期就成了绕过卡控的口子。
        subject: authenticated.map(|Extension(subject)| Subject {
            kind: SubjectKind::Human as i32,
            id: subject.user_code.clone(),
            scopes: subject.scopes.clone(),
            ..Default::default()
        }),
        ..Default::default()
    };

    // 装得下就内联，装不下走引用通道——插件拿到的信封里只有一个 uri
    let payload_ref = crate::payload::attach(&state.store, &mut envelope, payload).await?;

    let outcome = state
        .invoker
        .invoke(&plugin, request.version.as_deref(), envelope)
        .await
        .map_err(ApiError::from)?;

    match outcome {
        InvokeOutcome::Rejected { target, issues, .. } => Err(ApiError::ValidationRejected {
            plugin: target.plugin_name,
            issues: issues
                .into_iter()
                .map(|issue| crate::error::IssueDto {
                    path: issue.path,
                    message: issue.message,
                    severity: issue.severity,
                })
                .collect(),
        }),

        InvokeOutcome::Handled {
            target,
            envelope,
            elapsed_ms,
        } => {
            // 出参也可能走引用（插件自己决定），这时调用方要拿到的同样是一个 uri
            let response_ref =
                envelope
                    .payload_ref
                    .as_ref()
                    .map(|r| crate::payload::PayloadRefInfo {
                        uri: r.uri.clone(),
                        sha256: r.sha256.clone(),
                        size_bytes: r.size_bytes,
                    });

            let any = envelope.payload;
            let (payload, payload_type_url, payload_base64) = match &any {
                Some(any) => match hub_proto::decode_payload(any) {
                    Some(json) => (Some(json), None, None),
                    None => (
                        None,
                        Some(any.type_url.clone()),
                        Some(base64::engine::general_purpose::STANDARD.encode(&any.value)),
                    ),
                },
                None => (None, None, None),
            };

            Ok(Json(IngressResponse {
                message_id,
                trace_id,
                traceparent,
                plugin: target.plugin_name,
                version: target.version,
                instance_id: target.instance_id,
                elapsed_ms,
                payload,
                payload_type_url,
                payload_base64,
                payload_ref: response_ref.or(payload_ref),
            }))
        }
    }
}

fn resolve_timeout(requested: Option<i64>) -> Result<i64, ApiError> {
    let Some(ms) = requested else {
        return Ok(DEFAULT_TIMEOUT_MS);
    };
    if ms <= 0 {
        return Err(ApiError::bad_request("timeout_ms 必须为正数"));
    }
    if ms > MAX_TIMEOUT_MS {
        return Err(ApiError::bad_request(format!(
            "timeout_ms 最大 {MAX_TIMEOUT_MS}；更长的处理请编排成异步 flow"
        )));
    }
    Ok(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 未指定超时时用默认值() {
        assert_eq!(
            resolve_timeout(None).expect("应有默认值"),
            DEFAULT_TIMEOUT_MS
        );
    }

    #[test]
    fn 超时非正数被拒() {
        for bad in [0, -1] {
            assert!(resolve_timeout(Some(bad)).is_err(), "{bad} 应被拒");
        }
    }

    #[test]
    fn 超时超上限被拒并提示改走异步() {
        let err = resolve_timeout(Some(MAX_TIMEOUT_MS + 1)).expect_err("应被拒");
        let message = err.to_string();
        assert!(
            message.contains("异步"),
            "应引导到异步 flow，实际 {message}"
        );
    }

    #[test]
    fn 超时在范围内被接受() {
        assert_eq!(resolve_timeout(Some(1)).expect("应接受"), 1);
        assert_eq!(
            resolve_timeout(Some(MAX_TIMEOUT_MS)).expect("应接受"),
            MAX_TIMEOUT_MS
        );
    }
}
