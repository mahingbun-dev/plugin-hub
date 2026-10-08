//! 执行记录（run / run_node / span）的读写。
//!
//! 这些记录是控制台回答「这条数据卡在哪一跳」的依据，也是事后追责的凭据。
//! 刻意**不落全量报文**——只留摘要，存储与合规都受不了全量（见 docs/design.md 的留存策略）。

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::Result;
use crate::model::{RunNodeRow, RunRow};

/// 一次执行的全部信息（含节点明细）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunDetail {
    #[serde(flatten)]
    pub run: RunRow,
    pub nodes: Vec<RunNodeRow>,
}

pub struct NewRun<'a> {
    pub run_id: &'a str,
    pub flow_id: i64,
    pub flow_revision: i32,
    pub trace_id: &'a str,
    pub subject: Option<&'a Value>,
    pub trigger: Option<&'a Value>,
    /// `running` / `succeeded` / `failed` / `rejected`
    pub status: &'a str,
    pub input_summary: Option<&'a str>,
    pub error: Option<&'a str>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

pub struct NewRunNode<'a> {
    pub run_id: &'a str,
    pub node_id: &'a str,
    pub plugin: &'a str,
    pub version: &'a str,
    pub instance_id: Option<&'a str>,
    pub attempt: i32,
    /// `succeeded` / `failed` / `rejected` / `skipped`
    pub status: &'a str,
    pub duration_ms: Option<i64>,
    pub io_summary: Option<&'a str>,
    pub error: Option<&'a str>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// 写入一次执行。重复写入同一 `run_id` 时覆盖——执行状态会从 running 变成终态。
pub async fn upsert_run(pool: &PgPool, run: &NewRun<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO runs (run_id, flow_id, flow_revision, trace_id, subject, trigger,
                           status, input_summary, error, started_at, finished_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (run_id) DO UPDATE
            SET status = EXCLUDED.status,
                error = EXCLUDED.error,
                finished_at = EXCLUDED.finished_at",
    )
    .bind(run.run_id)
    .bind(run.flow_id)
    .bind(run.flow_revision)
    .bind(run.trace_id)
    .bind(run.subject)
    .bind(run.trigger)
    .bind(run.status)
    .bind(run.input_summary)
    .bind(run.error)
    .bind(run.started_at)
    .bind(run.finished_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// 写入一个节点的执行记录。
///
/// 同一次执行的同一个节点的同一次尝试重复写入是无害的（数据库有唯一约束）。
/// 返回 `false` 表示这一行**已经存在**——异步执行器据此判断「这个节点已经跑过了」，
/// 而这正是「消息会重投」的对策：同一件事做两次的结果与做一次相同。
pub async fn insert_run_node(pool: &PgPool, node: &NewRunNode<'_>) -> Result<bool> {
    let affected = sqlx::query(
        "INSERT INTO run_nodes (run_id, node_id, plugin, version, instance_id, attempt,
                                status, duration_ms, io_summary, error, started_at, finished_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         ON CONFLICT (run_id, node_id, attempt) DO NOTHING",
    )
    .bind(node.run_id)
    .bind(node.node_id)
    .bind(node.plugin)
    .bind(node.version)
    .bind(node.instance_id)
    .bind(node.attempt)
    .bind(node.status)
    .bind(node.duration_ms)
    .bind(node.io_summary)
    .bind(node.error)
    .bind(node.started_at)
    .bind(node.finished_at)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(affected > 0)
}

/// 节点跑完之后收敛 run 的状态。返回收敛出的终态；还没收敛时返回 `None`。
///
/// `expected_nodes` 是这条编排的节点总数。异步链里每个节点独立跑完，**最后一个**跑完
/// 的那个负责把 run 定终态——但「谁才是最后一个」在并发下不能靠计数猜。
///
/// 做法是在 `runs` 那一行上加锁：所有收敛判断在同一行上串行化，后到的那一个一定能
/// 看见先到的那个已提交的 `run_nodes` 行（PostgreSQL 默认的 READ COMMITTED 下，锁等待
/// 结束后每条语句取新快照）。锁的粒度落在「同一次执行的节点之间」，不同 run 互不影响。
///
/// `forced` 由失败短路给出：某个节点失败时它的下游根本不会跑，「等所有节点到齐」永远
/// 不会成立，必须显式定终态。
pub async fn settle_run(
    pool: &PgPool,
    run_id: &str,
    expected_nodes: i64,
    forced: Option<(&str, &str)>,
) -> Result<Option<String>> {
    let mut tx = pool.begin().await?;

    let locked: Option<(String,)> =
        sqlx::query_as("SELECT status FROM runs WHERE run_id = $1 FOR UPDATE")
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;

    let Some((current,)) = locked else {
        // run 不见了（被清过）：没什么可收敛的
        return Ok(None);
    };

    // 已经是终态：别人先收敛了。**不能覆盖**——先到的那次带着真实的失败原因，
    // 被后到的「成功」盖掉会让排障时看到一条假的好消息。
    if is_terminal(&current) {
        return Ok(Some(current));
    }

    let settled = match forced {
        Some((status, error)) => {
            sqlx::query(
                "UPDATE runs SET status = $2, error = $3, finished_at = now() WHERE run_id = $1",
            )
            .bind(run_id)
            .bind(status)
            .bind(error)
            .execute(&mut *tx)
            .await?;
            Some(status.to_string())
        }
        None => {
            let (done,): (i64,) =
                sqlx::query_as("SELECT count(*) FROM run_nodes WHERE run_id = $1")
                    .bind(run_id)
                    .fetch_one(&mut *tx)
                    .await?;

            if done >= expected_nodes {
                sqlx::query(
                    "UPDATE runs SET status = 'succeeded', finished_at = now() WHERE run_id = $1",
                )
                .bind(run_id)
                .execute(&mut *tx)
                .await?;
                Some("succeeded".to_string())
            } else {
                // 还没跑完：从 queued 翻到 running，让控制台看得出「正在跑」
                sqlx::query("UPDATE runs SET status = 'running' WHERE run_id = $1")
                    .bind(run_id)
                    .execute(&mut *tx)
                    .await?;
                None
            }
        }
    };

    tx.commit().await?;
    Ok(settled)
}

/// 这个状态是不是终态。
pub fn is_terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "rejected")
}

