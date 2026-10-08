//! 管理面读接口 + 受控的恢复操作（删除已登记版本）。
//!
//! ⚠️ 按设计**不含中台内置鉴权**：管理面全插件化，主机面运维通道是唯一的逃生口。
//! 过渡期由 nginx 在部署层限制来源网段兜底——这是刻意的设计意图，见 `docs/design.md`
//! 的风险表，不是遗漏。authz 装配后 `/admin/` 前缀整体落在 SCOPE_ADMIN 下。

use axum::Json;
use axum::extract::{Path, Query, State};
use hub_proto::rejection_code_name;
use hub_store::instances::InstanceView;
use hub_store::model::{
    ContractRow, PluginInstanceRow, PluginOverviewRow, PluginRow, PluginVersionRow, ToolRow,
};
use serde::{Deserialize, Serialize};

use crate::ApiState;
use crate::error::ApiError;

/// 插件列表（含版本数与在线实例数）。
pub async fn list_plugins(
    State(state): State<ApiState>,
) -> Result<Json<Vec<PluginOverviewRow>>, ApiError> {
    let rows = hub_store::plugins::list_plugins(state.store.pool()).await?;
    Ok(Json(rows))
}

/// 插件详情：逐版本给出契约、MCP 工具与实例。
///
/// 控制台的插件详情页与排障都靠它——「这个版本到底声明了什么、现在跑在哪」。
pub async fn get_plugin(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<Json<PluginDetailResponse>, ApiError> {
    let detail = hub_store::plugins::describe_plugin(state.store.pool(), &name)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("插件 {name} 未注册")))?;

    // 「下发中」= 本插件最新登记的那个版本，且它有在线实例——与 MCP 工具面
    // `online_tools` 的口径一字不差：控制台的「这个版本的工具在不在面上」
    // 必须和 agent 实际看见的一致，两边各算一套必然漂移。
    let latest_registered = detail.versions.iter().map(|v| v.id).max();
    let mut versions = Vec::with_capacity(detail.versions.len());
    for version in detail.versions {
        let contracts = hub_store::plugins::contracts_of(state.store.pool(), version.id).await?;
        let tools = hub_store::plugins::tools_of(state.store.pool(), version.id).await?;
        let instances =
            hub_store::instances::instances_of_version(state.store.pool(), version.id).await?;
        let online = !instances.is_empty();
        let serving = online && Some(version.id) == latest_registered;
        versions.push(VersionDetail {
            version,
            contracts,
            tools,
            instances,
            online,
            serving,
        });
    }

    Ok(Json(PluginDetailResponse {
        plugin: detail.plugin,
        versions,
    }))
}

/// 全部在线实例。
pub async fn list_instances(
    State(state): State<ApiState>,
) -> Result<Json<Vec<InstanceView>>, ApiError> {
    let rows = hub_store::instances::list_instances(state.store.pool()).await?;
    Ok(Json(rows))
}

/// 某个消息类型被谁生产、被谁消费。
///
/// 改契约前的影响面分析就靠它：动一个字段之前先看有多少插件在两端。
pub async fn message_usage(
    State(state): State<ApiState>,
    Path(fq_name): Path<String>,
) -> Result<Json<MessageUsage>, ApiError> {
    let producers =
        hub_store::plugins::versions_by_fq_name(state.store.pool(), &fq_name, "produces").await?;
    let consumers =
        hub_store::plugins::versions_by_fq_name(state.store.pool(), &fq_name, "consumes").await?;

    Ok(Json(MessageUsage {
        fq_name,
        producers: producers.into_iter().map(PluginVersionRef::from).collect(),
        consumers: consumers.into_iter().map(PluginVersionRef::from).collect(),
    }))
}

/// 最近的注册拒绝留痕，新到旧。
///
/// 「实例 0 个但容器活着」的答案在这里：插件会无限重试注册，中台每轮都拒——
/// 拒绝原因（VERSION_CONFLICT、BREAKING_CHANGE 等）此前只在插件容器日志里，
/// 控制台上只能看到实例数归零，看不出为什么。
pub async fn list_rejections(
    State(state): State<ApiState>,
    Query(query): Query<RejectionsQuery>,
) -> Result<Json<Vec<RejectionView>>, ApiError> {
    // 默认 50 条对一屏卡片绰绰有余；上限 200 挡住把表整页拖走的查询
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let rows =
        hub_store::rejections::list(state.store.pool(), query.plugin.as_deref(), limit).await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| RejectionView {
                code_name: rejection_code_name(row.code),
                row,
            })
            .collect(),
    ))
}

