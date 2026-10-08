//! 总线面：异步触发、死信的查看与重放。
//!
//! 与 `flows.rs` 分开是刻意的：那边是编排的**定义**（草稿、发布、修订），这边是编排
//! 的**运行痕迹**（排队中的执行、失败到头的消息）。改定义与查故障是两类人做两件事，
//! 混在一个文件里会让两边都变难读。
//!
//! ## 重放为什么要调用方给载荷
//!
//! 死信表里只有**摘要**没有全量报文——这是中台一贯的留存策略（见 `docs/design.md`）。
//! 所以重放不是「把存着的那份再发一遍」，而是「拿一份新数据，按原来的编排重跑一次」。
//!
//! 这不是妥协而是必然：真存全量报文的那份表会变成另一个数据库，而它的合规、容量、
//! 清理问题一个都不少。重放的数据由重放方提供——对 MQ 触发的死信，那通常是上游系统
//! 或运维人员手上本来就有的东西。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use hub_store::dead_letters;
use hub_store::model::DeadLetterRow;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::{ApiState, trace};

/// 默认列出多少条死信。
const DEFAULT_LIMIT: i64 = 50;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// 只看还没重放过的（默认 true）
    pub pending: Option<bool>,
    pub limit: Option<i64>,
}

/// 死信的对外形态。
#[derive(Debug, Serialize)]
pub struct DeadLetterDto {
    pub id: i64,
    pub stream: String,
    pub stream_id: String,

    pub run_id: Option<String>,
    pub flow_name: Option<String>,
    pub node_id: Option<String>,

    pub attempts: i32,
    pub error: String,

    /// 载荷摘要（类型 / 字节数 / meta 键）。
    ///
    /// 库里的列是文本，这里**解析成对象再吐出去**：控制台要按字段渲染，
    /// 让它自己去 `JSON.parse` 一个字符串是没必要的来回。
    pub payload_summary: Value,

    pub replayed_run_id: Option<String>,
    pub replayed_at: Option<DateTime<Utc>>,
    pub first_seen_at: DateTime<Utc>,
    pub last_attempt_at: DateTime<Utc>,
}

