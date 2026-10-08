//! 超限载荷的引用通道。
//!
//! 信封内联放不下（>4MB）的载荷落到这里，信封里只留一个 `PayloadRef`。这不是「大文件
//! 存储」，而是**中转**：带 TTL，默认 1 小时，过期即清。「不会长期保存」是设计的一部分
//! ——中台对业务报文的原则一直是只留摘要（见 `docs/design.md` 的留存策略），引用通道
//! 是这条原则的延伸而不是例外。
//!
//! 存 PG 的 BYTEA 而不是对象存储：UAT 没有对象存储，为这一步单独引一套服务不划算。
//!
//! 为什么带 `sha256`：引用传递最危险的失败模式不是「取不到」，而是「取到了别人的」。
//! 摘要让拿到字节的一方能立刻发现自己拿错了，而不是把错误数据当成正确数据用下去。

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::Result;
use crate::model::{PayloadBlobMeta, PayloadBlobRow};

/// 写入后返回的句柄。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoredBlob {
    pub id: String,
    pub size: i64,
    pub sha256: String,
    pub expires_at: DateTime<Utc>,
}

/// 存一份超限载荷，返回它的引用句柄。
pub async fn put(pool: &PgPool, bytes: &[u8], ttl: Duration) -> Result<StoredBlob> {
    // ULID 而不是 UUID：它按时间单调，`ORDER BY id` 就等于按写入顺序，
    // 排查「这个引用是什么时候产生的」不必再查一列
    let id = ulid::Ulid::generate().to_string();
    let sha256 = hex_sha256(bytes);
    let expires_at =
        Utc::now() + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::hours(1));

    sqlx::query(
        "INSERT INTO payload_blobs (id, size, sha256, bytes, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&id)
    .bind(bytes.len() as i64)
    .bind(&sha256)
    .bind(bytes)
    .bind(expires_at)
    .execute(pool)
    .await?;

    Ok(StoredBlob {
        id,
        size: bytes.len() as i64,
        sha256,
        expires_at,
    })
}

/// 取回载荷字节。已过期或不存在都返回 `None`。
///
/// 过期判断放在**查询里**而不是读出来再比：这样调用方不可能因为忘了比时间而用上
/// 一份本该消失的数据。
pub async fn get(pool: &PgPool, id: &str) -> Result<Option<PayloadBlobRow>> {
    let row = sqlx::query_as::<_, PayloadBlobRow>(
        "SELECT id, size, sha256, bytes, created_at, expires_at
           FROM payload_blobs
          WHERE id = $1 AND expires_at > now()",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// 只取元信息，不拉字节。列表页和信封回填用它。
pub async fn meta(pool: &PgPool, id: &str) -> Result<Option<PayloadBlobMeta>> {
    let row = sqlx::query_as::<_, PayloadBlobMeta>(
        "SELECT id, size, sha256, created_at, expires_at
           FROM payload_blobs
          WHERE id = $1 AND expires_at > now()",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// 清理过期载荷，返回清理条数。
///
/// 这个清理**必须按时跑**：TTL 只是让数据「不再可读」，不等于「不再占空间」。
/// 少了这一步，BYTEA 会一直在磁盘上躺着。
pub async fn purge_expired(pool: &PgPool, now: DateTime<Utc>) -> Result<u64> {
    let affected = sqlx::query("DELETE FROM payload_blobs WHERE expires_at < $1")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(affected)
}

/// 小写十六进制的 SHA-256。
fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let digest = Sha256::digest(bytes);
    // 手工转十六进制而不是引 `hex`：就这一处用途，一个 8 行的循环比多一个依赖划算
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 摘要与已知向量一致() {
        // 空串的 SHA-256 是一个广为人知的常量，用它确认我们没把字节序搞反
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(hex_sha256(b"").len(), 64, "十六进制形态是 64 个字符");
    }

    #[test]
    fn 不同内容摘要不同() {
        assert_ne!(hex_sha256(b"a"), hex_sha256(b"b"));
    }
}
