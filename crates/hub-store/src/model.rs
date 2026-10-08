//! 数据行类型。与 `migrations/` 里的表结构一一对应。

use chrono::{DateTime, Utc};

/// 逻辑插件。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct PluginRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub owner: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 插件版本。`manifest` 与 `descriptor` 都是 proto 字节。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct PluginVersionRow {
    pub id: i64,
    pub plugin_id: i64,
    pub version: String,

    /// proto 编码的 `hub.v1.PluginManifest`
    ///
    /// 对外不给原始字节：JSON 里会变成一长串数字，对人有百害无一利。
    /// 需要看内容就解成 manifest 结构再出。
    #[serde(skip_serializing)]
    pub manifest: Vec<u8>,

    /// 注册时提交的 `FileDescriptorSet`，作为后续版本的兼容性基线
    #[serde(skip_serializing)]
    pub descriptor: Vec<u8>,

    pub created_at: DateTime<Utc>,
}

/// 一次注册拒绝的留痕。同一 (插件, 实例, 拒绝码) 的重试收敛在一行里，
/// `count` 是累计次数——表结构见 `migrations/0005_registration_rejections.sql`。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct RegistrationRejectionRow {
    pub plugin_name: String,
    pub instance_id: String,
    pub code: i32,
    pub version: String,
    pub message: String,
    pub detail: String,
    pub source_ip: String,
    pub count: i64,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// 实例。同一版本可以有多个副本。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct PluginInstanceRow {
    pub id: i64,
    pub version_id: i64,
    pub instance_id: String,
    pub advertise_addr: String,
    pub status: String,

    /// 注册来源 IP。插件面按设计不鉴权，审计至少能追到来源。
    pub source_ip: Option<String>,

    pub registered_at: DateTime<Utc>,
    pub last_heartbeat_at: DateTime<Utc>,
}

/// 版本声明的消息类型。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct ContractRow {
    pub version_id: i64,
    /// `produces` / `consumes`
    pub direction: String,
    pub fq_name: String,
}

impl ContractRow {
    pub const PRODUCES: &'static str = "produces";
    pub const CONSUMES: &'static str = "consumes";
}

/// 版本声明的 MCP 工具。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct ToolRow {
    pub version_id: i64,
    pub name: String,
    pub description: String,
    pub input_schema_json: String,
    pub requires_approval: bool,
}

/// **此刻真的能调**的插件工具，供 MCP 工具面聚合。
///
/// 与 [`ToolRow`] 的两点不同，正是「能调」这个限定：
///
/// - 每个插件只算**最新登记版本**，且该版本**有在线实例**（实例行被摘除即下线，
///   见 `instances::sweep_stale`）。多版本共存时旧版本的工具不再兜底下发——
///   与调用路由（不指定版本即路由最新登记版本）严格一致
/// - 工具名要拼成 MCP 的 `插件名__工具名`，同插件内同名工具注册时已被拦住，
///   天然不会撞名
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct OnlineToolRow {
    pub plugin_name: String,
    pub name: String,
    pub description: String,
    pub input_schema_json: String,
    pub requires_approval: bool,
}

/// 插件总览（列表页用）：插件 + 版本数 + 在线实例数。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct PluginOverviewRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub owner: String,
    pub version_count: i64,
    pub instance_count: i64,
}

/// flow 的一次修订。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct FlowRevisionRow {
    pub id: i64,
    pub flow_id: i64,
    pub revision: i32,

    /// 编排定义（`hub_flow::FlowDefinition` 的 JSON 形态）
    pub definition: serde_json::Value,

    /// `draft` / `published` / `archived`
    pub status: String,

    /// 保存时的校验结果快照
    pub validation: Option<serde_json::Value>,

    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
}

/// 某个插件某个版本的契约聚合，供编排校验使用。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PluginVersionContractsRow {
    pub plugin: String,
    pub version: String,
    /// 该版本生产的消息全限定名
    pub produces: Vec<String>,
    /// 该版本消费的消息全限定名
    pub consumes: Vec<String>,
}

/// 一次 flow 执行。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct RunRow {
    pub run_id: String,
    pub flow_id: i64,
    pub flow_revision: i32,
    pub trace_id: String,
    pub subject: Option<serde_json::Value>,
    pub trigger: Option<serde_json::Value>,
    /// `running` / `succeeded` / `failed` / `rejected`
    pub status: String,
    pub input_summary: Option<String>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// 一次执行里的单个节点。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct RunNodeRow {
    pub id: i64,
    pub run_id: String,
    pub node_id: String,
    pub plugin: String,
    pub version: String,
    pub instance_id: Option<String>,
    pub attempt: i32,
    /// `succeeded` / `failed` / `rejected` / `skipped`
    pub status: String,
    pub duration_ms: Option<i64>,
    pub io_summary: Option<String>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// 调用链上的一个 span。
///
/// 字段按 OTel 的形状设计——将来要接 OTel 后端时导出是机械转换，不用改数据模型。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct SpanRow {
    pub id: i64,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub run_id: Option<String>,
    pub node_id: Option<String>,
    pub name: String,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i64,
    /// `ok` / `error` / `rejected`
    pub status: String,
    pub attributes: Option<serde_json::Value>,
}

/// 一条重投耗尽的死信。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct DeadLetterRow {
    pub id: i64,

    /// 来自哪条 Stream 的哪个消息 id
    pub stream: String,
    pub stream_id: String,

    pub run_id: Option<String>,
    pub flow_name: Option<String>,
    pub node_id: Option<String>,

    pub attempts: i32,
    pub error: String,

    /// 载荷摘要（类型、字节数、meta 键），**不是全量报文**
    pub payload_summary: Option<String>,

    /// 重放成了哪次新执行。为空表示还没重放过
    pub replayed_run_id: Option<String>,
    pub replayed_at: Option<DateTime<Utc>>,

    pub first_seen_at: DateTime<Utc>,
    pub last_attempt_at: DateTime<Utc>,
}

/// 超限载荷的引用。
///
/// `bytes` 刻意**不进 JSON 序列化**：这是字节流，编码进 JSON 只会变成一长串数字，
/// 对人有百害而无一利。要取内容就走 [`crate::payloads::get`]。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PayloadBlobRow {
    pub id: String,
    pub size: i64,
    pub sha256: String,

    pub bytes: Vec<u8>,

    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// 除了大小与摘要之外的引用元信息（列表页与信封里用，不拉字节）。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct PayloadBlobMeta {
    pub id: String,
    pub size: i64,
    pub sha256: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// 一个触发器。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct TriggerRow {
    pub id: i64,
    pub flow_id: i64,

    /// `cron` / `mq`
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
