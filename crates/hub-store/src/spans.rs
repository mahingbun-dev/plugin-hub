//! 调用链 span 的读写。
//!
//! **自存而不是只依赖 OTel 后端**：控制台要能按 traceId 直接查到「这条数据经过了哪些插件、
//! 在哪一跳慢了」，采样后的 trace 后端给不了这个确定性。span 的字段刻意按 OTel 的形状设计，
//! 将来要接 OTel 后端时导出是机械转换，不用改数据模型。

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::Result;
use crate::model::SpanRow;

pub struct NewSpan<'a> {
    pub trace_id: &'a str,
    pub span_id: &'a str,
    pub parent_span_id: Option<&'a str>,
    pub run_id: Option<&'a str>,
    pub node_id: Option<&'a str>,
    pub name: &'a str,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i64,
    /// `ok` / `error` / `rejected`
    pub status: &'a str,
    pub attributes: Option<&'a Value>,
}

/// 写入一个 span。
pub async fn insert_span(pool: &PgPool, span: &NewSpan<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO spans (trace_id, span_id, parent_span_id, run_id, node_id, name,
                            started_at, duration_ms, status, attributes)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT DO NOTHING",
    )
    .bind(span.trace_id)
    .bind(span.span_id)
    .bind(span.parent_span_id)
    .bind(span.run_id)
    .bind(span.node_id)
    .bind(span.name)
    .bind(span.started_at)
    .bind(span.duration_ms)
    .bind(span.status)
    .bind(span.attributes)
    .execute(pool)
    .await?;
    Ok(())
}

/// 按 trace 取全部 span，按开始时间排序。
///
/// 控制台点开一条调用链时用它——这是「这条数据卡在哪一跳」的直接答案。
pub async fn list_spans_by_trace(pool: &PgPool, trace_id: &str) -> Result<Vec<SpanRow>> {
    let rows = sqlx::query_as::<_, SpanRow>(
        "SELECT id, trace_id, span_id, parent_span_id, run_id, node_id, name,
                started_at, duration_ms, status, attributes
           FROM spans WHERE trace_id = $1
          ORDER BY started_at, id",
    )
    .bind(trace_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 取最近若干条 trace（每个 trace 一行，给出它的根 span 信息）。
///
/// 控制台的调用链列表用它——按 trace 聚合而不是按 span 铺开，
/// 否则一次 3 节点的执行会在列表里占 4 行。
pub async fn list_recent_traces(pool: &PgPool, limit: i64) -> Result<Vec<TraceSummaryRow>> {
    let limit = limit.clamp(1, 500);
    let rows = sqlx::query_as::<_, TraceSummaryRow>(
        // sum() 在 PostgreSQL 里返回 NUMERIC，必须显式转回 BIGINT——
        // 否则映射到 i64 会在解码时炸（而不是在编译期或 SQL 解析期）
        "SELECT trace_id,
                min(started_at)                                          AS started_at,
                max(started_at + duration_ms * interval '1 millisecond') AS finished_at,
                count(*)                                                 AS span_count,
                count(*) FILTER (WHERE status <> 'ok')                   AS error_count,
                COALESCE(sum(duration_ms), 0)::bigint                    AS total_duration_ms
           FROM spans
          GROUP BY trace_id
          ORDER BY min(started_at) DESC
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 清理过期的 span，返回删除条数。
///
/// span 的保留期（默认 7 天）比审计短：它是排障用的，不是追责用的。
pub async fn purge_spans_before(pool: &PgPool, cutoff: DateTime<Utc>) -> Result<u64> {
    let affected = sqlx::query("DELETE FROM spans WHERE started_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected)
}

/// 一次 trace 的概览。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct TraceSummaryRow {
    pub trace_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub span_count: i64,
    pub error_count: i64,
    pub total_duration_ms: i64,
}

// ---------------------------------------------------------------- 插件调用审计

/// 插件调用审计的一行（从 spans 里按 `attributes.kind = plugin-invocation` 筛出）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct InvocationAuditRow {
    pub trace_id: String,
    pub plugin: String,
    pub tool: Option<String>,
    pub caller: String,
    /// `ok` / `rejected` / `error`
    pub status: String,
    pub duration_ms: i64,
    pub started_at: DateTime<Utc>,
    pub instance_id: Option<String>,
    pub error: Option<String>,
    /// 完整 attributes：审计页展开细节（issues、message_id 等）用
    pub attributes: Option<Value>,
}

/// 插件调用审计的过滤条件与分页。
pub struct InvocationAuditFilter<'a> {
    pub plugin: Option<&'a str>,
    pub caller: Option<&'a str>,
    pub status: Option<&'a str>,
    pub limit: i64,
    pub offset: i64,
}

/// 插件调用审计：中台侧「谁、何时、调了什么插件、结果、耗时」。
///
/// 数据源是 hub-mcp 给每次插件调用写的 span（`attributes.kind =
/// plugin-invocation`）。**它随 spans 的保留期走（默认 7 天）**——审计页
/// 是排障+追责两用，要更长的追责窗口调大 `SPAN_RETENTION_DAYS`；将来要
/// 独立的长期审计存储，再从这张表分流。
pub async fn list_invocation_audits(
    pool: &PgPool,
    filter: &InvocationAuditFilter<'_>,
) -> Result<(Vec<InvocationAuditRow>, i64)> {
    let mut rows_query = sqlx::QueryBuilder::new(
        "SELECT trace_id, attributes->>'plugin' AS plugin, attributes->>'tool' AS tool,
                attributes->>'caller' AS caller, status, duration_ms, started_at,
                attributes->>'instance_id' AS instance_id, attributes->>'error' AS error,
                attributes
           FROM spans WHERE attributes->>'kind' = 'plugin-invocation'",
    );
    let mut count_query = sqlx::QueryBuilder::new(
        "SELECT count(*) FROM spans WHERE attributes->>'kind' = 'plugin-invocation'",
    );
    for (column, value) in [
        ("attributes->>'plugin'", filter.plugin),
        ("attributes->>'caller'", filter.caller),
        ("status", filter.status),
    ] {
        if let Some(value) = value {
            let predicate = format!(" AND {column} = ");
            rows_query
                .push(predicate.clone())
                .push_bind(value.to_string());
            count_query.push(predicate).push_bind(value.to_string());
        }
    }
    rows_query
        .push(" ORDER BY started_at DESC LIMIT ")
        .push_bind(filter.limit.clamp(1, 200))
        .push(" OFFSET ")
        .push_bind(filter.offset.max(0));
    let rows = rows_query
        .build_query_as::<InvocationAuditRow>()
        .fetch_all(pool)
        .await?;
    let total: i64 = count_query
        .build_query_as::<(i64,)>()
        .fetch_one(pool)
        .await?
        .0;
    Ok((rows, total))
}
