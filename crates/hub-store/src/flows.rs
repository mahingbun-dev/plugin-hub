//! flow 定义的读写与「草稿 → 发布」状态机。
//!
//! 状态机刻意做得很小，但两个约束是硬的：
//!
//! 1. **一条 flow 同时只能有一份草稿**（数据库用部分唯一索引兜住）
//! 2. **发布不可逆地替换上一版**：上一版转 archived 保留可查，但不再是「当前生效」
//!
//! 发布这一步之所以要人来做，是因为它直接改变生产流量走向；agent 只能改草稿。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

use crate::Result;
use crate::model::FlowRevisionRow;

/// flow 本体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct FlowRow {
    pub id: i64,
    pub name: String,
    pub description: String,

    /// 当前生效的修订号；0 表示从未发布过
    pub published_revision: i32,
}

/// flow 列表项：本体 + 修订数 + 是否有草稿。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct FlowSummaryRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub published_revision: i32,
    pub revision_count: i64,
    pub has_draft: bool,
}

/// 写入草稿所需的输入。
#[derive(Debug, Clone)]
pub struct DraftInput<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub definition: &'a Value,
    /// 保存时的校验结果快照，供排障时回答「当时为什么存不下去」
    pub validation: Option<&'a Value>,
    pub created_by: &'a str,
}

/// 还没保存过的 flow 想发布时用它——发布的前提是有一份草稿。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlowStateError {
    #[error("flow {0} 未定义")]
    FlowNotFound(String),

    #[error("flow {0} 没有草稿可发布")]
    NoDraft(String),
}

impl From<FlowStateError> for crate::StoreError {
    fn from(err: FlowStateError) -> Self {
        // 状态机的错误对调用方来说是「请求有问题」，不是数据库故障
        crate::StoreError::Invalid(err.to_string())
    }
}

