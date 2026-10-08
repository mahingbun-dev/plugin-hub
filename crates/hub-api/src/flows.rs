//! 编排的 HTTP 面：草稿、发布、触发与执行记录。
//!
//! **发布刻意只在这里暴露，不进 MCP 工具面**：发布直接改变生产流量走向，
//! 按设计 agent 只能改草稿、不能发布（见 docs/design.md 的 agent 权限决策）。
//! 把边界划在接口层面，比靠约定可靠。

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use chrono::Utc;
use hub_engine::TriggerOutcome;
use hub_flow::{FlowDefinition, FlowIssue};
use hub_proto::v1::{Envelope, PayloadType};
use hub_store::flows::FlowSummaryRow;
use hub_store::model::{FlowRevisionRow, RunNodeRow, RunRow};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::ApiState;
use crate::error::ApiError;

/// 触发时未指定超时的默认预算
const DEFAULT_TIMEOUT_MS: i64 = 30_000;

/// 超时上限。更长的处理应当编排成异步 flow（M3）。
const MAX_TIMEOUT_MS: i64 = 300_000;

// ---------------------------------------------------------------- 读

pub async fn list_flows(
    State(state): State<ApiState>,
) -> Result<Json<Vec<FlowSummaryRow>>, ApiError> {
    let rows = hub_store::flows::list_flows(state.store.pool()).await?;
    Ok(Json(rows))
}

/// 一条 flow 的全貌：草稿、当前已发布的版本、以及历史修订。
///
/// 控制台的编排页与排障都靠它——「现在跑的是哪一版、草稿改了什么」。
pub async fn get_flow(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<Json<FlowDetailResponse>, ApiError> {
    let flow = hub_store::flows::find_flow(state.store.pool(), &name)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("flow {name} 未定义")))?;

    let draft = hub_store::flows::find_draft(state.store.pool(), flow.id).await?;
    let published = hub_store::flows::find_published(state.store.pool(), flow.id).await?;
    let revisions = hub_store::flows::list_revisions(state.store.pool(), flow.id).await?;

    Ok(Json(FlowDetailResponse {
        name: flow.name,
        description: flow.description,
        published_revision: flow.published_revision,
        draft,
        published,
        revisions,
    }))
}

#[derive(Debug, Serialize)]
pub struct FlowDetailResponse {
    pub name: String,
    pub description: String,
    pub published_revision: i32,
    pub draft: Option<FlowRevisionRow>,
    pub published: Option<FlowRevisionRow>,
    pub revisions: Vec<FlowRevisionRow>,
}

pub async fn list_runs(
    State(state): State<ApiState>,
    Query(query): Query<RunsQuery>,
) -> Result<Json<Vec<RunRow>>, ApiError> {
    let rows =
        hub_store::runs::list_runs(state.store.pool(), query.flow.as_deref(), query.limit).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    #[serde(default)]
    pub flow: Option<String>,
    #[serde(default = "default_run_limit")]
    pub limit: i64,
}

fn default_run_limit() -> i64 {
    100
}

/// 一次执行的全貌：状态、耗时、以及每个节点的明细。
///
/// 这是回答「这条数据卡在哪一跳」的地方。
pub async fn get_run(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Json<RunDetailResponse>, ApiError> {
    let detail = hub_store::runs::find_run_detail(state.store.pool(), &run_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("执行记录 {run_id} 不存在")))?;

    Ok(Json(RunDetailResponse {
        run: detail.run,
        nodes: detail.nodes,
    }))
}

#[derive(Debug, Serialize)]
pub struct RunDetailResponse {
    #[serde(flatten)]
    pub run: RunRow,
    pub nodes: Vec<RunNodeRow>,
}