impl From<DeadLetterRow> for DeadLetterDto {
    fn from(row: DeadLetterRow) -> Self {
        Self {
            id: row.id,
            stream: row.stream,
            stream_id: row.stream_id,
            run_id: row.run_id,
            flow_name: row.flow_name,
            node_id: row.node_id,
            attempts: row.attempts,
            error: row.error,
            payload_summary: row
                .payload_summary
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or(Value::Null),
            replayed_run_id: row.replayed_run_id,
            replayed_at: row.replayed_at,
            first_seen_at: row.first_seen_at,
            last_attempt_at: row.last_attempt_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReplayRequest {
    /// 重放用的载荷。**必须由调用方提供**——死信表里只有摘要，没有全量报文。
    pub payload: Value,

    /// 可选的 meta，原样带进信封
    #[serde(default)]
    pub meta: std::collections::HashMap<String, String>,

    /// 本次重放的超时；不传用默认值
    pub timeout_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct EnqueueResponse {
    pub run_id: String,
    pub trace_id: String,
    pub traceparent: String,
}

/// `POST /flows/{flow}/trigger-async` —— 入队一次异步执行，立刻返回。
///
/// 与同步触发分开两个端点而不是加一个 `mode` 字段：两者的**响应语义完全不同**——
/// 同步返回执行结果，异步返回一个句柄。用一个端点两套响应形状，读代码的人迟早会
/// 把其中一个的返回当成另一个的。
pub async fn trigger_async(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Json(request): Json<crate::flows::TriggerRequest>,
) -> Result<(StatusCode, Json<EnqueueResponse>), ApiError> {
    let exec = state.async_exec()?;
    let timeout_ms = crate::flows::resolve_timeout(request.timeout_ms)?;
    let payload = hub_proto::encode_payload(&request.payload)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    let trace_id = trace::new_trace_id();
    let mut envelope = hub_proto::Envelope {
        message_id: request
            .message_id
            .clone()
            .unwrap_or_else(|| ulid::Ulid::generate().to_string()),
        trace_id: trace_id.clone(),
        deadline_ms: Utc::now().timestamp_millis() + timeout_ms,
        r#type: hub_proto::PayloadType::Request as i32,
        meta: request.meta.clone(),
        ..Default::default()
    };

    // 与 ingress 同一条边界。异步链上更要紧：一条 4MB 的消息躺在 Redis 里，
    // 重投几次就能把内存吃掉一大块
    crate::payload::attach(&state.store, &mut envelope, payload).await?;

    let run_id = exec
        .enqueue(
            &name,
            envelope,
            Some(json!({"kind": "http", "path": format!("/flows/{name}/trigger-async")})),
        )
        .await
        .map_err(map_async_error)?;

    Ok((
        // 202 而不是 200：这次请求**没有**跑完编排，只是把它排上了队。
        // 返回 200 会让调用方以为数据已经处理完了。
        StatusCode::ACCEPTED,
        Json(EnqueueResponse {
            run_id,
            traceparent: trace::format_traceparent(&trace_id, &trace::new_span_id()),
            trace_id,
        }),
    ))
}

/// `GET /dead-letters`
pub async fn list_dead_letters(
    State(state): State<ApiState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<DeadLetterDto>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500);
    let rows = dead_letters::list(state.store.pool(), query.pending.unwrap_or(true), limit)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?;

    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// `GET /dead-letters/{id}`
pub async fn get_dead_letter(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<Json<DeadLetterDto>, ApiError> {
    let row = dead_letters::find(state.store.pool(), id)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("死信 {id} 不存在")))?;

    Ok(Json(row.into()))
}

/// `POST /dead-letters/{id}/replay` —— 重放一条死信，产生一次新的执行。
pub async fn replay_dead_letter(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(request): Json<ReplayRequest>,
) -> Result<(StatusCode, Json<EnqueueResponse>), ApiError> {
    let exec = state.async_exec()?;
    let pool = state.store.pool();

    let letter = dead_letters::find(pool, id)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("死信 {id} 不存在")))?;

    let flow_name = letter
        .flow_name
        .clone()
        .ok_or_else(|| ApiError::bad_request("这条死信没有记下所属编排，无法重放".to_string()))?;

    // 抢占重放权。抢不到说明别人正在放——直接拒绝比产生两次执行好。
    let claimed = dead_letters::begin_replay(pool, id, Utc::now())
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?;
    if !claimed {
        return Err(ApiError::conflict(format!(
            "死信 {id} 已经重放过（{}），不能重复重放",
            letter.replayed_run_id.as_deref().unwrap_or("")
        )));
    }

    let timeout_ms = crate::flows::resolve_timeout(request.timeout_ms)?;
    let payload = hub_proto::encode_payload(&request.payload)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    let trace_id = trace::new_trace_id();

    let envelope = hub_proto::Envelope {
        // 重放**必须换一个新的 message_id**：message_id 是数据的幂等键，沿用旧的会让
        // 下游插件把这次重放当成「那条数据又来了」而按幂等跳过——重放就白做了。
        message_id: ulid::Ulid::generate().to_string(),
        trace_id: trace_id.clone(),
        deadline_ms: Utc::now().timestamp_millis() + timeout_ms,
        r#type: hub_proto::PayloadType::Request as i32,
        payload: Some(payload),
        meta: request.meta.clone(),
        ..Default::default()
    };

    let run_id = match exec
        .enqueue(
            &flow_name,
            envelope,
            Some(json!({
                "kind": "replay",
                "dead_letter_id": id,
                "original_run_id": letter.run_id,
                "original_node_id": letter.node_id,
            })),
        )
        .await
    {
        Ok(run_id) => run_id,
        Err(err) => {
            // 入队没成功就把抢占撤掉。**抢占了却没真的重放**比没抢占更糟：
            // 那条死信会显示「已重放」，而实际上什么也没发生。
            let _ = dead_letters::clear_replay(pool, id).await;
            return Err(map_async_error(err));
        }
    };

    dead_letters::set_replayed_run(pool, id, &run_id)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(EnqueueResponse {
            run_id,
            traceparent: trace::format_traceparent(&trace_id, &trace::new_span_id()),
            trace_id,
        }),
    ))
}

/// 异步链的错误映射。
///
/// 「消息/编排不成立」是 400（改请求就能解决），总线不可用是 503（等等再来），
/// 两者混成一个会让调用方对同一个状态码有两种相反的处置。
fn map_async_error(err: hub_engine::AsyncError) -> ApiError {
    match err {
        hub_engine::AsyncError::Store(hub_store::StoreError::Invalid(message)) => {
            ApiError::bad_request(message)
        }
        hub_engine::AsyncError::Invalid(message) => ApiError::bad_request(message),
        hub_engine::AsyncError::Bus(hub_bus::BusError::Overloaded { depth, limit }) => {
            ApiError::overloaded(depth, limit)
        }
        other => ApiError::internal(other.to_string()),
    }
}