/// 这个节点在这条 run 里是否已经有执行记录（不论成败）。
///
/// 异步执行器用它区分两种「幂等键已存在」：**做完过**（该跳过）与**上一个持有者中途
/// 死了**（该接着做）。少了这个区分，一次进程被杀就会让那个节点永远没人跑。
pub async fn node_done(pool: &PgPool, run_id: &str, node_id: &str) -> Result<bool> {
    let row: Option<(i32,)> =
        sqlx::query_as("SELECT 1 FROM run_nodes WHERE run_id = $1 AND node_id = $2 LIMIT 1")
            .bind(run_id)
            .bind(node_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

pub async fn find_run(pool: &PgPool, run_id: &str) -> Result<Option<RunRow>> {
    let row = sqlx::query_as::<_, RunRow>(
        "SELECT run_id, flow_id, flow_revision, trace_id, subject, trigger,
                status, input_summary, error, started_at, finished_at
           FROM runs WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 一次执行的全部明细。控制台点开一条调用链时用它。
pub async fn find_run_detail(pool: &PgPool, run_id: &str) -> Result<Option<RunDetail>> {
    let Some(run) = find_run(pool, run_id).await? else {
        return Ok(None);
    };
    let nodes = list_run_nodes(pool, run_id).await?;
    Ok(Some(RunDetail { run, nodes }))
}

pub async fn list_run_nodes(pool: &PgPool, run_id: &str) -> Result<Vec<RunNodeRow>> {
    let rows = sqlx::query_as::<_, RunNodeRow>(
        "SELECT id, run_id, node_id, plugin, version, instance_id, attempt,
                status, duration_ms, io_summary, error, started_at, finished_at
           FROM run_nodes WHERE run_id = $1
          ORDER BY started_at, id",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 按 trace 查全部执行——一条 trace 可能对应多次执行（异步链里会）。
pub async fn list_runs_by_trace(pool: &PgPool, trace_id: &str) -> Result<Vec<RunRow>> {
    let rows = sqlx::query_as::<_, RunRow>(
        "SELECT run_id, flow_id, flow_revision, trace_id, subject, trigger,
                status, input_summary, error, started_at, finished_at
           FROM runs WHERE trace_id = $1 ORDER BY started_at",
    )
    .bind(trace_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 列出执行记录（新到旧）。`flow_name` 为 `None` 时返回全部。
pub async fn list_runs(pool: &PgPool, flow_name: Option<&str>, limit: i64) -> Result<Vec<RunRow>> {
    let limit = limit.clamp(1, 1000);

    let rows = match flow_name {
        Some(name) => {
            sqlx::query_as::<_, RunRow>(
                "SELECT r.run_id, r.flow_id, r.flow_revision, r.trace_id, r.subject, r.trigger,
                        r.status, r.input_summary, r.error, r.started_at, r.finished_at
                   FROM runs r JOIN flows f ON f.id = r.flow_id
                  WHERE f.name = $1
                  ORDER BY r.started_at DESC LIMIT $2",
            )
            .bind(name)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, RunRow>(
                "SELECT run_id, flow_id, flow_revision, trace_id, subject, trigger,
                        status, input_summary, error, started_at, finished_at
                   FROM runs ORDER BY started_at DESC LIMIT $1",
            )
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows)
}

/// 清理过期的执行记录，返回删除条数。
///
/// `run_nodes` 由外键级联一起删；spans 单独清理（它的保留期更短）。
///
/// **只删终态的**。停在 `queued` / `running` 的执行是「有东西卡住了」的信号——
/// 按时间无差别清掉它，等于把这个问题从记录里抹掉，而它本来正是要被看见的。
pub async fn purge_runs_before(pool: &PgPool, cutoff: DateTime<Utc>) -> Result<u64> {
    let affected = sqlx::query(
        "DELETE FROM runs
          WHERE started_at < $1 AND status IN ('succeeded', 'failed', 'rejected')",
    )
    .bind(cutoff)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}
