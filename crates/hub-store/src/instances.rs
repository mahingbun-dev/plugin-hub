//! 实例的注册、心跳与摘除。
//!
//! 摘除只删实例行、保留插件与版本定义——插件下线不该让它的契约从注册表里消失，
//! 否则已编排的 flow 会突然「查不到这个插件」。

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::Result;
use crate::model::PluginInstanceRow;

/// 注册或更新实例。
///
/// 同一个 `instance_id` 重复注册视为插件进程重启：地址与心跳时间都会被刷新，
/// 不产生重复行。**状态凭证同时轮换**——旧凭证随进程一起作废。
///
/// `RETURNING` 有意不含 `state_token`：凭证只该在需要它的地方出现，不随
/// 常规的实例查询四处扩散。要取凭证用 [`find_state_token`]。
pub async fn upsert_instance(
    pool: &PgPool,
    version_id: i64,
    instance_id: &str,
    advertise_addr: &str,
    source_ip: Option<&str>,
) -> Result<PluginInstanceRow> {
    let row = sqlx::query_as::<_, PluginInstanceRow>(
        "INSERT INTO plugin_instances (version_id, instance_id, advertise_addr, source_ip, status, state_token)
         VALUES ($1, $2, $3, $4, 'healthy', gen_random_uuid()::text)
         ON CONFLICT (instance_id) DO UPDATE
            SET version_id        = EXCLUDED.version_id,
                advertise_addr    = EXCLUDED.advertise_addr,
                source_ip         = EXCLUDED.source_ip,
                status            = 'healthy',
                state_token       = gen_random_uuid()::text,
                last_heartbeat_at = now()
         RETURNING id, version_id, instance_id, advertise_addr, status, source_ip,
                   registered_at, last_heartbeat_at",
    )
    .bind(version_id)
    .bind(instance_id)
    .bind(advertise_addr)
    .bind(source_ip)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// 续期心跳。
///
/// 返回 `false` 表示实例已不在注册表里（被摘除或库被清过），调用方应让插件重新注册
/// ——这是实例被摘除后能自愈的关键。
pub async fn touch_heartbeat(pool: &PgPool, instance_id: &str, now: DateTime<Utc>) -> Result<bool> {
    let affected = sqlx::query(
        "UPDATE plugin_instances
            SET last_heartbeat_at = $2, status = 'healthy'
          WHERE instance_id = $1",
    )
    .bind(instance_id)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

pub async fn delete_instance(pool: &PgPool, instance_id: &str) -> Result<bool> {
    let affected = sqlx::query("DELETE FROM plugin_instances WHERE instance_id = $1")
        .bind(instance_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

/// 注销：只有凭证与这一行**当前**的状态凭证一致时才删除。
///
/// 与 [`delete_instance`] 的区别是**认属主**。注销路径原本只按 `instance_id` 删行，
/// 而 `instance_id` 由插件自己生成、可跨插件重复（缺省「主机名-PID」，同一 host
/// 网络下容器 PID 又都是 1，必然重复）——于是两个撞了 id 的插件里，谁先退出谁就删掉
/// **对方**那一行。被删的一方心跳仍按 `instance_id` 命中、返回 accepted，**完全察觉
/// 不到自己已经从注册表里消失**（实测：auth 重启一次，sql-executor 的工具从 MCP
/// 工具面上全部消失，而它自己的日志停在「已注册到中台」之后再无输出）。
///
/// 凭证把这条路径堵死：没注册成功就没有凭证，也就注销不了；旧凭证在重新注册时已被
/// 轮换掉（见 `upsert_instance`），拿旧凭证来注销同样删不动。判定与删除在**同一条
/// 语句**里，没有先查后删的窗口。
///
/// 空凭证直接不匹配（`state_token <> ''`），所以迁移前登记的旧行不会被空凭证注销——
/// 它们只能等心跳超时被 [`sweep_stale`] 摘除。
///
/// 返回 `false` 有两种可能：「没有这一行」与「凭证不符」。调用方若要区分，用
/// [`find_state_token`] 复查即可——**那只是为了让日志说得清，不参与判定**。
pub async fn delete_instance_with_token(
    pool: &PgPool,
    instance_id: &str,
    state_token: &str,
) -> Result<bool> {
    if state_token.is_empty() {
        return Ok(false);
    }
    let affected = sqlx::query(
        "DELETE FROM plugin_instances
          WHERE instance_id = $1 AND state_token = $2 AND state_token <> ''",
    )
    .bind(instance_id)
    .bind(state_token)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// 按 `instance_id` 查当前属主（插件名, 版本）。不存在返回 `None`。
///
/// 注册前用它挡住「顶掉别插件的实例行」：`instance_id` 由插件自己生成、可跨插件重复，
/// 而它同时是实例行与状态凭证的键。属主与注册方不是同一个插件时必须拒绝——否则被顶掉
/// 的那个插件的实例会从列表里消失，而它的心跳仍按 `instance_id` 命中、毫无察觉。
pub async fn instance_owner(pool: &PgPool, instance_id: &str) -> Result<Option<(String, String)>> {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT p.name, v.version
           FROM plugin_instances i
           JOIN plugin_versions v ON v.id = i.version_id
           JOIN plugins p ON p.id = v.plugin_id
          WHERE i.instance_id = $1",
    )
    .bind(instance_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 摘除心跳超时的实例，返回被摘除的 `instance_id`。
///
/// 掉线实例被摘除后，其上的流量会由中台的实例选择逻辑自动绕开（同版本的其它副本）。
pub async fn sweep_stale(pool: &PgPool, cutoff: DateTime<Utc>) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "DELETE FROM plugin_instances WHERE last_heartbeat_at < $1 RETURNING instance_id",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// 全部实例，连同所属插件与版本，供控制台与 MCP 列表使用。
pub async fn list_instances(pool: &PgPool) -> Result<Vec<InstanceView>> {
    let rows = sqlx::query_as::<_, InstanceView>(
        "SELECT i.id, p.name AS plugin_name, v.version, i.instance_id, i.advertise_addr,
                i.status, i.source_ip, i.registered_at, i.last_heartbeat_at
           FROM plugin_instances i
           JOIN plugin_versions v ON v.id = i.version_id
           JOIN plugins p ON p.id = v.plugin_id
          ORDER BY p.name, v.version, i.instance_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn instances_of_version(
    pool: &PgPool,
    version_id: i64,
) -> Result<Vec<PluginInstanceRow>> {
    let rows = sqlx::query_as::<_, PluginInstanceRow>(
        "SELECT id, version_id, instance_id, advertise_addr, status, source_ip,
                registered_at, last_heartbeat_at
           FROM plugin_instances WHERE version_id = $1 ORDER BY instance_id",
    )
    .bind(version_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 取实例的状态凭证。
///
/// 返回 `None` 表示实例不存在，或凭证为空（迁移前登记的旧行）。两种情况对
/// 调用方是同一件事：**这个实例不能用 HubState**。
pub async fn find_state_token(pool: &PgPool, instance_id: &str) -> Result<Option<String>> {
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT NULLIF(state_token, '') FROM plugin_instances WHERE instance_id = $1",
    )
    .bind(instance_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(token,)| token))
}

/// 按状态凭证反查插件名。凭证无效或为空时返回 `None`。
///
/// 这是 HubState 的认证原语：中台不信插件自报的身份，只信注册时下发的凭证。
/// 命中多个实例说明凭证重复（不该发生）——取第一个，不放大成错误：拒绝服务
/// 的代价比多查一行大得多。
pub async fn plugin_of_state_token(pool: &PgPool, token: &str) -> Result<Option<String>> {
    if token.is_empty() {
        return Ok(None);
    }
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT p.name
           FROM plugin_instances i
           JOIN plugin_versions v ON v.id = i.version_id
           JOIN plugins p ON p.id = v.plugin_id
          WHERE i.state_token = $1
          LIMIT 1",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(name,)| name))
}

/// 按状态凭证反查调用方（插件名, 版本 id）。凭证无效或为空时返回 `None`。
///
/// 与 [`plugin_of_state_token`] 是同一条 join，只是多选出 `version_id`：插件互调的
/// 权限校验除了 caller 的名字，还要拿它注册版本的 manifest 去解析 `invokes` 声明
/// （见 [`crate::plugins::manifest_of_version`]），版本 id 就是那一步的句柄。
/// 「空凭证直接不查、多实例命中取第一个」的口径与 [`plugin_of_state_token`] 一致。
pub async fn caller_of_state_token(pool: &PgPool, token: &str) -> Result<Option<(String, i64)>> {
    if token.is_empty() {
        return Ok(None);
    }
    let row = sqlx::query_as::<_, (String, i64)>(
        "SELECT p.name, v.id
           FROM plugin_instances i
           JOIN plugin_versions v ON v.id = i.version_id
           JOIN plugins p ON p.id = v.plugin_id
          WHERE i.state_token = $1
          LIMIT 1",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 实例的带插件名/版本的视图（列表与排障用）。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct InstanceView {
    pub id: i64,
    pub plugin_name: String,
    pub version: String,
    pub instance_id: String,
    pub advertise_addr: String,
    pub status: String,
    pub source_ip: Option<String>,
    pub registered_at: DateTime<Utc>,
    pub last_heartbeat_at: DateTime<Utc>,
}
