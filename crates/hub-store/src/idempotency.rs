//! 幂等：把「至少一次」的投递变成「实际上只生效一次」。
//!
//! Stream 的投递语义是 at-least-once，「不丢消息」的反面就是「会重投」。所以去重不是
//! 可选项——没有它，一次执行会被重投放大成多次，而且放大是静默的。
//!
//! 判据只有一个：**第一次见到这个键返回 `true`，之后再见到返回 `false`**。
//! 靠 `INSERT ... ON CONFLICT DO NOTHING` 的 `rows_affected` 实现，而不是「先查再插」：
//! 后者在两个消费者同时处理同一条消息时会双双查到「没见过」，双双放行——而它们本来
//! 就是为并发准备的。
//!
//! 带 `expires_at` 而不是永久保留：表会随时间无限长，而幂等的价值只覆盖消息可能被
//! 重投的那个时间窗（默认与 `STREAM_RETENTION_HOURS` 对齐）。

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::Result;

/// 认领一个幂等键。
///
/// 返回 `true` 表示这是第一次见到它，调用方应当继续处理；
/// 返回 `false` 表示**这件事已经做过了**，调用方应当直接跳过。
pub async fn claim(
    pool: &PgPool,
    key: &str,
    run_id: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<bool> {
    let affected = sqlx::query(
        "INSERT INTO idempotency_keys (key, run_id, expires_at)
         VALUES ($1, $2, $3)
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(key)
    .bind(run_id)
    .bind(expires_at)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(affected > 0)
}

/// 这个键是否已经被认领过（只读查询，不改状态）。
///
/// 与 [`claim`] 的区别：`claim` 会**占住**这个键。想「先看看再决定要不要做」时用它。
pub async fn seen(pool: &PgPool, key: &str) -> Result<bool> {
    let row: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM idempotency_keys WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// 接管一个已被认领但**没有做完**的键，返回 `true` 表示接管成功。
///
/// 用来关掉 [`claim`] 留下的一个窗口：认领成功之后、事情做完之前进程被杀，消息会被
/// 重投，而重投的消费者看到键已存在就跳过——**这件事就永远没人做了**。
///
/// 所以「跳过」的判据不能只看键在不在，还要看**那件事到底做完了没有**。调用方据此
/// 走两步：键在 → 查完成记录；完成记录也没有 → 说明上一个持有者中途死了，
/// 用本方法把键拿回来接着做。
///
/// 代价是「上一个持有者其实还活着，只是慢」时会做两次。这正是 at-least-once 的
/// 本来含义——宁可重做，不可不做。
pub async fn take_over(pool: &PgPool, key: &str) -> Result<bool> {
    let affected = sqlx::query("UPDATE idempotency_keys SET seen_at = now() WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(affected > 0)
}

/// 放弃一个已经认领但没做的键，返回 `true` 表示确实删掉了。
///
/// 「认领」不是「完成」。基础设施失败（发不出去、库连不上）时要把键放回去，
/// 否则重投的那一次会被当成「已经做过了」而跳过。
pub async fn release(pool: &PgPool, key: &str) -> Result<bool> {
    let affected = sqlx::query("DELETE FROM idempotency_keys WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(affected > 0)
}

/// 清理过期的键，返回清理条数。
pub async fn purge_expired(pool: &PgPool, now: DateTime<Utc>) -> Result<u64> {
    let affected = sqlx::query("DELETE FROM idempotency_keys WHERE expires_at < $1")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected)
}

/// 构造一个节点执行的幂等键。
///
/// 放进这个模块而不是散落在调用点：键的格式一旦不一致，去重就会静默失效——两边各自
/// 拼出不同的字符串，谁也看不出问题，直到重复执行真的发生。
pub fn node_key(run_id: &str, node_id: &str) -> String {
    format!("node:{run_id}:{node_id}")
}
