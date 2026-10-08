//! 触发器的登记与运行状态。
//!
//! 统一触发模型里，**HTTP 触发不需要建行**——`/flows/{name}/trigger` 这条路由天然存在，
//! 登记一张表反而多一份要同步的状态。这张表管的是「需要常驻监听才有意义」的那些：
//! cron 定时与 MQ 订阅。它们没有一个天然存在的入口，不登记就没人知道要跑。
//!
//! `last_fired_at` / `fired_count` / `last_error` 三列是刻意的：定时器最典型的故障是
//! **静默失效**——没报错、没日志，只是不再触发了。把运行状态和定义放在同一行，控制台
//! 一眼就能回答「这个定时器还活着吗」，而不必去翻日志。

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::model::TriggerRow;
use crate::{Result, StoreError};

/// cron 触发器
pub const KIND_CRON: &str = "cron";
/// MQ 订阅触发器
pub const KIND_MQ: &str = "mq";

pub struct NewTrigger<'a> {
    /// 挂在哪条 flow 上
    pub flow_name: &'a str,
    /// [`KIND_CRON`] 或 [`KIND_MQ`]
    pub kind: &'a str,
    /// 同一条 flow 下同类触发器靠它区分（多条 cron 各跑各的）
    pub name: &'a str,
    /// 触发时套用的信封模板
    pub config: &'a serde_json::Value,
}

/// 登记（或更新）一个触发器。
///
/// 用 `(flow_id, kind, name)` 唯一键 upsert 而不是每次插入新行：改一个定时器的表达式
/// 取配置里的非空字符串字段。
///
/// 空串视为「没给」：`{"expr": ""}` 与 `{}` 对调度器是同一件事，
/// 当成两种情形分别处理只会让错误信息与行为分叉。
fn config_str<'a>(config: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// 登记或更新一个触发器（同 `(flow, kind, name)` 视为同一条）。
///
/// 是「改」不是「换一个新的」，留下历史版本只会让「现在到底有哪几个定时器」变模糊。
///
/// 重新登记会把 `enabled` 置回 true：登记一份配置的意图就是「让它按这个跑」。
pub async fn upsert(pool: &PgPool, input: &NewTrigger<'_>) -> Result<TriggerRow> {
    if input.kind != KIND_CRON && input.kind != KIND_MQ {
        return Err(StoreError::Invalid(format!(
            "触发器类型只能是 {KIND_CRON} 或 {KIND_MQ}，收到 {}",
            input.kind
        )));
    }

    // 必填项在这里就拦掉。表达式本身能不能解析留给调度器判断——cron 解析器在它那儿，
    // 装配失败会记在这条触发器的 `last_error` 上、控制台看得见。
    // 但「字段压根没给」不该等到那一刻：那种触发器从存下来第一秒就注定不工作，
    // 而它在界面上看起来是配好了的。
    let (key, message) = match input.kind {
        KIND_CRON => (
            "expr",
            "cron 触发器必须给出 config.expr（如 \"0 2 * * *\"）",
        ),
        _ => (
            "stream",
            "mq 触发器必须给出 config.stream（要订阅的 Redis Stream 名）",
        ),
    };
    if config_str(input.config, key).is_none() {
        return Err(StoreError::Invalid(message.to_string()));
    }

    let row = sqlx::query_as::<_, TriggerRow>(
        "INSERT INTO triggers (flow_id, kind, name, config)
         SELECT f.id, $2, $3, $4 FROM flows f WHERE f.name = $1
         ON CONFLICT (flow_id, kind, name) DO UPDATE
            SET config     = EXCLUDED.config,
                enabled    = true,
                updated_at = now()
         RETURNING id, flow_id, kind, name, config, enabled, last_fired_at, last_error,
                   fired_count, created_at, updated_at",
    )
    .bind(input.flow_name)
    .bind(input.kind)
    .bind(input.name)
    .bind(input.config)
    .fetch_optional(pool)
    .await?
    // 没插进任何行只有一种可能：`SELECT ... FROM flows` 没选到 flow。刻意用
    // INSERT ... SELECT 这个形态，让「flow 不存在」体现为「一行没插」并在这里
    // 报一句人话，而不是把外键约束错误抛给调用方
    .ok_or_else(|| StoreError::Invalid(format!("flow {} 未定义，无法挂触发器", input.flow_name)))?;

    Ok(row)
}