/// 按 id 找 flow。触发器挂的是 id，触发时要拿回名字。
pub async fn find_flow_by_id(pool: &PgPool, flow_id: i64) -> Result<Option<FlowRow>> {
    let row = sqlx::query_as::<_, FlowRow>(
        "SELECT id, name, description, published_revision FROM flows WHERE id = $1",
    )
    .bind(flow_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn find_flow(pool: &PgPool, name: &str) -> Result<Option<FlowRow>> {
    let row = sqlx::query_as::<_, FlowRow>(
        "SELECT id, name, description, published_revision FROM flows WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn list_flows(pool: &PgPool) -> Result<Vec<FlowSummaryRow>> {
    let rows = sqlx::query_as::<_, FlowSummaryRow>(
        "SELECT f.id,
                f.name,
                f.description,
                f.published_revision,
                (SELECT count(*) FROM flow_revisions r WHERE r.flow_id = f.id) AS revision_count,
                EXISTS (SELECT 1 FROM flow_revisions r
                         WHERE r.flow_id = f.id AND r.status = 'draft') AS has_draft
           FROM flows f
          ORDER BY f.name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 取当前草稿。
pub async fn find_draft(pool: &PgPool, flow_id: i64) -> Result<Option<FlowRevisionRow>> {
    find_revision_by_status(pool, flow_id, "draft").await
}

/// 取当前已发布的修订。
pub async fn find_published(pool: &PgPool, flow_id: i64) -> Result<Option<FlowRevisionRow>> {
    find_revision_by_status(pool, flow_id, "published").await
}

pub async fn find_revision(
    pool: &PgPool,
    flow_id: i64,
    revision: i32,
) -> Result<Option<FlowRevisionRow>> {
    let row = sqlx::query_as::<_, FlowRevisionRow>(
        "SELECT id, flow_id, revision, definition, status, validation, created_by,
                created_at, published_at
           FROM flow_revisions WHERE flow_id = $1 AND revision = $2",
    )
    .bind(flow_id)
    .bind(revision)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 列出某条 flow 的全部修订（新到旧）。
pub async fn list_revisions(pool: &PgPool, flow_id: i64) -> Result<Vec<FlowRevisionRow>> {
    let rows = sqlx::query_as::<_, FlowRevisionRow>(
        "SELECT id, flow_id, revision, definition, status, validation, created_by,
                created_at, published_at
           FROM flow_revisions WHERE flow_id = $1 ORDER BY revision DESC",
    )
    .bind(flow_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn find_revision_by_status(
    pool: &PgPool,
    flow_id: i64,
    status: &str,
) -> Result<Option<FlowRevisionRow>> {
    let row = sqlx::query_as::<_, FlowRevisionRow>(
        "SELECT id, flow_id, revision, definition, status, validation, created_by,
                created_at, published_at
           FROM flow_revisions WHERE flow_id = $1 AND status = $2
          ORDER BY revision DESC LIMIT 1",
    )
    .bind(flow_id)
    .bind(status)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 写入草稿：没有就建，有就就地更新。
///
/// 草稿是可改的，这是它与已发布修订的根本区别。发布之后的下一次编辑会开出新的一版草稿。
pub async fn upsert_draft(pool: &PgPool, input: &DraftInput<'_>) -> Result<FlowRevisionRow> {
    let mut tx = pool.begin().await?;

    let flow = sqlx::query_as::<_, FlowRow>(
        "INSERT INTO flows (name, description) VALUES ($1, $2)
         ON CONFLICT (name) DO UPDATE
            SET description = EXCLUDED.description, updated_at = now()
         RETURNING id, name, description, published_revision",
    )
    .bind(input.name)
    .bind(input.description)
    .fetch_one(&mut *tx)
    .await?;

    // 已有草稿就更新它；没有则开新的一版（修订号接在最大值之后）
    let existing = sqlx::query_as::<_, FlowRevisionRow>(
        "SELECT id, flow_id, revision, definition, status, validation, created_by,
                created_at, published_at
           FROM flow_revisions WHERE flow_id = $1 AND status = 'draft'",
    )
    .bind(flow.id)
    .fetch_optional(&mut *tx)
    .await?;

    let row = match existing {
        Some(draft) => {
            sqlx::query_as::<_, FlowRevisionRow>(
                "UPDATE flow_revisions
                    SET definition = $2, validation = $3, created_by = $4
                  WHERE id = $1
              RETURNING id, flow_id, revision, definition, status, validation, created_by,
                        created_at, published_at",
            )
            .bind(draft.id)
            .bind(input.definition)
            .bind(input.validation)
            .bind(input.created_by)
            .fetch_one(&mut *tx)
            .await?
        }
        None => {
            sqlx::query_as::<_, FlowRevisionRow>(
                "INSERT INTO flow_revisions (flow_id, revision, definition, status, validation, created_by)
                 SELECT $1, COALESCE(MAX(revision), 0) + 1, $2, 'draft', $3, $4
                   FROM flow_revisions WHERE flow_id = $1
              RETURNING id, flow_id, revision, definition, status, validation, created_by,
                        created_at, published_at",
            )
            .bind(flow.id)
            .bind(input.definition)
            .bind(input.validation)
            .bind(input.created_by)
            .fetch_one(&mut *tx)
            .await?
        }
    };

    tx.commit().await?;
    Ok(row)
}

/// 发布草稿：草稿转 published，上一版已发布的转 archived。
///
/// 返回发布后的那一条。没有草稿时报 [`FlowStateError::NoDraft`]。
pub async fn publish_draft(pool: &PgPool, name: &str) -> Result<FlowRevisionRow> {
    let mut tx = pool.begin().await?;

    let flow = sqlx::query_as::<_, FlowRow>(
        "SELECT id, name, description, published_revision FROM flows WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| FlowStateError::FlowNotFound(name.to_string()))?;

    let draft = sqlx::query_as::<_, FlowRevisionRow>(
        "SELECT id, flow_id, revision, definition, status, validation, created_by,
                created_at, published_at
           FROM flow_revisions WHERE flow_id = $1 AND status = 'draft'",
    )
    .bind(flow.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| FlowStateError::NoDraft(name.to_string()))?;

    // 上一版留档：不再是「当前生效」，但要能查到「上一版长什么样」
    sqlx::query(
        "UPDATE flow_revisions SET status = 'archived'
          WHERE flow_id = $1 AND status = 'published'",
    )
    .bind(flow.id)
    .execute(&mut *tx)
    .await?;

    let published = sqlx::query_as::<_, FlowRevisionRow>(
        "UPDATE flow_revisions SET status = 'published', published_at = now()
          WHERE id = $1
      RETURNING id, flow_id, revision, definition, status, validation, created_by,
                created_at, published_at",
    )
    .bind(draft.id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("UPDATE flows SET published_revision = $2, updated_at = now() WHERE id = $1")
        .bind(flow.id)
        .bind(published.revision)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(published)
}

/// 改名。**只对从未发布过的 flow 开放**。
///
/// 名字是身份：触发器、调用方 URL、MCP 工具都按名字寻址。发布过的 flow
/// 一旦改名，这些引用会全部悄悄失效——所以那条线划在「发布」上，不在「草稿」上。
///
/// 新名字撞上已有 flow 时由数据库唯一约束兜底，翻译成 [`crate::StoreError::Invalid`]。
pub async fn rename_flow(pool: &PgPool, old_name: &str, new_name: &str) -> Result<FlowRow> {
    // 同名请求是幂等短路：名字没变就没有引用失效，已发布过的 flow 发起
    // 同名「改名」也放行——它和不同名请求的 400 不对称，但行为无害
    if old_name == new_name {
        return find_flow(pool, old_name)
            .await?
            .ok_or(FlowStateError::FlowNotFound(old_name.to_string()))
            .map_err(Into::into);
    }

    let updated = sqlx::query_as::<_, FlowRow>(
        "UPDATE flows SET name = $2, updated_at = now()
          WHERE name = $1 AND published_revision = 0
      RETURNING id, name, description, published_revision",
    )
    .bind(old_name)
    .bind(new_name)
    .fetch_optional(pool)
    .await;

    match updated {
        Ok(Some(row)) => Ok(row),
        Ok(None) => {
            // 0 行只有两种成因，区分开才能给出对的提示
            match find_flow(pool, old_name).await? {
                None => Err(FlowStateError::FlowNotFound(old_name.to_string()).into()),
                Some(_) => Err(crate::StoreError::Invalid(format!(
                    "flow {old_name} 已发布过，名字不能改：调用方与触发器都按名字引用它"
                ))),
            }
        }
        Err(err) => {
            if err
                .as_database_error()
                .is_some_and(|db| db.is_unique_violation())
            {
                return Err(crate::StoreError::Invalid(format!(
                    "flow {new_name} 已存在"
                )));
            }
            Err(err.into())
        }
    }
}

/// 删除草稿（放弃未发布的改动）。
pub async fn discard_draft(pool: &PgPool, name: &str) -> Result<bool> {
    let affected = sqlx::query(
        "DELETE FROM flow_revisions
          WHERE status = 'draft'
            AND flow_id = (SELECT id FROM flows WHERE name = $1)",
    )
    .bind(name)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// 删除整条 flow（连同全部修订与执行记录，由外键级联）。
pub async fn delete_flow(pool: &PgPool, name: &str) -> Result<bool> {
    let affected = sqlx::query("DELETE FROM flows WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}