/// 最近若干条调用链（按 trace 聚合，不是按 span 铺开）。
///
/// 按 trace 聚合是刻意的：一次 3 节点的执行会产生 4 个 span，
/// 铺开的话列表里一次执行要占 4 行。
pub async fn list_traces(
    State(state): State<ApiState>,
    Query(query): Query<TracesQuery>,
) -> Result<Json<Vec<hub_store::spans::TraceSummaryRow>>, ApiError> {
    let rows = hub_store::spans::list_recent_traces(state.store.pool(), query.limit).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
pub struct TracesQuery {
    #[serde(default = "default_run_limit")]
    pub limit: i64,
}

/// 一条调用链的全部 span。
///
/// **这是回答「这条数据卡在哪一跳」的地方**：按开始时间排开每个 span，
/// 同层并发时能直接看出哪几个节点在同时跑、谁拖慢了整条链。
pub async fn get_trace(
    State(state): State<ApiState>,
    Path(trace_id): Path<String>,
) -> Result<Json<TraceDetailResponse>, ApiError> {
    let spans = hub_store::spans::list_spans_by_trace(state.store.pool(), &trace_id).await?;
    if spans.is_empty() {
        return Err(ApiError::not_found(format!("trace {trace_id} 不存在")));
    }

    let runs = hub_store::runs::list_runs_by_trace(state.store.pool(), &trace_id).await?;

    Ok(Json(TraceDetailResponse {
        trace_id,
        spans,
        runs,
    }))
}

#[derive(Debug, Serialize)]
pub struct TraceDetailResponse {
    pub trace_id: String,
    pub spans: Vec<hub_store::model::SpanRow>,
    pub runs: Vec<RunRow>,
}

// ---------------------------------------------------------------- 写

#[derive(Debug, Deserialize)]
pub struct DraftRequest {
    #[serde(default)]
    pub description: String,

    /// 编排定义（hub-flow 的 FlowDefinition）
    pub definition: FlowDefinition,

    #[serde(default)]
    pub created_by: String,
}

#[derive(Debug, Serialize)]
pub struct DraftResponse {
    pub revision: FlowRevisionRow,

    /// 校验发现的问题。可能有警告而无错误。
    pub issues: Vec<FlowIssue>,

    /// 有阻断性问题时为 true——此时不能发布。
    ///
    /// 注意：**保存不会被拒**。编排是一步步改出来的，中途存不下来会很别扭。
    pub blocked: bool,
}

pub async fn save_draft(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Json(request): Json<DraftRequest>,
) -> Result<Json<DraftResponse>, ApiError> {
    let outcome = state
        .flows
        .save_draft(
            &name,
            &request.description,
            &request.definition,
            &request.created_by,
        )
        .await
        .map_err(map_store_error)?;

    Ok(Json(DraftResponse {
        revision: outcome.revision,
        issues: outcome.issues,
        blocked: outcome.blocked,
    }))
}

/// 发布草稿。发布前会重新校验，有阻断性问题则拒绝。
///
/// 这个端点没有对应的 MCP 工具——发布要人来做。
pub async fn publish(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<Json<FlowRevisionRow>, ApiError> {
    let revision = state.flows.publish(&name).await.map_err(map_store_error)?;
    Ok(Json(revision))
}

#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

/// 给从未发布过的 flow 改名。
///
/// 与发布同理不进 MCP 工具面：名字是身份，agent 拿着改名的自由，
/// 很容易把触发器与调用脚本留下的引用变成悬空。
pub async fn rename(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Json(request): Json<RenameRequest>,
) -> Result<Json<hub_store::flows::FlowRow>, ApiError> {
    let new_name = request.name.trim();
    if new_name.is_empty() {
        return Err(ApiError::bad_request("新名字不能为空"));
    }
    let flow = state
        .flows
        .rename_flow(&name, new_name)
        .await
        .map_err(map_store_error)?;
    Ok(Json(flow))
}

#[derive(Debug, Deserialize)]
pub struct TriggerRequest {
    /// 业务载荷，必须是 JSON 对象
    pub payload: serde_json::Value,

    /// 幂等键；缺省由中台生成
    #[serde(default)]
    pub message_id: Option<String>,

    /// 附加到信封 meta 的键值
    #[serde(default)]
    pub meta: HashMap<String, String>,

    /// 本次执行的超时预算（毫秒）
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct TriggerResponse {
    pub run_id: String,
    pub trace_id: String,

    /// 回给调用方的 W3C traceparent——把它带到下一个系统，链路就串得起来
    pub traceparent: String,

    pub flow: String,
    pub flow_revision: i32,

    /// `succeeded` / `failed` / `rejected`
    pub status: String,

    pub elapsed_ms: u64,
    pub error: Option<String>,
    pub nodes: Vec<TriggerNodeResponse>,

    /// 叶子节点的输出
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_type_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_base64: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TriggerNodeResponse {
    pub node_id: String,
    pub plugin: String,
    pub version: String,
    pub status: String,
    pub attempts: u32,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn trigger(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<TriggerRequest>,
) -> Result<Json<TriggerResponse>, ApiError> {
    let timeout_ms = resolve_timeout(request.timeout_ms)?;
    let payload = hub_proto::encode_payload(&request.payload)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    // 调用方带了 traceparent 就沿用它的 trace-id——见 ingress.rs 里的同一处理
    let trace_id = headers
        .get(crate::trace::TRACEPARENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(crate::trace::parse_traceparent)
        .map_or_else(crate::trace::new_trace_id, |ctx| ctx.trace_id);

    let mut envelope = Envelope {
        message_id: request
            .message_id
            .clone()
            .unwrap_or_else(|| Ulid::generate().to_string()),
        trace_id,
        deadline_ms: Utc::now().timestamp_millis() + timeout_ms,
        r#type: PayloadType::Request as i32,
        meta: request.meta.clone(),
        ..Default::default()
    };

    // 与 ingress 同一条边界：装得下内联，装不下走引用通道
    let _ = crate::payload::attach(&state.store, &mut envelope, payload).await?;

    let trigger = serde_json::json!({
        "kind": "http",
        "path": format!("/flows/{name}/trigger"),
    });

    let outcome = state
        .flows
        .trigger(&name, envelope, Some(trigger))
        .await
        .map_err(map_store_error)?;

    Ok(Json(into_trigger_response(&name, outcome)))
}

fn into_trigger_response(flow_name: &str, outcome: TriggerOutcome) -> TriggerResponse {
    let run = outcome.run;

    let (payload, payload_type_url, payload_base64) = match run.output.as_ref() {
        Some(envelope) => match envelope.payload.as_ref() {
            Some(any) => match hub_proto::decode_payload(any) {
                Some(json) => (Some(json), None, None),
                None => (
                    None,
                    Some(any.type_url.clone()),
                    Some(base64_of(&any.value)),
                ),
            },
            None => (None, None, None),
        },
        None => (None, None, None),
    };

    TriggerResponse {
        run_id: run.run_id,
        trace_id: run.trace_id.clone(),
        traceparent: crate::trace::format_traceparent(&run.trace_id, &crate::trace::new_span_id()),
        flow: flow_name.to_string(),
        flow_revision: outcome.flow_revision,
        status: status_name(run.status),
        elapsed_ms: run.elapsed_ms,
        error: run.error,
        nodes: run
            .nodes
            .into_iter()
            .map(|node| TriggerNodeResponse {
                node_id: node.node_id,
                plugin: node.plugin,
                version: node.version,
                status: node_status_name(node.status),
                attempts: node.attempts,
                duration_ms: node.duration_ms,
                error: node.error,
            })
            .collect(),
        payload,
        payload_type_url,
        payload_base64,
    }
}

fn status_name(status: hub_engine::RunStatus) -> String {
    match status {
        hub_engine::RunStatus::Succeeded => "succeeded",
        hub_engine::RunStatus::Failed => "failed",
        hub_engine::RunStatus::Rejected => "rejected",
    }
    .to_string()
}

fn node_status_name(status: hub_engine::NodeStatus) -> String {
    match status {
        hub_engine::NodeStatus::Succeeded => "succeeded",
        hub_engine::NodeStatus::Failed => "failed",
        hub_engine::NodeStatus::Rejected => "rejected",
    }
    .to_string()
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub(crate) fn resolve_timeout(requested: Option<i64>) -> Result<i64, ApiError> {
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

/// 把存储层的状态错误映射成 400（请求不成立）而不是 500（服务故障）。
fn map_store_error(err: hub_store::StoreError) -> ApiError {
    match err {
        hub_store::StoreError::Invalid(message) => ApiError::bad_request(message),
        other => ApiError::internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 未指定超时时用默认值() {
        assert_eq!(resolve_timeout(None).expect("应有默认"), DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn 超时非正数被拒() {
        assert!(resolve_timeout(Some(0)).is_err());
        assert!(resolve_timeout(Some(-1)).is_err());
    }

    #[test]
    fn 超时超上限被拒并引导到异步() {
        let err = resolve_timeout(Some(MAX_TIMEOUT_MS + 1)).expect_err("应被拒");
        assert!(err.to_string().contains("异步"));
    }

    #[test]
    fn 状态名称与数据库取值一致() {
        // 数据库对这两列有 CHECK 约束，映射写错会在插入时才炸
        for status in [
            hub_engine::RunStatus::Succeeded,
            hub_engine::RunStatus::Failed,
            hub_engine::RunStatus::Rejected,
        ] {
            let name = status_name(status);
            assert!(["succeeded", "failed", "rejected"].contains(&name.as_str()));
        }
    }
}
