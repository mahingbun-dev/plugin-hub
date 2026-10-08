//! 注册拒绝的留痕：记录、销案与查询。
//!
//! 写入口只有 [`record`]（registry 在拒绝时调用，重试由 upsert 消化）；
//! 销案两条路：插件注册成功（[`clear`]，按实例）与版本被管理面删除
//! （[`clear`] 传 `None`，整个插件的旧记录作废——删版本本就是「彻底重来」，
//! 留着旧拒绝只会让控制台继续喊狼来了）。

use sqlx::PgPool;

use crate::Result;
use crate::model::RegistrationRejectionRow;

/// 一次注册被拒的全部事实（来自 registry 的拒绝路径）。
pub struct NewRejection<'a> {
    pub plugin_name: &'a str,
    pub instance_id: &'a str,

    /// `hub.v1.RejectCode` 的 i32 值
    pub code: i32,
    pub version: &'a str,
    pub message: &'a str,
    pub detail: &'a str,
    pub source_ip: &'a str,
}

/// 记一次拒绝。同一 (插件, 实例, 拒绝码) 已有记录时累计 `count` 并刷新
/// `last_seen_at`，`first_seen_at` 保持不变。
pub async fn record(pool: &PgPool, input: &NewRejection<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO registration_rejections
                (plugin_name, instance_id, code, version, message, detail, source_ip)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (plugin_name, instance_id, code) DO UPDATE
            SET version = EXCLUDED.version,
                message = EXCLUDED.message,
                detail = EXCLUDED.detail,
                source_ip = EXCLUDED.source_ip,
                count = registration_rejections.count + 1,
                last_seen_at = now()",
    )
    .bind(input.plugin_name)
    .bind(input.instance_id)
    .bind(input.code)
    .bind(input.version)
    .bind(input.message)
    .bind(input.detail)
    .bind(input.source_ip)
    .execute(pool)
    .await?;
    Ok(())
}

/// 销案。`instance_id` 给 `None` 表示清掉该插件的全部记录。
pub async fn clear(pool: &PgPool, plugin_name: &str, instance_id: Option<&str>) -> Result<u64> {
    let affected = sqlx::query(
        "DELETE FROM registration_rejections
          WHERE plugin_name = $1 AND ($2::text IS NULL OR instance_id = $2)",
    )
    .bind(plugin_name)
    .bind(instance_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// 最近的拒绝记录，新到旧。`plugin_name` 传 `None` 查全部。
pub async fn list(
    pool: &PgPool,
    plugin_name: Option<&str>,
    limit: i64,
) -> Result<Vec<RegistrationRejectionRow>> {
    let rows = sqlx::query_as::<_, RegistrationRejectionRow>(
        "SELECT plugin_name, instance_id, code, version, message, detail,
                source_ip, count, first_seen_at, last_seen_at
           FROM registration_rejections
          WHERE ($1::text IS NULL OR plugin_name = $1)
          ORDER BY last_seen_at DESC
          LIMIT $2",
    )
    .bind(plugin_name)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 留存巡检：清掉 `last_seen_at` 早于 `cutoff` 的记录。
///
/// 一行 30 天没有重试来刷新，说明问题早就没了（修好或下线）——
/// 留痕是给「现在正在发生的事」看的，不是档案。
pub async fn purge_before(pool: &PgPool, cutoff: chrono::DateTime<chrono::Utc>) -> Result<u64> {
    let affected = sqlx::query("DELETE FROM registration_rejections WHERE last_seen_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected)
}