/// 列出启用中的触发器。`kind` 为 `None` 时列出全部类型。
///
/// 调度器启动时读它来装配定时任务；关掉的触发器不该被装配进去。
pub async fn list_enabled(pool: &PgPool, kind: Option<&str>) -> Result<Vec<TriggerRow>> {
    let rows = sqlx::query_as::<_, TriggerRow>(
        "SELECT id, flow_id, kind, name, config, enabled, last_fired_at, last_error,
                fired_count, created_at, updated_at
           FROM triggers
          WHERE enabled AND ($1::text IS NULL OR kind = $1)
          ORDER BY id",
    )
    .bind(kind)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// 某条 flow 的全部触发器（含已禁用的）。控制台的详情页用它。
pub async fn list_of_flow(pool: &PgPool, flow_name: &str) -> Result<Vec<TriggerRow>> {
    let rows = sqlx::query_as::<_, TriggerRow>(
        "SELECT t.id, t.flow_id, t.kind, t.name, t.config, t.enabled, t.last_fired_at,
                t.last_error, t.fired_count, t.created_at, t.updated_at
           FROM triggers t
           JOIN flows f ON f.id = t.flow_id
          WHERE f.name = $1
          ORDER BY t.kind, t.name",
    )
    .bind(flow_name)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// 触发器连同它所属 flow 的名字。
///
/// 单独的 `TriggerRow` 只有 `flow_id`，而控制台的触发器页要按名字列出来——
/// 逐个回查 flow 表就是一次 N+1。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct TriggerView {
    pub id: i64,
    pub flow_id: i64,
    pub flow_name: String,
    pub kind: String,
    pub name: String,
    pub config: serde_json::Value,
    pub enabled: bool,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub fired_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 全部触发器（含已停用的），跨 flow。
///
/// 控制台的触发器页用它。**必须带上已停用的那些**：排障时问「它为什么没跑」，
/// 第一个答案往往就是「它被关掉了」——只列启用中的会把这个答案藏起来。
pub async fn list_all(pool: &PgPool) -> Result<Vec<TriggerView>> {
    let rows = sqlx::query_as::<_, TriggerView>(
        "SELECT t.id, t.flow_id, f.name AS flow_name, t.kind, t.name, t.config, t.enabled,
                t.last_fired_at, t.last_error, t.fired_count, t.created_at, t.updated_at
           FROM triggers t
           JOIN flows f ON f.id = t.flow_id
          -- `enabled` 升序 = 停用的排前面（PostgreSQL 里 false < true）。
          -- 排障时问「它为什么没跑」，第一个答案往往是「它被关了」。
          ORDER BY t.enabled, f.name, t.kind, t.name",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// 启用 / 停用一个触发器。
pub async fn set_enabled(pool: &PgPool, id: i64, enabled: bool) -> Result<bool> {
    let affected =
        sqlx::query("UPDATE triggers SET enabled = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(enabled)
            .execute(pool)
            .await?
            .rows_affected();

    Ok(affected > 0)
}

pub async fn delete(pool: &PgPool, id: i64) -> Result<bool> {
    let affected = sqlx::query("DELETE FROM triggers WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(affected > 0)
}

/// 记一次触发的结果。
///
/// **成功时要把 `last_error` 清掉**：否则一个修好了的定时器会永远带着一条历史错误，
/// 控制台据此报警就会变成狼来了。
pub async fn record_fired(
    pool: &PgPool,
    id: i64,
    at: DateTime<Utc>,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE triggers
            SET last_fired_at = $2,
                last_error    = $3,
                fired_count   = fired_count + 1
          WHERE id = $1",
    )
    .bind(id)
    .bind(at)
    .bind(error)
    .execute(pool)
    .await?;

    Ok(())
}