/// 删除一个已登记的版本——VERSION_CONFLICT / BREAKING_CHANGE 拒绝后的恢复操作。
///
/// 版本连同它的契约、工具与实例一起被级联删除（实例行没了 = 工具面立即下线），
/// 插件方修好 manifest 重新注册即可重新登记。**不可逆**：落在 SCOPE_ADMIN 守卫下，
/// 主机面的 `hubctl remove-version` 与它是同一条 store 路径的两个入口。
///
/// 删除即销案：该插件此前的拒绝留痕一并清掉——版本都没了，留痕失去所指，
/// 留着只会让控制台对着一堆不再重试的旧问题空喊。
pub async fn delete_plugin_version(
    State(state): State<ApiState>,
    Path((name, version)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let deleted = hub_store::plugins::delete_version(state.store.pool(), &name, &version).await?;
    if !deleted {
        return Err(ApiError::not_found(format!(
            "插件 {name} 没有已登记的版本 {version}"
        )));
    }
    let cleared = hub_store::rejections::clear(state.store.pool(), &name, None).await?;
    Ok(Json(serde_json::json!({
        "plugin": name,
        "version": version,
        "deleted": true,
        "cleared_rejections": cleared,
    })))
}

#[derive(Debug, Deserialize)]
pub struct RejectionsQuery {
    /// 按插件过滤；缺省返回全部
    pub plugin: Option<String>,
    /// 返回条数上限，1..=200，默认 50
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct RejectionView {
    #[serde(flatten)]
    pub row: hub_store::model::RegistrationRejectionRow,
    /// 拒绝码的展示名（`VERSION_CONFLICT` 等）。前端不认枚举值
    pub code_name: String,
}

#[derive(Debug, Serialize)]
pub struct PluginDetailResponse {
    #[serde(flatten)]
    pub plugin: PluginRow,
    pub versions: Vec<VersionDetail>,
}

#[derive(Debug, Serialize)]
pub struct VersionDetail {
    #[serde(flatten)]
    pub version: PluginVersionRow,
    pub contracts: Vec<ContractRow>,
    pub tools: Vec<ToolRow>,
    pub instances: Vec<PluginInstanceRow>,
    /// 该版本当前有在线实例（实例行被摘除即下线，见 `instances::sweep_stale`）。
    pub online: bool,
    /// 该版本的工具正在被 MCP 工具面下发：是本插件最新登记版本，且有在线实例。
    /// 与 `hub-mcp` 的 `tools/list` 口径一致，旧版本即使实例还活着也是 false。
    pub serving: bool,
}

#[derive(Debug, Serialize)]
pub struct MessageUsage {
    pub fq_name: String,
    pub producers: Vec<PluginVersionRef>,
    pub consumers: Vec<PluginVersionRef>,
}

#[derive(Debug, Serialize)]
pub struct PluginVersionRef {
    pub plugin: String,
    pub version: String,
}

impl From<(String, String)> for PluginVersionRef {
    fn from((plugin, version): (String, String)) -> Self {
        Self { plugin, version }
    }
}

// ---------------------------------------------------------------- 插件调用审计

/// 插件调用审计的查询参数：全部可选，组合过滤。
#[derive(Debug, Deserialize)]
pub struct PluginAuditQuery {
    pub plugin: Option<String>,
    pub caller: Option<String>,
    /// `ok` / `rejected` / `error`
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, serde::Serialize)]
pub struct PluginAuditResponse {
    pub total: i64,
    pub rows: Vec<hub_store::spans::InvocationAuditRow>,
}

/// 插件调用审计：谁、何时、调了什么插件、结果、耗时。
///
/// **归 `hub:admin`**（/admin/ 前缀的门）：「谁调了什么」本身就是敏感信息，
/// 比一般目录读高一级。数据源与保留期见
/// [`hub_store::spans::list_invocation_audits`]。
pub async fn list_plugin_audit(
    State(state): State<ApiState>,
    Query(query): Query<PluginAuditQuery>,
) -> Result<Json<PluginAuditResponse>, ApiError> {
    let filter = hub_store::spans::InvocationAuditFilter {
        plugin: query.plugin.as_deref(),
        caller: query.caller.as_deref(),
        status: query.status.as_deref(),
        limit: query.limit.unwrap_or(50),
        offset: query.offset.unwrap_or(0),
    };
    let (rows, total) =
        hub_store::spans::list_invocation_audits(state.store.pool(), &filter).await?;
    Ok(Json(PluginAuditResponse { total, rows }))
}
