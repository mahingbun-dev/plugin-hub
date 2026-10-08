//! plugin-hub 持久化层：PostgreSQL（sqlx）+ 内嵌迁移。
//!
//! **用运行时 SQL 而非编译期宏**（`sqlx::query_as` 而非 `sqlx::query_as!`）：
//! 编译期宏要求构建时能连上数据库、或维护一份 `.sqlx` 离线缓存，会让「本地 PG 容器
//! 没起就编译不过」，且每次改 SQL 都要重新生成缓存。这里的取舍是放弃编译期校验，
//! 由**针对真实 PostgreSQL 的集成测试**兜住 schema 与代码的偏差——偏差会在测试里
//! 立刻暴露，而不是等到注册插件时才炸。

pub mod dead_letters;
pub mod flows;
pub mod idempotency;
pub mod instances;
pub mod model;
pub mod payloads;
pub mod plugins;
pub mod rejections;
pub mod runs;
pub mod spans;
pub mod triggers;

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// 迁移内嵌自 `crates/hub-store/migrations/`，运行时不需要读文件系统（离线可用）。
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("数据库操作失败: {0}")]
    Db(#[from] sqlx::Error),

    #[error("数据库迁移失败: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// 请求本身不成立（如「没有草稿却要发布」）。
    ///
    /// 与 [`StoreError::Db`] 分开是有用的：前者是调用方改请求就能解决的，
    /// 后者是基础设施问题，两者的处置方式完全不同。
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// 持久化入口。
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// 连接数据库。`acquire_timeout` 刻意设短：池子取不到连接时应当尽快失败，
    /// 而不是把请求挂在池上——等待会一路传导成上游的超时，错误也就失去了归属。
    ///
    /// （这条超时原是为「UAT 的 PG 跨机访问」设的；现在 PG 已是部署自带的容器、
    /// 走宿主回环，理由不再成立，但「快速失败」本身仍然是对的，故保留原值。）
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 执行迁移。
    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}
