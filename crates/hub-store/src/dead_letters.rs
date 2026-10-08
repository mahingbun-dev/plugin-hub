//! 死信：重投耗尽之后的消息去哪了。
//!
//! **进死信而不是丢弃**是需要理由的，理由有两条：一条消息反复失败说明它不是暂时性问题，
//! 继续重投只是持续消耗资源；而直接丢掉会让「这条数据去哪了」变成一个查不到答案的问题。
//! 死信表就是那个答案，并且它还是可重放的。
//!
//! 载荷只留**摘要**，与 run / span 的留存策略一致。真要重放时载荷从哪来？由重放方决定
//! ——总线不知道业务，`payload_ref` 在消息里，重放方能拿到它才有重放的意义。存全量
//! 会让这张表变成另一个「全量报文库」，前面所有克制就白做了。

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::Result;
use crate::model::DeadLetterRow;

pub struct NewDeadLetter<'a> {
    pub stream: &'a str,
    pub stream_id: &'a str,

    pub run_id: Option<&'a str>,
    pub flow_name: Option<&'a str>,
    pub node_id: Option<&'a str>,

    pub attempts: i32,
    pub error: &'a str,
    pub payload_summary: Option<&'a str>,
}

/// 记一条死信，返回它的 id。重复记同一条消息时刷新错误信息并返回已有的 id。
///
/// 「同一条消息」的判据是 `(stream, stream_id)`——消息在 Stream 上的 id 是唯一的，
/// 而重复进死信完全可能发生（比如巡检和消费循环同时判定它已耗尽）。
pub async fn insert(pool: &PgPool, entry: &NewDeadLetter<'_>) -> Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO dead_letters (stream, stream_id, run_id, flow_name, node_id,
                                   attempts, error, payload_summary)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (stream, stream_id) DO UPDATE
            SET attempts        = EXCLUDED.attempts,
                error           = EXCLUDED.error,
                payload_summary = EXCLUDED.payload_summary,
                last_attempt_at = now()
         RETURNING id",
    )
    .bind(entry.stream)
    .bind(entry.stream_id)
    .bind(entry.run_id)
    .bind(entry.flow_name)
    .bind(entry.node_id)
    .bind(entry.attempts)
    .bind(entry.error)
    .bind(entry.payload_summary)
    .fetch_one(pool)
    .await?;

    Ok(id)
}

/// 死信列表，最近的在前。
///
/// `only_pending` 为真时只列**还没重放过**的——控制台上默认看到的应当是需要处理的
/// 那一批，已经重放完的属于历史。
pub async fn list(pool: &PgPool, only_pending: bool, limit: i64) -> Result<Vec<DeadLetterRow>> {
    let rows = sqlx::query_as::<_, DeadLetterRow>(
        "SELECT id, stream, stream_id, run_id, flow_name, node_id, attempts, error,
                payload_summary, replayed_run_id, replayed_at, first_seen_at, last_attempt_at
           FROM dead_letters
          WHERE ($1 = false OR replayed_at IS NULL)
          ORDER BY last_attempt_at DESC
          LIMIT $2",
    )
    .bind(only_pending)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn find(pool: &PgPool, id: i64) -> Result<Option<DeadLetterRow>> {
    let row = sqlx::query_as::<_, DeadLetterRow>(
        "SELECT id, stream, stream_id, run_id, flow_name, node_id, attempts, error,
                payload_summary, replayed_run_id, replayed_at, first_seen_at, last_attempt_at
           FROM dead_letters WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// 按原消息 id 找死信。消费循环判定「已耗尽」时用它，避免重复插一条。
pub async fn find_by_message(
    pool: &PgPool,
    stream: &str,
    stream_id: &str,
) -> Result<Option<DeadLetterRow>> {
    let row = sqlx::query_as::<_, DeadLetterRow>(
        "SELECT id, stream, stream_id, run_id, flow_name, node_id, attempts, error,
                payload_summary, replayed_run_id, replayed_at, first_seen_at, last_attempt_at
           FROM dead_letters WHERE stream = $1 AND stream_id = $2",
    )
    .bind(stream)
    .bind(stream_id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// 标记一条死信已被重放，并记下重放成了哪次新执行。
///
/// **必须记下新的 run_id**：否则「重放过了」就成了一句没有下文的断言，谁也没法顺着
/// 它去查重放的结果——而死信重放的价值恰恰在于「重放之后这次到底成没成」。
///
/// 返回 `false` 表示这条死信不存在或已经重放过（幂等：重复点重放不该产生第二次执行）。
pub async fn mark_replayed(
    pool: &PgPool,
    id: i64,
    new_run_id: &str,
    at: DateTime<Utc>,
) -> Result<bool> {
    let affected = sqlx::query(
        "UPDATE dead_letters
            SET replayed_run_id = $2, replayed_at = $3
          WHERE id = $1 AND replayed_at IS NULL",
    )
    .bind(id)
    .bind(new_run_id)
    .bind(at)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(affected > 0)
}

/// 抢占一条死信的重放权，返回 `true` 表示由本次调用负责重放。
///
/// 重放不是「改个标志位」就完了——它要**真的产生一次新执行**。所以这里做的是
/// 一次 CAS 式的抢占：先把自己的名字写上，抢到了再去入队。抢不到说明别人正在放，
/// 直接拒绝比产生两次执行好。
///
/// 分两步（先抢占、后补 run_id）而不是一步写完，是因为 run_id 要等入队成功才有。
/// 中间的失败由 [`clear_replay`] 撤销。
pub async fn begin_replay(pool: &PgPool, id: i64, at: DateTime<Utc>) -> Result<bool> {
    let affected = sqlx::query(
        "UPDATE dead_letters SET replayed_at = $2
          WHERE id = $1 AND replayed_at IS NULL",
    )
    .bind(id)
    .bind(at)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(affected > 0)
}

/// 补上「重放成了哪次新执行」。
pub async fn set_replayed_run(pool: &PgPool, id: i64, run_id: &str) -> Result<()> {
    sqlx::query("UPDATE dead_letters SET replayed_run_id = $2 WHERE id = $1")
        .bind(id)
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 撤销重放抢占。入队失败时用它——**抢占了却没真的重放**，比没抢占更糟：
/// 那条死信会显示「已重放」，而实际上什么也没发生。
pub async fn clear_replay(pool: &PgPool, id: i64) -> Result<()> {
    sqlx::query("UPDATE dead_letters SET replayed_at = NULL, replayed_run_id = NULL WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 清理躺太久的死信，返回清理条数。
///
/// 给人留出处理窗口之后再清。**已经重放过的可以更早清**：它已经完成了使命，留着
/// 只占空间；没重放的要多留一阵子，那正是需要人去看的一批。
pub async fn purge(
    pool: &PgPool,
    replayed_before: DateTime<Utc>,
    unreplayed_before: DateTime<Utc>,
) -> Result<u64> {
    let affected = sqlx::query(
        "DELETE FROM dead_letters
          WHERE (replayed_at IS NOT NULL AND replayed_at < $1)
             OR (replayed_at IS NULL AND last_attempt_at < $2)",
    )
    .bind(replayed_before)
    .bind(unreplayed_before)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(affected)
}
