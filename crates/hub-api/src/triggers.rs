//! 触发器的 HTTP 面：cron 定时与 MQ 订阅的登记、启停与删除。
//!
//! 与 MCP 面共用同一套仓储层校验（`hub_store::triggers::upsert`），
//! 所以两个入口对「什么样的配置算合法」不会漂移。
//!
//! **登记触发器不改变编排本身**，只让它多一个被触发的入口——这也是为什么
//! 它进了 MCP 工具面，而「发布」没有。

use axum::Json;
use axum::extract::{Path, State};
use hub_store::model::TriggerRow;
use hub_store::triggers::{self, NewTrigger, TriggerView};
use serde::Deserialize;

use crate::ApiState;
use crate::error::ApiError;

/// 全部触发器（含已停用的），跨 flow。
pub async fn list_triggers(
    State(state): State<ApiState>,
) -> Result<Json<Vec<TriggerView>>, ApiError> {
    let rows = triggers::list_all(state.store.pool()).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
pub struct SaveTriggerRequest {
    /// `cron` 或 `mq`
    pub kind: String,

    /// 同名视为同一条（改而不是新增）
    #[serde(default)]
    pub name: Option<String>,

    /// cron 要 `{"expr": "0 2 * * *"}`；mq 要 `{"stream": "wms:orders"}`
    pub config: serde_json::Value,
}

/// 登记或更新一条触发器。
///
/// 重新登记会把它置回启用——登记一份配置的意图就是「让它按这个跑」。
pub async fn save_trigger(
    State(state): State<ApiState>,
    Path(flow): Path<String>,
    Json(request): Json<SaveTriggerRequest>,
) -> Result<Json<TriggerRow>, ApiError> {
    let row = triggers::upsert(
        state.store.pool(),
        &NewTrigger {
            flow_name: &flow,
            kind: &request.kind,
            name: request.name.as_deref().unwrap_or("default"),
            config: &request.config,
        },
    )
    .await
    .map_err(map_store_error)?;

    Ok(Json(row))
}

#[derive(Debug, Deserialize)]
pub struct EnabledRequest {
    pub enabled: bool,
}

/// 启用 / 停用一条触发器。
pub async fn set_enabled(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(request): Json<EnabledRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let changed = triggers::set_enabled(state.store.pool(), id, request.enabled).await?;
    if !changed {
        return Err(ApiError::not_found(format!("触发器 {id} 不存在")));
    }

    // 返回新的状态而不是空体：调用方要能确认「我点的那一下生效了哪个值」，
    // 尤其在这个动作是可被重复点击的开关上
    Ok(Json(serde_json::json!({
        "id": id,
        "enabled": request.enabled,
    })))
}

/// 删除一条触发器。
pub async fn delete_trigger(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let deleted = triggers::delete(state.store.pool(), id).await?;
    if !deleted {
        return Err(ApiError::not_found(format!("触发器 {id} 不存在")));
    }
    Ok(Json(serde_json::json!({ "id": id, "deleted": true })))
}

/// 存储层的 `Invalid` 是「这次请求不成立」，映射成 400 而不是 500。
fn map_store_error(err: hub_store::StoreError) -> ApiError {
    match err {
        hub_store::StoreError::Invalid(message) => ApiError::bad_request(message),
        other => ApiError::internal(other.to_string()),
    }
}
