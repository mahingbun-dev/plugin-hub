//! MCP 工具面：让 agent 能看见并调用中台与插件。
//!
//! 按设计，agent 的能力面由**插件 manifest 里声明的工具**自动聚合而来（M2 起），
//! M1 先落管理类工具与 `invoke_plugin`——后者是「agent 与插件通讯」这条诉求的最小闭环。
//!
//! **这个面要 `hub:invoke` 权限位**。它由 `hub_server::http_app` 挂上鉴权中间件
//! （`hub_api::authz`），挂载点在 `merge` 之后——挂在内层 `api_router` 上的话，
//! `merge` 进来的路由收不到，这个面就会静默地没有鉴权。那件事发生过一次，
//! 见 `hub_api::router_with_extras` 的说明。

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::request::Parts;
use hub_core::AuthenticatedSubject;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use std::sync::Mutex;
use std::time::Instant;

use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ClientResult,
    ContentBlock, ElicitRequest, ElicitRequestParams, ElicitationAction, ElicitationSchema,
    ErrorCode, ErrorData, Implementation, ListToolsResult, PaginatedRequestParams,
    PrimitiveSchemaDefinition, ServerCapabilities, ServerConfig, ServerRequest, StringSchema, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use hub_engine::{AsyncExecutor, FlowService, InvokeOutcome, Invoker};
use hub_proto::v1::{Envelope, PayloadType, PluginManifest, Subject, SubjectKind};
use hub_store::Store;
use hub_store::model::OnlineToolRow;
use prost::Message as _;
use ulid::Ulid;

/// agent 调用插件时的默认预算
const DEFAULT_TIMEOUT_MS: i64 = 30_000;

/// 人工确认（elicitation）的等待上限。
///
/// 用户要在确认框里读一张决策卡——几秒给不够；但一次没人理的确认也不能
/// 拖死整条工具调用。超时按「未获同意」处理（fail-safe），卡上的确认令牌
/// 不会随结果下发。
const DEFAULT_ELICITATION_TIMEOUT: Duration = Duration::from_secs(120);

/// 登录态的默认有效期。
///
/// auth 插件 `login` 返回的 `expiresAt` 是它**自身缓存条目**的 TTL，不是
/// masToken 的真实有效期（那边文档写明了这个坑）。masToken 真实寿命未知，
/// 所以这里取一个保守值：到期后下一次插件调用重新弹登录框，而不是把
/// 「可能已过期的登录态」当真。
const DEFAULT_LOGIN_TTL: Duration = Duration::from_secs(8 * 60 * 60);

/// 信封 meta 里「调用方登录态」的契约键名。
///
/// hub 与插件之间的约定：dc-dict 这类需要下游登录态的插件从这里取
/// masToken 去调 UAT 的 DC 接口（hub 透传登录态，插件不持任何凭证）。
/// 改名等于断契约，必须与全部消费插件同步改。它的允许活动范围只有
/// 内存缓存与发往插件的那份信封——审计 / 日志 / 死信一律不得出现
/// （见 `redact_mas_token` 的边界兜底）。
const HUB_MAS_TOKEN_META: &str = "hub.mas_token";

/// MCP 服务端。
#[derive(Clone)]
pub struct HubMcp {
    store: Store,
    invoker: Invoker,
    flows: FlowService,

    /// 异步链执行器。`Option` 与 HTTP 面同理：没配总线时异步触发与死信重放
    /// 明确回「这个能力没开」，而不是让 agent 以为消息已经发出去了。
    async_exec: Option<AsyncExecutor>,
    tool_router: ToolRouter<Self>,

    /// MCP 面的 Host 白名单；`None` = 沿用 rmcp 的默认（只接受本机 Host）。
    allowed_hosts: Option<Vec<String>>,

    /// 人工确认（elicitation）的等待上限，见 [`DEFAULT_ELICITATION_TIMEOUT`]。
    elicitation_timeout: Duration,

    /// 登录闸门开关（默认关）。开 = 插件工具调用前先弹账号密码换登录态。
    login_gate_enabled: bool,
    /// 登录用的鉴权插件名。
    login_plugin: String,
    /// 已建立的登录身份（含 masToken），见 [`LoginCache`]。
    login_cache: LoginCache,
}

/// 一次成功登录建立的调用身份。
#[derive(Clone)]
struct CachedIdentity {
    subject: Subject,
    /// auth 插件 `login` 返回的 masToken。hub 不持久化任何凭证：它只活在
    /// 内存缓存与发往插件的那份信封 meta 里，到期随整条身份一起作废。
    mas_token: Option<String>,
    expires_at: Instant,
}

/// 登录态缓存：**进程级单槽**，最近一次登录的账号代表当前调用方。
///
/// 多人各持会话的身份隔离需要会话键（rmcp 的 handler 层拿不到 session id），
/// 是一个已知的 v1 取舍——内部部署下，登录行为本身就是区分人。`Arc<Mutex<_>>`
/// 是因为 HubMcp 是 Clone，缓存必须跨克隆共享。登录闸门（elicitation）与
/// 显式 `login` 工具两条入口都汇入 [`LoginCache::store`]，语义只有一份。
#[derive(Clone)]
struct LoginCache(Arc<Mutex<Option<CachedIdentity>>>);

impl LoginCache {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// 把一次成功登录存进缓存，返回代表当前调用方的 subject。
    ///
    /// 单槽语义：后登录者**整体顶掉**先登录者（subject 与 masToken 一起换），
    /// 不是合并——不存在「A 的身份带 B 的 token」的拼装态。
    fn store(&self, identity: EstablishedIdentity) -> Subject {
        let expires_at = identity
            .expires_at_ms
            .and_then(chrono::DateTime::from_timestamp_millis)
            .and_then(|at| at.signed_duration_since(chrono::Utc::now()).to_std().ok())
            // expiresAt 只是 auth 插件缓存条目的 TTL；缺失或已过期都退回保守值
            .filter(|ttl| !ttl.is_zero())
            .unwrap_or(DEFAULT_LOGIN_TTL);
        let subject = identity.subject.clone();
        *self.0.lock().expect("登录缓存锁中毒") = Some(CachedIdentity {
            subject,
            mas_token: identity.mas_token,
            expires_at: Instant::now() + expires_at,
        });
        identity.subject
    }

    /// 有效期内的一条缓存身份；过期即清除（读时惰性淘汰）。
    fn valid(&self) -> Option<CachedIdentity> {
        let mut slot = self.0.lock().expect("登录缓存锁中毒");
        if let Some(cached) = slot.as_ref().filter(|c| c.expires_at > Instant::now()) {
            return Some(cached.clone());
        }
        *slot = None;
        None
    }

    /// 当前调用主体的 subject（登录闸门复用判定用）。
    fn valid_subject(&self) -> Option<Subject> {
        self.valid().map(|c| c.subject)
    }

    /// 当前登录态的 masToken（信封 meta 注入用）。没有登录态、login 未返回
    /// token、或值为空串时都返回 `None`——调用方以「缺键」识别匿名调用，
    /// 绝不注入空壳键。
    fn valid_mas_token(&self) -> Option<String> {
        self.valid()
            .and_then(|c| c.mas_token)
            .filter(|token| !token.is_empty())
    }
}

/// 登录闸门的结论。
enum LoginOutcome {
    /// 闸门未启用：调用以匿名进行（历史行为，`HUB_MCP_LOGIN_GATE` 未开）。
    Disabled,
    /// 已建立（或缓存命中）身份，随信封下发给插件。
    Authenticated(Subject),
    /// 无法建立身份：载荷是给调用方的拒绝卡。
    Denied(serde_json::Value),
}

impl HubMcp {
    pub fn new(store: Store, invoker: Invoker, flows: FlowService) -> Self {
        Self {
            store,
            invoker,
            flows,
            async_exec: None,
            tool_router: Self::tool_router(),
            allowed_hosts: None,
            elicitation_timeout: DEFAULT_ELICITATION_TIMEOUT,
            login_gate_enabled: false,
            login_plugin: "auth".to_string(),
            login_cache: LoginCache::new(),
        }
    }

    /// 配上异步链执行器，开启异步触发与死信重放。
    pub fn with_async(mut self, exec: AsyncExecutor) -> Self {
        self.async_exec = Some(exec);
        self
    }

    /// 开关登录闸门（默认关；生产由 `HUB_MCP_LOGIN_GATE` 驱动）。
    pub fn with_login_gate(mut self, enabled: bool) -> Self {
        self.login_gate_enabled = enabled;
        self
    }

    /// 指定登录用的鉴权插件名（默认 `auth`）。
    pub fn with_login_plugin(mut self, plugin: impl Into<String>) -> Self {
        self.login_plugin = plugin.into();
        self
    }

    /// 调整人工确认的等待上限（默认 [`DEFAULT_ELICITATION_TIMEOUT`]）。
    /// 测试里调小，免得一条用例等两分钟。
    pub fn with_elicitation_timeout(mut self, timeout: Duration) -> Self {
        self.elicitation_timeout = timeout;
        self
    }

    /// 设置 MCP 面的 Host 白名单（防 DNS rebinding）。
    ///
    /// **不调用即沿用 rmcp 的默认**——只接受 `localhost` / `127.0.0.1` / `::1`，
    /// 本机直连与 vite 代理都够用。中台经反向代理对外时**必须**调用，否则反向
    /// 代理传下来的外部 Host 会被判成 DNS rebinding 而 403。
    ///
    /// 传 `None` 或空 `Vec` 都落到「用默认」：rmcp 把**空列表**当作「放行全部」，
    /// 不能让「没配」从这条路悄悄变成「不设防」。
    pub fn with_allowed_hosts(mut self, hosts: Option<Vec<String>>) -> Self {
        self.allowed_hosts = hosts.filter(|h| !h.is_empty());
        self
    }

    fn async_exec(&self) -> Result<&AsyncExecutor, ErrorData> {
        self.async_exec
            .as_ref()
            .ok_or_else(|| internal("本实例未启用异步链（总线未装配）"))
    }

    /// 构造可挂到 axum 上的 `/mcp` 路由。
    pub fn router(self, shutdown: CancellationToken) -> Router {
        let mut config = StreamableHttpServerConfig::default().with_cancellation_token(shutdown);
        if let Some(hosts) = self.allowed_hosts.clone() {
            config = config.with_allowed_hosts(hosts);
        }
        let service: StreamableHttpService<Self, LocalSessionManager> =
            StreamableHttpService::new(move || Ok(self.clone()), Default::default(), config);
        // **关于 panic 隔离的取舍**：handler 的意外 panic 默认会炸掉整条 HTTP
        // 连接（客户端看到 connection reset）。标准修法是 `catch_unwind` 一层，
        // 但 workspace 没有任何 crate 直接依赖 `futures-util`，显式引入会改
        // `Cargo.lock` 的依赖数组——UAT 的 vendor 快照按 lock 的 md5 校验，
        // lock 一变 CI 就要重跑 `scripts/cargo-vendor-sync.sh`。这个缺口
        // （应当修 bug 而不是屏蔽 panic，且引入有部署管线成本）留待下次
        // 顺路升依赖时一并补上；插件故障与解析故障已经全部以错误卡表达。
        Router::new().nest_service("/mcp", service)
    }
}

// ---------------------------------------------------------------- 工具

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPluginArgs {
    /// 插件名
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeMessageArgs {
    /// 消息的全限定名，例如 wms.v1.OrderCreated
    pub fq_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRejectionsArgs {
    /// 按插件过滤；缺省返回全部
    #[serde(default)]
    pub plugin: Option<String>,

    /// 最多返回几条（默认 50，上限 200）
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InvokePluginArgs {
    /// 插件名
    pub plugin: String,

    /// 业务载荷，JSON 对象
    pub payload: serde_json::Value,

    /// 插件版本；缺省取最新版本
    #[serde(default)]
    pub version: Option<String>,

    /// 幂等键。重试时带上同一个值，便于下游去重
    #[serde(default)]
    pub message_id: Option<String>,

    /// 超时预算（毫秒）
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

/// [`Self::login`] 工具的入参。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoginArgs {
    /// 平台账号
    pub account: String,
    /// 平台密码。只用于经鉴权插件换取登录态：不落缓存、不写日志、
    /// 不出现在任何回执里。支持 elicitation 的客户端会走弹窗，
    /// 密码根本不经过模型上下文——这个工具是给**不支持弹窗**的客户端的。
    pub password: String,
}

#[tool_router(router = tool_router)]
impl HubMcp {
    /// 列出所有已注册插件及其版本数、在线实例数。
    #[tool(description = "列出所有已注册的插件（含版本数与在线实例数）")]
    pub async fn list_plugins(&self) -> Result<CallToolResult, ErrorData> {
        let plugins = hub_store::plugins::list_plugins(self.store.pool())
            .await
            .map_err(internal)?;
        Ok(json_result(&plugins))
    }

    /// 查询单个插件的详情：逐版本的契约、MCP 工具与实例。
    ///
    /// agent 靠它回答「这个插件到底能干什么、吃什么类型、现在跑在哪」。
    /// 每个版本带 `online`（有无在线实例）与 `serving`（工具是否正被 tools/list
    /// 下发——最新登记版本且有在线实例；旧版本即使实例还在也是 false）。
    /// `invokes` 是 manifest 声明的互调授权名单，人工审计「谁被授权调谁」用。
    #[tool(
        description = "查询单个插件的详情：逐版本给出契约（消费/生产哪些消息类型）、MCP 工具、互调授权名单（invokes）与在线实例；online 表示该版本有在线实例，serving 表示该版本的工具正在被工具面下发"
    )]
    pub async fn get_plugin(
        &self,
        Parameters(args): Parameters<GetPluginArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let detail = hub_store::plugins::describe_plugin(self.store.pool(), &args.name)
            .await
            .map_err(internal)?
            .ok_or_else(|| not_found(format!("插件 {} 未注册", args.name)))?;

        // 「下发中」与工具面 tools/list 的口径一致：最新登记版本 + 有在线实例。
        // agent 拿它就能解释「为什么这个版本的工具我调不到」。
        let latest_registered = detail.versions.iter().map(|v| v.id).max();

        let mut versions = Vec::with_capacity(detail.versions.len());
        for version in &detail.versions {
            let contracts = hub_store::plugins::contracts_of(self.store.pool(), version.id)
                .await
                .map_err(internal)?;
            let tools = hub_store::plugins::tools_of(self.store.pool(), version.id)
                .await
                .map_err(internal)?;
            let instances =
                hub_store::instances::instances_of_version(self.store.pool(), version.id)
                    .await
                    .map_err(internal)?;
            let online = !instances.is_empty();
            let serving = online && Some(version.id) == latest_registered;

            // invokes（互调权限名单）只存在 manifest 字节里，解出来给人工审计用：
            // 「这个插件被授权调谁」在 MCP 面一眼可见。manifest 注册时已过 registry
            // 的自洽校验，解码失败只能是存储损坏——回落空列表而不是让整个详情
            // 工具报错，坏一个版本不该挡住看其它版本。
            let invokes = PluginManifest::decode(version.manifest.as_slice())
                .map(|m| m.invokes)
                .unwrap_or_default();

            versions.push(serde_json::json!({
                "version": version.version,
                "online": online,
                "serving": serving,
                "contracts": contracts
                    .iter()
                    .map(|c| serde_json::json!({
                        "direction": c.direction,
                        "fq_name": c.fq_name,
                    }))
                    .collect::<Vec<_>>(),
                "tools": tools
                    .iter()
                    .map(|t| serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "requires_approval": t.requires_approval,
                    }))
                    .collect::<Vec<_>>(),
                "invokes": invokes,
                "instances": instances
                    .iter()
                    .map(|i| serde_json::json!({
                        "instance_id": i.instance_id,
                        "advertise_addr": i.advertise_addr,
                        "status": i.status,
                    }))
                    .collect::<Vec<_>>(),
            }));
        }

        Ok(json_result(&serde_json::json!({
            "name": detail.plugin.name,
            "description": detail.plugin.description,
            "owner": detail.plugin.owner,
            "versions": versions,
        })))
    }

    /// 列出全部在线实例。
    #[tool(description = "列出全部在线插件实例及其可达地址")]
    pub async fn list_instances(&self) -> Result<CallToolResult, ErrorData> {
        let instances = hub_store::instances::list_instances(self.store.pool())
            .await
            .map_err(internal)?;
        Ok(json_result(&instances))
    }

    /// 最近的注册拒绝留痕。
    ///
    /// 「插件离线了」的排查第一站：容器活着但注册被中台拒死时（典型如改了契约
    /// 没升版本号的 VERSION_CONFLICT），实例表里根本没有这一行，拒绝原因
    /// 唯一可查的地方就是留痕表。
    #[tool(
        description = "列出最近的插件注册拒绝（谁被拒、拒绝码、原因、已重试多少次）——插件离线但容器还活着时先看这里"
    )]
    pub async fn list_register_rejections(
        &self,
        Parameters(args): Parameters<ListRejectionsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(50).clamp(1, 200);
        let rows = hub_store::rejections::list(self.store.pool(), args.plugin.as_deref(), limit)
            .await
            .map_err(internal)?;
        // MCP 面出精简视图，与 get_plugin 同一取舍：给 agent 看的字段挑出来，
        // code_name 一并给出——拒绝码对人有意义的是名字不是数字
        let views: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "plugin_name": r.plugin_name,
                    "instance_id": r.instance_id,
                    "code_name": hub_proto::rejection_code_name(r.code),
                    "version": r.version,
                    "message": r.message,
                    "detail": r.detail,
                    "count": r.count,
                    "first_seen_at": r.first_seen_at,
                    "last_seen_at": r.last_seen_at,
                })
            })
            .collect();
        Ok(json_result(&views))
    }

    /// 某个消息类型被谁生产、被谁消费。
    #[tool(description = "查询某个消息类型被哪些插件生产、被哪些插件消费（改契约前的影响面分析）")]
    pub async fn describe_message(
        &self,
        Parameters(args): Parameters<DescribeMessageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let producers =
            hub_store::plugins::versions_by_fq_name(self.store.pool(), &args.fq_name, "produces")
                .await
                .map_err(internal)?;
        let consumers =
            hub_store::plugins::versions_by_fq_name(self.store.pool(), &args.fq_name, "consumes")
                .await
                .map_err(internal)?;

        Ok(json_result(&serde_json::json!({
            "fq_name": args.fq_name,
            "producers": producers,
            "consumers": consumers,
        })))
    }

    /// 列出已发布的编排。
    #[tool(description = "列出已发布的编排（flow）及其当前生效的修订号")]
    pub async fn list_flows(&self) -> Result<CallToolResult, ErrorData> {
        let flows = self.flows.list_published().await.map_err(internal)?;
        Ok(json_result(
            &flows
                .into_iter()
                .map(|(flow, revision)| {
                    serde_json::json!({
                        "name": flow.name,
                        "description": flow.description,
                        "published_revision": flow.published_revision,
                        "nodes": revision.definition.get("nodes").cloned().unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>(),
        ))
    }

    /// 查询一条编排的详情。
    #[tool(description = "查询一条编排的详情：编排定义、当前生效的版本号、以及历史修订")]
    pub async fn get_flow(
        &self,
        Parameters(args): Parameters<GetFlowArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let flow = hub_store::flows::find_flow(self.store.pool(), &args.name)
            .await
            .map_err(internal)?
            .ok_or_else(|| not_found(format!("flow {} 未定义", args.name)))?;

        let draft = hub_store::flows::find_draft(self.store.pool(), flow.id)
            .await
            .map_err(internal)?;
        let published = hub_store::flows::find_published(self.store.pool(), flow.id)
            .await
            .map_err(internal)?;

        Ok(json_result(&serde_json::json!({
            "name": flow.name,
            "description": flow.description,
            "published_revision": flow.published_revision,
            "draft": draft.map(|d| d.definition),
            "published": published.map(|p| p.definition),
        })))
    }

    /// 保存编排草稿。
    ///
    /// agent 能改草稿、**不能发布**——这个边界是刻意的（见 docs/design.md）。
    #[tool(
        description = "保存编排草稿（flow 定义）。注意：agent 只能改草稿、不能发布——发布决定生产流量走向，需要人来做。保存时会做静态校验（DAG 无环、上下游契约能否接上、插件是否存在），问题会一并返回"
    )]
    pub async fn save_flow_draft(
        &self,
        Parameters(args): Parameters<SaveFlowDraftArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // 收 JSON 再解析，而不是让工具签名直接带 FlowDefinition：
        // 后者要求 hub-flow 依赖 schemars，为了给 agent 看 schema 而把领域 crate
        // 绑上 schema 生成库不值当——形状写在工具描述里就够了
        let definition: hub_flow::FlowDefinition = serde_json::from_value(args.definition.clone())
            .map_err(|err| invalid_params(format!("编排定义无法解析: {err}")))?;

        let outcome = self
            .flows
            .save_draft(&args.name, &args.description, &definition, &args.created_by)
            .await
            .map_err(|err| invalid_params(err.to_string()))?;

        Ok(json_result(&serde_json::json!({
            "flow": args.name,
            "revision": outcome.revision.revision,
            "status": outcome.revision.status,
            // 有问题也存下来了；blocked 为 true 时不能发布
            "blocked": outcome.blocked,
            "issues": outcome.issues,
        })))
    }

    /// 触发一条已发布的编排。
    #[tool(
        description = "触发一条已发布的编排（flow）：按顺序跑完其中的插件节点，返回每个节点的状态、耗时与整条链的输出。中间节点失败会短路下游"
    )]
    pub async fn trigger_flow(
        &self,
        Parameters(args): Parameters<TriggerFlowArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if timeout_ms <= 0 {
            return Err(invalid_params("timeout_ms 必须为正数"));
        }

        let payload = hub_proto::encode_payload(&args.payload).map_err(invalid_params)?;
        let envelope = Envelope {
            message_id: args
                .message_id
                .clone()
                .unwrap_or_else(|| generate_message_id(&args.flow)),
            trace_id: Ulid::generate().to_string(),
            deadline_ms: chrono::Utc::now().timestamp_millis() + timeout_ms,
            r#type: PayloadType::Request as i32,
            payload: Some(payload),
            ..Default::default()
        };

        let trigger = serde_json::json!({"kind": "mcp", "tool": "trigger_flow"});
        let outcome = self
            .flows
            .trigger(&args.flow, envelope, Some(trigger))
            .await
            .map_err(|err| invalid_params(err.to_string()))?;

        let run = outcome.run;
        let output = run
            .output
            .as_ref()
            .and_then(|env| env.payload.as_ref().and_then(hub_proto::decode_payload));

        Ok(json_result(&serde_json::json!({
            "run_id": run.run_id,
            "trace_id": run.trace_id,
            "flow": args.flow,
            "flow_revision": outcome.flow_revision,
            "status": match run.status {
                hub_engine::RunStatus::Succeeded => "succeeded",
                hub_engine::RunStatus::Failed => "failed",
                hub_engine::RunStatus::Rejected => "rejected",
            },
            "elapsed_ms": run.elapsed_ms,
            "error": run.error,
            "nodes": run.nodes.iter().map(|n| serde_json::json!({
                "node_id": n.node_id,
                "plugin": n.plugin,
                "version": n.version,
                "status": match n.status {
                    hub_engine::NodeStatus::Succeeded => "succeeded",
                    hub_engine::NodeStatus::Failed => "failed",
                    hub_engine::NodeStatus::Rejected => "rejected",
                },
                "attempts": n.attempts,
                "duration_ms": n.duration_ms,
                "error": n.error,
            })).collect::<Vec<_>>(),
            "payload": output,
        })))
    }

    /// 列出执行记录。
    #[tool(description = "列出编排的执行记录（新到旧），可按 flow 名过滤")]
    pub async fn list_runs(
        &self,
        Parameters(args): Parameters<ListRunsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let rows = hub_store::runs::list_runs(
            self.store.pool(),
            args.flow.as_deref(),
            args.limit.unwrap_or(50),
        )
        .await
        .map_err(internal)?;
        Ok(json_result(&rows))
    }

    /// 查询一次执行的明细。
    #[tool(
        description = "查询一次执行的明细：每个节点的状态、耗时、尝试次数与错误。这是回答「这条数据卡在哪一跳」的地方"
    )]
    pub async fn get_run(
        &self,
        Parameters(args): Parameters<GetRunArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let detail = hub_store::runs::find_run_detail(self.store.pool(), &args.run_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| not_found(format!("执行记录 {} 不存在", args.run_id)))?;
        Ok(json_result(&detail))
    }

    /// 列出最近的调用链。
    #[tool(
        description = "列出最近的调用链（按 trace 聚合：每行的 span_count 与 error_count 一眼看出哪条链出过问题）"
    )]
    pub async fn list_traces(
        &self,
        Parameters(args): Parameters<ListTracesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let rows =
            hub_store::spans::list_recent_traces(self.store.pool(), args.limit.unwrap_or(50))
                .await
                .map_err(internal)?;
        Ok(json_result(&rows))
    }

    /// 查询一条调用链的全部 span。
    #[tool(
        description = "查询一条调用链的全部 span（按开始时间排开）：同层并发时能直接看出哪几个节点在同时跑、谁拖慢了整条链"
    )]
    pub async fn get_trace(
        &self,
        Parameters(args): Parameters<GetTraceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let spans = hub_store::spans::list_spans_by_trace(self.store.pool(), &args.trace_id)
            .await
            .map_err(internal)?;
        if spans.is_empty() {
            return Err(not_found(format!("trace {} 不存在", args.trace_id)));
        }
        Ok(json_result(&serde_json::json!({
            "trace_id": args.trace_id,
            "spans": spans,
        })))
    }

    /// 建立登录身份：登录闸门的**通用路径**。
    ///
    /// 支持 elicitation 的客户端（Claude CLI）在首次插件调用时会直接弹账号
    /// 密码表单，用不着这个工具；**不支持弹窗的客户端**（zcode / codex 等）
    /// 收到「需要登录」的指引卡后，由 agent 向用户要来凭证调它。两条路汇入
    /// 同一个身份缓存——登录一次，全部插件调用共享。
    #[tool(
        description = "建立登录身份：用平台账号密码经鉴权插件换取登录态（默认 8 小时内所有插件调用共享）。仅在收到「需要登录」的指引卡时调用；password 只用于换取登录态，不要在对话里复述"
    )]
    pub async fn login(
        &self,
        Parameters(args): Parameters<LoginArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let inner = self
            .login_via_auth_plugin(&args.account, &args.password)
            .await?;
        let identity = match inner {
            Ok(identity) => identity,
            Err(reason) => {
                return Ok(json_result(&serde_json::json!({
                    "status": "rejected",
                    "login": {"required": true, "outcome": "bad-credentials"},
                    "message": format!("登录失败：{reason}。请核对账号密码后重试。"),
                })));
            }
        };
        let subject = self.login_cache.store(identity);
        Ok(json_result(&serde_json::json!({
            "status": "handled",
            "authenticated": true,
            "user": subject.id,
            "scopes": subject.scopes,
            "note": "登录态已建立并缓存。请重试刚才被拒的插件调用。",
        })))
    }

    /// 调用插件业务能力。
    ///
    /// 载荷先过插件的校验器，通过才进插件体——校验拒绝时返回 `status=rejected`
    /// 与结构化问题（`path` / `message` / `severity`），照着改参数重试即可。
    ///
    /// 调用主体（`Envelope.subject`）取自这个请求的凭证：rmcp 把 HTTP 的
    /// `request::Parts`（含鉴权中间件写进去的身份）注入给处理函数，这里再装进信封。
    ///
    /// **必须是 `#[tool]`**：它曾经只是这个 `#[tool_router]` 块里的一个 pub 方法，
    /// 测试直接调 Rust 方法全绿，而 agent 经 MCP 根本看不到它。工具面是 agent 唯一的
    /// 入口，"方法存在"与"工具可达"是两回事。
    #[tool(
        description = "调用插件业务能力：载荷 JSON 对象，先过插件的校验器，通过才进插件体。校验不通过返回 status=rejected 与逐条问题——那是业务结果，不是工具错误；通过则返回 status=handled 与插件回传的载荷"
    )]
    pub async fn invoke_plugin(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<InvokePluginArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let subject = match self.login_gate_subject(&ctx, &args.plugin).await? {
            LoginOutcome::Disabled => subject_of(&ctx),
            LoginOutcome::Authenticated(subject) => Some(subject),
            LoginOutcome::Denied(card) => return Ok(json_result(&card)),
        };
        // 调用层故障（插件不可达 / 超时 / gRPC 错误）转成**结构化卡**而不是
        // JSON-RPC error：agent 拿到的是与业务结果同构、可读可对账的信息。
        let value = match self.invoke_plugin_value(subject, None, args).await {
            Ok(value) => value,
            Err(err) => invocation_error_card(&err.to_string()),
        };
        let value = self.human_approval_gate(&ctx, value).await?;
        Ok(json_result(&value))
    }

    /// 插件体调用的实现体。
    ///
    /// 与上面那条拆开，是因为**工具签名里带 `RequestContext` 就构造不出测试用的值**，
    /// 而「信封里到底带了什么身份」正是要断言的东西。`subject` 由 `#[tool]` 那条
    /// 从 HTTP 上下文里取出来。
    ///
    /// `tool` 是**经工具聚合**进来时的工具短名（如 `sql_query`），会写进信封的
    /// `meta["hub.tool"]`。直接走 `invoke_plugin` 时是 `None`——那条路上调用方
    /// 自己知道在调什么。
    pub async fn invoke_plugin_as(
        &self,
        subject: Option<Subject>,
        tool: Option<&str>,
        args: InvokePluginArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let value = self.invoke_plugin_value(subject, tool, args).await?;
        Ok(json_result(&value))
    }

    /// [`Self::invoke_plugin_as`] 的**载荷版**：返回还没渲染成 MCP 结果的
    /// 载荷 JSON，调用方（工具入口）可以先做人工确认闸门再渲染。
    async fn invoke_plugin_value(
        &self,
        subject: Option<Subject>,
        tool: Option<&str>,
        args: InvokePluginArgs,
    ) -> Result<serde_json::Value, ErrorData> {
        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if timeout_ms <= 0 {
            return Err(invalid_params("timeout_ms 必须为正数"));
        }

        let payload = hub_proto::encode_payload(&args.payload).map_err(invalid_params)?;

        let mut envelope = Envelope {
            message_id: args
                .message_id
                .clone()
                .unwrap_or_else(|| generate_message_id(&args.plugin)),
            deadline_ms: chrono::Utc::now().timestamp_millis() + timeout_ms,
            r#type: PayloadType::Request as i32,
            payload: Some(payload),
            // 调用主体，与 HTTP 面同源：身份来自鉴权中间件验过的凭证。
            // 未配鉴权时是 None——插件必须把空 subject 当作匿名，不是「没有限制」。
            subject,
            ..Default::default()
        };

        // 工具名走 `meta` 而不是塞进载荷：载荷是业务参数，掺进中台的约定字段会让
        // 插件难以分辨「经工具聚合调用」与「经 invoke_plugin 直接调用」两种形状。
        // 放 meta 里两种调用方式的载荷形状就是同一个。
        if let Some(tool) = tool {
            envelope
                .meta
                .insert("hub.tool".to_string(), tool.to_string());
        }

        // 登录态透传（契约键见 HUB_MAS_TOKEN_META）：hub 自己不持凭证，只把
        // login 换来的 masToken 随信封带给 dc-dict 这类需要下游登录态的插件。
        // 缓存无登录态（闸门关 / 未登录）时不注入——插件以「缺键」识别匿名调用，
        // 空壳键只会让插件把「没有」误判成「有但为空」。login 工具自身经本函数
        // 调 auth 插件时同理：换登录期间缓存若仍有效，带上的是旧 token，而
        // auth 插件只读载荷不读 meta，不受影响。
        if let Some(mas_token) = self.login_cache.valid_mas_token() {
            envelope
                .meta
                .insert(HUB_MAS_TOKEN_META.to_string(), mas_token);
        }

        // 审计：每次插件调用生成 trace_id，**同时**写进信封与中台的 span——
        // 插件侧审计（如 sql-executor 的 SQLite）引用同一个 id，两本账能对上；
        // 中台侧「谁、何时、调了什么、结果、耗时」经 list_traces / get_trace 可查，
        // 不再依赖插件自觉。
        let trace_id = Ulid::generate().to_string();
        envelope.trace_id = trace_id.clone();
        let caller = envelope
            .subject
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "anonymous".to_string());
        let message_id = envelope.message_id.clone();
        let span_name = match tool {
            Some(tool) => format!("{}.{}", args.plugin, tool),
            None => args.plugin.clone(),
        };
        let started_at = chrono::Utc::now();

        let outcome = match self
            .invoker
            .invoke(&args.plugin, args.version.as_deref(), envelope)
            .await
        {
            Ok(outcome) => outcome,
            // 插件不可达 / 调用层故障也要留痕：审计的空窗与故障窗一样有信息量
            Err(err) => {
                let error = err.to_string();
                self.record_invocation_span(
                    &trace_id,
                    &span_name,
                    started_at,
                    "error",
                    serde_json::json!({
                        "kind": "plugin-invocation",
                        "plugin": args.plugin,
                        "tool": tool,
                        "caller": caller,
                        "message_id": message_id,
                        "error": error,
                    }),
                )
                .await;
                return Err(internal(error));
            }
        };

        let value = match outcome {
            InvokeOutcome::Rejected { target, issues, .. } => {
                let issues_json: Vec<_> = issues
                    .iter()
                    .map(|i| {
                        serde_json::json!({
                            "path": i.path,
                            "message": i.message,
                            "severity": i.severity,
                        })
                    })
                    .collect();
                self.record_invocation_span(
                    &trace_id,
                    &span_name,
                    started_at,
                    "rejected",
                    serde_json::json!({
                        "kind": "plugin-invocation",
                        "plugin": target.plugin_name,
                        "version": target.version,
                        "instance_id": target.instance_id,
                        "tool": tool,
                        "caller": caller,
                        "message_id": message_id,
                        "issues": issues_json,
                    }),
                )
                .await;
                serde_json::json!({
                    "status": "rejected",
                    "trace_id": trace_id,
                    "plugin": target.plugin_name,
                    "version": target.version,
                    "issues": issues_json,
                })
            }
            InvokeOutcome::Handled {
                target,
                envelope,
                elapsed_ms,
            } => {
                let (payload, type_url, base64) = match envelope.payload.as_ref() {
                    Some(any) => match hub_proto::decode_payload(any) {
                        Some(json) => (Some(json), None, None),
                        None => (
                            None,
                            Some(any.type_url.clone()),
                            Some(base64_of(&any.value)),
                        ),
                    },
                    None => (None, None, None),
                };

                self.record_invocation_span(
                    &trace_id,
                    &span_name,
                    started_at,
                    "ok",
                    serde_json::json!({
                        "kind": "plugin-invocation",
                        "plugin": target.plugin_name,
                        "version": target.version,
                        "instance_id": target.instance_id,
                        "tool": tool,
                        "caller": caller,
                        "message_id": message_id,
                        "elapsed_ms": elapsed_ms,
                    }),
                )
                .await;
                serde_json::json!({
                    "status": "handled",
                    "trace_id": trace_id,
                    "plugin": target.plugin_name,
                    "version": target.version,
                    "instance_id": target.instance_id,
                    "elapsed_ms": elapsed_ms,
                    "payload": payload,
                    "payload_type_url": type_url,
                    "payload_base64": base64,
                })
            }
        };
        Ok(value)
    }

    /// 落一条插件调用的审计 span（进 trace 体系，`list_traces` / `get_trace` 可查）。
    ///
    /// **旁路语义**：写入失败只记日志，不挡调用——调用能不能成与审计存不存得进
    /// 是两件事；审计的完整性靠「每次都尝试写」，而不是「写失败就连坐」。
    async fn record_invocation_span(
        &self,
        trace_id: &str,
        name: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        status: &str,
        attributes: serde_json::Value,
    ) {
        // 审计边界统一脱敏：attributes 由各调用点手拼、目前不会带信封，但
        // masToken 是登录态，宁可在落库前兜底剔除（见 redact_mas_token），
        // 也不指望每个调用点永远记得不把 envelope.meta 序列化进审计。
        let attributes = redact_mas_token(attributes);
        let span = hub_store::spans::NewSpan {
            trace_id,
            span_id: &Ulid::generate().to_string(),
            parent_span_id: None,
            run_id: None, // 直接调用不是编排执行，没有 run
            node_id: None,
            name,
            started_at,
            duration_ms: (chrono::Utc::now() - started_at).num_milliseconds().max(0),
            status,
            attributes: Some(&attributes),
        };
        if let Err(err) = hub_store::spans::insert_span(self.store.pool(), &span).await {
            tracing::warn!(trace_id, name, %err, "插件调用审计 span 写入失败");
        }
    }

    // -------------------------------------------------------- 人工确认闸门

    /// 插件结果声明「这一步要人同意」时，先把决策卡**弹给用户**，用户同意才放行。
    ///
    /// 背景（E2E 实测的产品缺口）：两阶段确认的令牌只在 agent 与插件之间闭环，
    /// agent 拿到令牌就自己执行了，用户全程不知情。插件是无界面服务，强制不了
    /// 人参与；中台是 MCP server，只有它能向客户端发起标准 elicitation——这是
    /// 全链路里唯一「agent 伪造不了」的人机回路：确认框由 CLI 原生弹出，
    /// **未经同意，插件签发的确认令牌根本不会出现在 agent 收到的结果里**，
    /// 它只能等 5 分钟过期。
    ///
    /// 三个口径：
    /// - 拦截依据是**载荷内容**（`next_action.human_approval_required=true`），
    ///   不是工具声明的 `requires_approval`——后者会让 sql_execute 的执行结果
    ///   也弹一次确认，而那次确认在 submit 时已经做过了。
    /// - 客户端没声明 elicitation 能力时**放行**（维持旧行为）：拦下来等于让
    ///   不支持弹窗的客户端永远做不了写入。强制人参与依赖客户端能力，卡片上的
    ///   `human_approval_required` 与工具描述的强制指令仍然在场。
    /// - 超时 / 客户端回不出合法确认，一律按**未获同意**处理：返回的卡片不含令牌
    ///   （fail-safe）。令牌未泄露，会自己过期。
    async fn human_approval_gate(
        &self,
        ctx: &RequestContext<RoleServer>,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, ErrorData> {
        // 拦截的是 invoke_plugin 的**结果信封**：插件载荷在 `payload` 键下。
        let inner = value.get("payload").unwrap_or(&serde_json::Value::Null);
        if !approval_required(inner) {
            return Ok(value);
        }
        // 客户端不会弹窗 → 放行（维持旧行为）。**但必须让调用方与用户知道这层
        // 闸门没有过**：静默放行会让 agent 以为「human_approval_required 只是
        // 摆设」，用户也看不到「这次写入没经过人工确认」——标注比拦截更诚实。
        // 真正的强制确认依赖客户端能力（支持 elicitation 的 CLI 会弹原生确认框），
        // 不支持的客户端靠卡片上的字段与工具描述的三方一致兜底。
        if !client_can_elicit(ctx) {
            let mut value = value;
            if let Some(payload) = value.get_mut("payload").and_then(|p| p.as_object_mut()) {
                payload.insert("approval_bypassed".into(), serde_json::Value::Bool(true));
                payload.insert(
                    "approval_note".into(),
                    serde_json::Value::String(
                        "当前客户端不支持确认框（elicitation），人工确认闸门未生效：令牌已随本卡下发。"
                            .to_string()
                            + "请先把决策卡完整展示给用户并征得明确同意，再调 sql_execute。",
                    ),
                );
            }
            return Ok(value);
        }

        let message = format_approval_message(inner);
        let schema = ElicitationSchema::builder().build().map_err(internal)?;
        let request = ElicitRequest::new(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message,
            requested_schema: schema,
        });

        let outcome = tokio::time::timeout(
            self.elicitation_timeout,
            ctx.peer.send_request(ServerRequest::ElicitRequest(request)),
        )
        .await;

        match outcome {
            // 用户在 CLI 的确认框里点了同意：放行，令牌随结果下发
            Ok(Ok(ClientResult::ElicitResult(result)))
                if result.action == ElicitationAction::Accept =>
            {
                Ok(value)
            }
            Ok(Ok(ClientResult::ElicitResult(result))) => {
                let (outcome, reason) = match result.action {
                    ElicitationAction::Decline => ("declined", "用户在确认框中拒绝了这次写入"),
                    ElicitationAction::Cancel => ("cancelled", "用户取消了这次写入"),
                    _ => ("invalid", "客户端返回了无法解释的确认结果"),
                };
                Ok(approval_rejection(&value, outcome, reason))
            }
            // 客户端应答不出来（通道故障 / 应答形状不对）：与拒绝同等对待
            Ok(_) => Ok(approval_rejection(
                &value,
                "invalid",
                "客户端未能给出有效的确认结果",
            )),
            // 用户一直没理确认框：不能默认他同意
            Err(_) => Ok(approval_rejection(
                &value,
                "timeout",
                "等待用户确认超时，本次写入未获同意",
            )),
        }
    }

    // -------------------------------------------------------- 登录闸门

    /// 插件调用前建立调用身份。
    ///
    /// 需求口径：MCP 面不做 Cookie 鉴权；**「每个插件使用前」先询问账号密码、
    /// 经 auth 插件 `login` 换登录态**。闸门只在身份未建立时弹窗——成功后按
    /// 进程缓存，之后所有调用静默复用；到期（保守 8 小时，login 返回的
    /// `expiresAt` 只是它自己缓存的 TTL）或被拒时再次弹窗。
    ///
    /// - 弹的是 **elicitation**：账号密码由用户在 CLI 原生表单里填，
    ///   应答从客户端直达中台，**不经过模型上下文**。
    /// - 建立身份只用一次 `login`：它的返回里就带 `userCode`（平台用户名）
    ///   与 `scopes`——check_login 的身份与它同源（auth 插件的 README 写明
    ///   两条路都必须经平台取身份），不必二次验票。
    /// - 关（默认）→ `Disabled`：匿名调用，与历史行为一致。
    async fn login_gate_subject(
        &self,
        ctx: &RequestContext<RoleServer>,
        plugin: &str,
    ) -> Result<LoginOutcome, ErrorData> {
        if !self.login_gate_enabled {
            return Ok(LoginOutcome::Disabled);
        }
        // 登录用的鉴权插件自己**不拦**：登录是闸门的前置，拦了它就鸡生蛋——
        // agent 连 auth__login / auth__check_login 都调不了。它收到的调用
        // 以匿名进行（登录前本就没有身份），身份由中台的 login 工具建立。
        if plugin == self.login_plugin {
            return Ok(LoginOutcome::Disabled);
        }
        if let Some(subject) = self.login_cache.valid_subject() {
            return Ok(LoginOutcome::Authenticated(subject));
        }
        // 客户端不会弹窗 → 无法建立身份，也**不能**静默放行：闸门开了就要认账。
        // 拒绝卡说明怎么解决（升级客户端或关闸门）。
        if !client_can_elicit(ctx) {
            return Ok(LoginOutcome::Denied(login_required_card(
                "client-unsupported",
            )));
        }

        // 给两次机会：第一次可能是手滑，第二次仍失败才定性为拒绝
        for attempt in 0..2 {
            let Some((account, password)) = self.elicit_credentials(ctx).await? else {
                return Ok(LoginOutcome::Denied(login_required_card("no-credentials")));
            };
            match self.login_via_auth_plugin(&account, &password).await? {
                Ok(identity) => {
                    return Ok(LoginOutcome::Authenticated(self.login_cache.store(identity)));
                }
                Err(reason) if attempt == 0 => {
                    let _ = reason; // 第二次弹窗的 message 已说明「上次失败」
                }
                Err(reason) => {
                    let _ = reason;
                    return Ok(LoginOutcome::Denied(login_required_card("bad-credentials")));
                }
            }
        }
        unreachable!("循环内两条 Err 分支已覆盖全部退出路径")
    }

    /// 弹出账号密码表单。`None` = 用户拒绝/取消/超时/空值——都视为「没给凭证」。
    async fn elicit_credentials(
        &self,
        ctx: &RequestContext<RoleServer>,
    ) -> Result<Option<(String, String)>, ErrorData> {
        let schema = ElicitationSchema::builder()
            .required_property(
                "account",
                PrimitiveSchemaDefinition::String(StringSchema::new().description("平台账号")),
            )
            .required_property(
                "password",
                PrimitiveSchemaDefinition::String(
                    StringSchema::new()
                        .description("平台密码；仅用于经鉴权插件换取登录态，不进入对话记录"),
                ),
            )
            .build()
            .map_err(internal)?;
        let request = ElicitRequest::new(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "首次使用插件前需要登录。请输入平台账号与密码以建立本次调用身份。".to_string(),
            requested_schema: schema,
        });
        let outcome = tokio::time::timeout(
            self.elicitation_timeout,
            ctx.peer.send_request(ServerRequest::ElicitRequest(request)),
        )
        .await;
        match outcome {
            Ok(Ok(ClientResult::ElicitResult(result)))
                if result.action == ElicitationAction::Accept =>
            {
                let content = result.content.unwrap_or_default();
                let account = content
                    .get("account")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let password = content
                    .get("password")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if account.is_empty() || password.is_empty() {
                    return Ok(None);
                }
                Ok(Some((account, password)))
            }
            _ => Ok(None),
        }
    }

    /// 经 auth 插件 `login` 用账号密码换登录态。
    ///
    /// 凭证错（`authenticated=false`）是业务结果 → `Err(原因)`，闸门据此重问；
    /// 登录服务不可达是插件返回 error → invoke 层给出 MCP 错误，调用方可重试。
    /// **密码只在这一段流动**：进的是 auth 插件（它只以 HMAC 指纹参与校验），
    /// 不落缓存、不写日志、不进任何卡。
    async fn login_via_auth_plugin(
        &self,
        account: &str,
        password: &str,
    ) -> Result<Result<EstablishedIdentity, String>, ErrorData> {
        let value = self
            .invoke_plugin_value(
                None,
                None,
                InvokePluginArgs {
                    plugin: self.login_plugin.clone(),
                    payload: serde_json::json!({"account": account, "password": password}),
                    version: None,
                    message_id: None,
                    timeout_ms: None,
                },
            )
            .await?;
        let payload = value.get("payload").cloned().unwrap_or_default();
        Ok(parse_login_response(&payload, account))
    }

    /// 列出重投耗尽的死信。
    ///
    /// agent 靠它回答「有什么数据卡住了、卡在哪条编排的哪个节点」。
    #[tool(
        description = "列出重投耗尽的死信（默认只看还没重放过的）：每条给出所属编排、节点、尝试次数、最后的错误与载荷摘要"
    )]
    pub async fn list_dead_letters(
        &self,
        Parameters(args): Parameters<ListDeadLettersArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let rows = hub_store::dead_letters::list(
            self.store.pool(),
            args.pending.unwrap_or(true),
            args.limit.unwrap_or(50).clamp(1, 500),
        )
        .await
        .map_err(internal)?;

        Ok(json_result(&rows))
    }

    /// 重放一条死信。
    #[tool(
        description = "重放一条死信，产生一次新的执行。**载荷必须由你提供**——死信表里只存摘要，中台不保留全量报文"
    )]
    pub async fn replay_dead_letter(
        &self,
        Parameters(args): Parameters<ReplayDeadLetterArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let exec = self.async_exec()?;
        let pool = self.store.pool();

        let letter = hub_store::dead_letters::find(pool, args.id)
            .await
            .map_err(internal)?
            .ok_or_else(|| not_found(format!("死信 {} 不存在", args.id)))?;

        let flow_name = letter
            .flow_name
            .clone()
            .ok_or_else(|| invalid_params("这条死信没有记下所属编排，无法重放"))?;

        // 抢占重放权：抢不到说明别人正在放，直接拒绝比产生两次执行好
        let claimed = hub_store::dead_letters::begin_replay(pool, args.id, chrono::Utc::now())
            .await
            .map_err(internal)?;
        if !claimed {
            return Err(invalid_params(format!(
                "死信 {} 已经重放过（{}），不能重复重放",
                args.id,
                letter.replayed_run_id.as_deref().unwrap_or("")
            )));
        }

        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if timeout_ms <= 0 {
            let _ = hub_store::dead_letters::clear_replay(pool, args.id).await;
            return Err(invalid_params("timeout_ms 必须为正数"));
        }

        let payload = match hub_proto::encode_payload(&args.payload) {
            Ok(payload) => payload,
            Err(err) => {
                let _ = hub_store::dead_letters::clear_replay(pool, args.id).await;
                return Err(invalid_params(err.to_string()));
            }
        };

        let envelope = Envelope {
            // 重放必须换一个新的 message_id：沿用旧的会让下游插件按幂等把这次重放跳过
            message_id: generate_message_id(&flow_name),
            deadline_ms: chrono::Utc::now().timestamp_millis() + timeout_ms,
            r#type: PayloadType::Request as i32,
            payload: Some(payload),
            ..Default::default()
        };

        let run_id = match exec
            .enqueue(
                &flow_name,
                envelope,
                Some(serde_json::json!({
                    "kind": "replay",
                    "dead_letter_id": args.id,
                    "original_run_id": letter.run_id,
                })),
            )
            .await
        {
            Ok(run_id) => run_id,
            Err(err) => {
                // 抢占了却没真的重放，比没抢占更糟——那条死信会显示「已重放」
                let _ = hub_store::dead_letters::clear_replay(pool, args.id).await;
                return Err(internal(err.to_string()));
            }
        };

        hub_store::dead_letters::set_replayed_run(pool, args.id, &run_id)
            .await
            .map_err(internal)?;

        Ok(json_result(&serde_json::json!({
            "status": "replayed",
            "dead_letter_id": args.id,
            "run_id": run_id,
            "flow": flow_name,
        })))
    }

    /// 异步触发一条已发布的编排。
    #[tool(
        description = "异步触发一条已发布的编排：立刻返回 run_id，编排在后台按节点推进。适合长链路或不需要立刻拿结果的场景"
    )]
    pub async fn trigger_flow_async(
        &self,
        Parameters(args): Parameters<TriggerFlowArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let exec = self.async_exec()?;
        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if timeout_ms <= 0 {
            return Err(invalid_params("timeout_ms 必须为正数"));
        }

        let payload = hub_proto::encode_payload(&args.payload).map_err(invalid_params)?;
        let envelope = Envelope {
            message_id: args
                .message_id
                .clone()
                .unwrap_or_else(|| generate_message_id(&args.flow)),
            deadline_ms: chrono::Utc::now().timestamp_millis() + timeout_ms,
            r#type: PayloadType::Request as i32,
            payload: Some(payload),
            ..Default::default()
        };

        let run_id = exec
            .enqueue(
                &args.flow,
                envelope,
                Some(serde_json::json!({"kind": "mcp", "tool": "trigger_flow_async"})),
            )
            .await
            .map_err(|err| internal(err.to_string()))?;

        Ok(json_result(&serde_json::json!({
            "status": "queued",
            "run_id": run_id,
            "flow": args.flow,
        })))
    }

    /// 列出触发器。
    #[tool(
        description = "列出触发器：不传 flow 时列出全部启用中的，传了就给出那条 flow 的全部（含已停用的）及其最近触发时间与错误"
    )]
    pub async fn list_triggers(
        &self,
        Parameters(args): Parameters<ListTriggersArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let rows = match args.flow.as_deref() {
            Some(flow) => hub_store::triggers::list_of_flow(self.store.pool(), flow)
                .await
                .map_err(internal)?,
            None => hub_store::triggers::list_enabled(self.store.pool(), None)
                .await
                .map_err(internal)?,
        };

        Ok(json_result(&rows))
    }

    /// 登记或更新一个触发器。
    ///
    /// 与「发布」不同，这里**不涉及生产流量的走向**——它只是让一条已经发布过的编排
    /// 多一个被触发的入口。所以按设计 agent 可以做，而发布仍然只能由人来做。
    #[tool(
        description = "登记或更新一个 cron / mq 触发器（同名的按新配置覆盖并重新启用）。只影响「怎么被触发」，不改变编排本身"
    )]
    pub async fn save_trigger(
        &self,
        Parameters(args): Parameters<SaveTriggerArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let row = hub_store::triggers::upsert(
            self.store.pool(),
            &hub_store::triggers::NewTrigger {
                flow_name: &args.flow,
                kind: &args.kind,
                name: args.name.as_deref().unwrap_or("default"),
                config: &args.config,
            },
        )
        .await
        .map_err(|err| match err {
            hub_store::StoreError::Invalid(message) => invalid_params(message),
            other => internal(other.to_string()),
        })?;

        Ok(json_result(&row))
    }
}

// ---------------------------------------------------------------- 入参

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFlowArgs {
    /// flow 名
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaveFlowDraftArgs {
    /// flow 名
    pub name: String,

    /// 编排定义，形如：
    /// `{"name":"<flow 名>","nodes":[{"id":"n1","plugin":"auth"},{"id":"n2","plugin":"validate"}],"edges":[{"from":"n1","to":"n2"}]}`
    ///
    /// 节点可带 `version`（缺省取最新）、`timeout_ms`、`retries`。
    pub definition: serde_json::Value,

    #[serde(default)]
    pub description: String,

    /// 谁改的（审计用）
    #[serde(default)]
    pub created_by: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TriggerFlowArgs {
    /// flow 名
    pub flow: String,

    /// 业务载荷，JSON 对象
    pub payload: serde_json::Value,

    /// 幂等键；缺省由中台生成
    #[serde(default)]
    pub message_id: Option<String>,

    /// 整条链的超时预算（毫秒）
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRunsArgs {
    /// 按 flow 名过滤
    #[serde(default)]
    pub flow: Option<String>,

    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetRunArgs {
    /// 执行 id
    pub run_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListDeadLettersArgs {
    /// 只看还没重放过的（默认 true）
    #[serde(default)]
    pub pending: Option<bool>,

    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplayDeadLetterArgs {
    /// 死信 id（`list_dead_letters` 里给出的那个数字）
    pub id: i64,

    /// 重放用的业务载荷。**必须提供**——死信表里只有摘要，中台不保留全量报文
    pub payload: serde_json::Value,

    /// 这次重放的超时预算（毫秒）
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTriggersArgs {
    /// 只看某条 flow 的（含已停用的）；不传则列出全部启用中的
    #[serde(default)]
    pub flow: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaveTriggerArgs {
    /// 挂在哪条 flow 上
    pub flow: String,

    /// 触发器类型：`cron` 或 `mq`
    pub kind: String,

    /// 同一条 flow 下同类触发器靠它区分；缺省为 default
    #[serde(default)]
    pub name: Option<String>,

    /// 触发配置。cron 形如 `{"expr":"0 2 * * *"}`；mq 形如 `{"stream":"wms:orders"}`
    pub config: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTracesArgs {
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTraceArgs {
    /// trace id（W3C traceparent 里的 32 位十六进制）
    pub trace_id: String,
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HubMcp {
    /// 让 agent 知道它在跟谁说话。rmcp 的默认名是库名，对排查毫无帮助。
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(hub_core::SERVICE_NAME, env!("CARGO_PKG_VERSION")),
        )
    }

    /// 工具列表 = 中台自己的工具 + **在线插件声明的工具**。
    ///
    /// 手写覆盖宏生成的那份：`#[tool_handler]` 只在方法缺失时才补，而它补出来的
    /// 只列 `self.tool_router` 里的静态工具——agent 会因此看不见任何插件能力，
    /// 而「插件注册即用、agent 立即可见」正是这个面的设计目的。
    ///
    /// 每次调用都查一次库，不做缓存：一次索引查询很轻，而缓存要处理插件上下线的
    /// 失效，那才是容易出错的地方（缓存里多出一个已下线的工具，agent 调它只会拿到
    /// 一句「插件不可达」，而原因看不出是缓存）。
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = self.tool_router.list_all();
        let rows = hub_store::plugins::online_tools(self.store.pool())
            .await
            .map_err(internal)?;
        tools.extend(rows.iter().map(plugin_tool_to_mcp));

        // 两段各自有序，合起来再排一次，让列表对 agent 稳定可读
        tools.sort_by(|a, b| a.name.cmp(&b.name));

        // 分页字段留默认：工具总量是「中台自己 + 在线插件」的量级，没有分页的需要。
        //
        // ttlMs/cacheScope 必须显式给：协商到 2025-11-25 的客户端（Claude CLI 实测）
        // 把这两个字段当必填，缺失时整个 tools/list 校验失败、服务器被丢弃——
        // 表现为「连接成功但一个工具都看不见」。ttl=0 表示结果不许缓存，
        // 每次现查，与本函数「不做缓存」的既有语义一致。
        Ok(ListToolsResult {
            ttl_ms: Some(0),
            cache_scope: Some(CacheScope::Private),
            tools,
            ..Default::default()
        })
    }

    /// 分发一次工具调用：**先认插件工具，再落回静态工具**。
    ///
    /// 认的方式是拿库里的 (插件名, 工具名) 拼出全名做**精确匹配**，而不是把
    /// `request.name` 按 `__` 切开再猜。插件名的字符集允许 `_`（连 `__` 也允许），
    /// 字符串切分遇到 `my__plugin__tool` 就会给出错的一对；精确匹配没有这个问题。
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let rows = hub_store::plugins::online_tools(self.store.pool())
            .await
            .map_err(internal)?;

        if let Some(row) = rows
            .iter()
            .find(|row| plugin_tool_name(&row.plugin_name, &row.name) == request.name)
        {
            let payload = serde_json::Value::Object(request.arguments.unwrap_or_default());
            let subject = match self.login_gate_subject(&context, &row.plugin_name).await? {
                LoginOutcome::Disabled => subject_of(&context),
                LoginOutcome::Authenticated(subject) => Some(subject),
                LoginOutcome::Denied(card) => return Ok(json_result(&card).into()),
            };
            let value = match self
                .invoke_plugin_value(
                    subject,
                    Some(&row.name),
                    InvokePluginArgs {
                        plugin: row.plugin_name.clone(),
                        payload,
                        version: None,
                        message_id: None,
                        timeout_ms: None,
                    },
                )
                .await
            {
                Ok(value) => value,
                Err(err) => invocation_error_card(&err.to_string()),
            };
            // 插件工具的结果一律是「调用完成」：插件自己用载荷表达业务结果
            // （拒绝、需确认都是业务结果，不是 MCP 层的 input-required）。
            // 但声明了「要人同意」的载荷要先过确认闸门再交给 agent。
            let value = self.human_approval_gate(&context, value).await?;
            return Ok(json_result(&value).into());
        }

        let call = ToolCallContext::new(self, request, context);
        self.tool_router.call(call).await
    }
}

/// 插件**调用层故障**（不可达 / 超时 / gRPC 错误）的卡形表达。
///
/// 与业务拒绝（`status=rejected`，插件给的）不同层：这是中台与插件之间的故障，
/// 不是对 SQL 的判定。给 agent 的仍是结构化 JSON 而不是传输层错误——
/// 客户端少一层各自为政的 error 渲染，问题排查也多一张说得清「坏在哪一跳」的卡。
/// 中台本身不受影响：该请求以 200 正常收尾，其它会话照常。
fn invocation_error_card(error: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "error",
        "decision": "reject",
        "message": format!("插件调用失败（中台不受影响）：{error}"),
    })
}

/// 插件工具在 MCP 工具面上的全名。
///
/// 用 `__` 分隔：跨插件重名由前缀隔离，插件内的重名在注册时就被
/// `hub-registry` 的 `validate_tools` 拦住了——两处合起来保证全名唯一。
fn plugin_tool_name(plugin: &str, tool: &str) -> String {
    format!("{plugin}__{tool}")
}

/// 把插件声明的一个工具转成 MCP 的工具描述。
///
/// `input_schema_json` 是插件给的入参 JSON Schema（字符串形式）。**解析失败不阻断**：
/// 一个插件的 schema 写坏了不该让整个工具面报错，退化成「无参数约束」并把原因写进
/// 描述——至少让它可调用、可排查，而不是从列表里凭空消失。
fn plugin_tool_to_mcp(row: &OnlineToolRow) -> Tool {
    let (schema, note) = match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
        &row.input_schema_json,
    ) {
        Ok(schema) => (schema, String::new()),
        Err(err) => (
            serde_json::Map::new(),
            format!("（入参 schema 无法解析，已忽略：{err}）"),
        ),
    };

    // 让 agent 在读到描述时就看见「这个工具会改生产」，而不是调用之后才知道
    let approval = if row.requires_approval {
        "⚠️ 会改变生产走向，调用前需人工确认。"
    } else {
        ""
    };

    Tool::new(
        plugin_tool_name(&row.plugin_name, &row.name),
        format!(
            "[{}] {}{}{}",
            row.plugin_name, row.description, approval, note
        ),
        Arc::new(schema),
    )
}

// ---------------------------------------------------------------- 辅助

fn json_result(value: &impl serde::Serialize) -> CallToolResult {
    let text = serde_json::to_string_pretty(value)
        .unwrap_or_else(|err| format!("{{\"error\":\"结果序列化失败: {err}\"}}"));
    CallToolResult::success(vec![ContentBlock::text(text)])
}

// ---------------------------------------------------------------- 登录

/// 递归剔除 JSON 里的登录态键 [`HUB_MAS_TOKEN_META`]。
///
/// masToken 的允许活动范围只有两处：内存缓存与发往插件的那份信封 meta。
/// 审计 span 落库前过一遍这里，将来谁把 envelope（含 meta）整个序列化进
/// 审计属性，键也会在边界被剥掉——不依赖每个调用点自觉。其它键一律
/// 原样保留，脱敏只删这一把钥匙。
fn redact_mas_token(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, _)| key != HUB_MAS_TOKEN_META)
                .map(|(key, v)| (key, redact_mas_token(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_mas_token).collect()),
        other => other,
    }
}

/// 一次成功登录建立的调用身份与到期信息。
#[derive(Debug)]
struct EstablishedIdentity {
    subject: Subject,
    /// auth 插件 `login` 返回的 masToken，随信封 meta 透传给需要下游登录态的
    /// 插件（见 [`HUB_MAS_TOKEN_META`]）。login 未返回（旧版插件）或为空串时
    /// 是 `None`——没有登录态就不注入，不造空壳键。
    mas_token: Option<String>,
    /// auth 插件 `login` 返回的 `expiresAt`（毫秒时间戳）——那是它**自身缓存
    /// 条目**的 TTL，不是 masToken 的真实有效期。缓存过期只是「下次重新弹
    /// 登录框」，不是安全问题。
    expires_at_ms: Option<i64>,
}

/// 解析 auth 插件 `login` 的返回。
///
/// 凭证错（`authenticated=false` + `reason`）是**业务结果** → `Err(原因)`，
/// 闸门据此重问；登录服务不可达是插件 error，在 invoke 层就已变成 MCP 错误，
/// 到不了这里。`userCode` 是**平台用户名**——审计里的调用主体用它（auth
/// 插件的 README 写明两条鉴权路径的身份必须同源，都来自平台）。`masToken`
/// 原样保留进身份供信封透传（[`HUB_MAS_TOKEN_META`]）；缺失或为空不阻断
/// 登录建立——插件侧以「meta 缺键」识别无登录态。
fn parse_login_response(
    payload: &serde_json::Value,
    account: &str,
) -> Result<EstablishedIdentity, String> {
    use serde_json::Value;
    if payload.get("authenticated").and_then(Value::as_bool) != Some(true) {
        let reason = payload
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("账号或密码错误");
        return Err(reason.to_string());
    }
    let user = payload
        .get("userCode")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if user.is_empty() {
        return Err("鉴权插件未返回平台身份（userCode 缺失）".to_string());
    }
    let scopes = payload
        .get("scopes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(EstablishedIdentity {
        subject: Subject {
            kind: SubjectKind::Human as i32,
            id: user.to_string(),
            scopes,
            origin: format!("mcp-login(account={account})"),
            ..Default::default()
        },
        // masToken 原样保留进身份：它要随信封 meta 透传给插件。空串视同缺失。
        mas_token: payload
            .get("masToken")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|token| !token.is_empty()),
        expires_at_ms: payload.get("expiresAt").and_then(Value::as_i64),
    })
}

/// 客户端能否接收 form 型 elicitation（登录框、确认框都靠它）。
fn client_can_elicit(ctx: &RequestContext<RoleServer>) -> bool {
    ctx.client_capabilities()
        .and_then(|caps| caps.elicitation)
        .map(|elic| elic.form.is_some())
        .unwrap_or(false)
}

/// 「需要登录」指引卡：agent 照着调 [`Self::login`] 工具即可。
///
/// 这是**所有客户端**都支持的路径——调工具是 MCP 的基本能力，不依赖
/// elicitation。zcode / codex 这类不支持服务端弹窗的 agent 靠它完成登录：
/// agent 向用户要来账号密码 → 调 login → 重试原调用。
fn login_required_card(outcome: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "rejected",
        "decision": "reject",
        "message": "需要先建立登录身份：请调用 login 工具（account 与 password 由用户提供）完成登录，然后重试本次调用。",
        "login": {"required": true, "outcome": outcome},
        "next_action": {
            "tool": "login",
            "args": {"account": "<平台账号>", "password": "<平台密码>"},
            "note": "向用户询问账号密码；password 只用于换取登录态，不要复述。登录成功后重试原调用。"
        }
    })
}

// ---------------------------------------------------------------- 人工确认

/// 插件载荷里「这一步要人同意」的标记键。与 sql-executor 的 confirm 卡约定
/// 一致；任何插件都可以用同一个键接入确认闸门。
const HUMAN_APPROVAL_FLAG: &str = "human_approval_required";

/// 插件载荷有没有声明「这一步要人同意」。
fn approval_required(payload: &serde_json::Value) -> bool {
    payload
        .get("next_action")
        .and_then(|next| next.get(HUMAN_APPROVAL_FLAG))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// 单段文本的截断上限。确认框是给人读的：塞进一份 2 万字符的 EXPLAIN 留档
/// 没有人能看完，也就没有人真的在确认。
const APPROVAL_TEXT_LIMIT: usize = 600;

fn truncate_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…（已截断，完整内容见审计）")
}

/// 确认框的正文：从决策卡里摘**人**要看的部分。
///
/// 人工确认要能负得起责，前提是「确认的东西」完整可见：待执行 SQL、回滚
/// SQL、审核命中了什么、预估动多少行。少一样，那个「同意」就是签空白支票。
fn format_approval_message(payload: &serde_json::Value) -> String {
    use serde_json::Value;

    let mut lines: Vec<String> = Vec::new();
    let plugin = payload
        .get("plugin")
        .and_then(Value::as_str)
        .unwrap_or("未知插件");
    let version = payload
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("?");
    lines.push(format!(
        "插件 {plugin}（{version}）请求执行一条写入，等待你的确认："
    ));

    if let Some(kind) = payload.pointer("/statement/kind").and_then(Value::as_str) {
        let tables = payload
            .pointer("/statement/tables")
            .and_then(Value::as_array)
            .map(|t| {
                t.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("、")
            })
            .unwrap_or_default();
        lines.push(format!(
            "语句类型：{kind}；涉及表：{}",
            if tables.is_empty() { "?" } else { &tables }
        ));
    }

    for (label, key) in [("待执行 SQL", "sql"), ("回滚 SQL", "rollback_sql")] {
        if let Some(text) = payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            lines.push(format!(
                "{label}：{}",
                truncate_text(text, APPROVAL_TEXT_LIMIT)
            ));
        }
    }

    if let Some(findings) = payload.get("findings").and_then(Value::as_array) {
        for f in findings.iter().take(8) {
            let rule = f.get("rule").and_then(Value::as_str).unwrap_or("?");
            let severity = f.get("severity").and_then(Value::as_str).unwrap_or("?");
            let message = f.get("message").and_then(Value::as_str).unwrap_or("");
            lines.push(format!("审核发现 [{rule}/{severity}] {message}"));
        }
    }

    let mut impact_bits: Vec<String> = Vec::new();
    for (label, pointer) in [
        ("预估扫描", "/impact/est_scan_rows"),
        ("预估代价", "/impact/est_cost"),
    ] {
        if let Some(n) = payload.pointer(pointer).and_then(Value::as_f64) {
            impact_bits.push(format!("{label} {n}"));
        }
    }
    if !impact_bits.is_empty() {
        lines.push(format!("影响面估计：{}", impact_bits.join("，")));
    }

    if let Some(note) = payload.pointer("/next_action/note").and_then(Value::as_str) {
        lines.push(truncate_text(note, APPROVAL_TEXT_LIMIT));
    }
    lines.join("\n")
}

/// 闸门拦下后回给 agent 的卡片：形状与插件的拒绝卡一致（agent 按「业务结果」
/// 处理，不需要新逻辑），但**不含确认令牌**——它从未离开中台。
fn approval_rejection(
    envelope: &serde_json::Value,
    outcome: &str,
    reason: &str,
) -> serde_json::Value {
    serde_json::json!({
        "status": "rejected",
        "decision": "reject",
        "plugin": envelope.get("plugin"),
        "version": envelope.get("version"),
        "message": format!("人工确认未通过（{outcome}）：{reason}。写入未执行；确认令牌未随本结果下发，将在有效期后自动作废。"),
        "human_approval": {"required": true, "outcome": outcome},
    })
}

fn internal(err: impl std::fmt::Display) -> ErrorData {
    ErrorData::new(ErrorCode::INTERNAL_ERROR, err.to_string(), None)
}

fn not_found(message: String) -> ErrorData {
    ErrorData::new(ErrorCode::INVALID_PARAMS, message, None)
}

fn invalid_params(err: impl std::fmt::Display) -> ErrorData {
    ErrorData::new(ErrorCode::INVALID_PARAMS, err.to_string(), None)
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 从 MCP 的请求上下文取已认证的调用主体。
///
/// rmcp 把 HTTP 的 `request::Parts` 注入进 `RequestContext.extensions`，而
/// **axum 中间件写进请求扩展的东西保留在 `Parts.extensions` 里**——鉴权中间件放的
/// 那份身份因此在这里取得到。
///
/// 这条链路有个前提：**鉴权要真的挂在 `/mcp` 上**。挂在内层 `api_router` 上时
/// 这里永远取不到东西，表现出来只是「agent 的调用都是匿名的」——不报错、不打日志。
/// 见 `hub_api::router_with_extras` 与 `hub-server/tests/http_app.rs`。
fn subject_of(ctx: &RequestContext<RoleServer>) -> Option<Subject> {
    let parts = ctx.extensions.get::<Parts>()?;
    let subject = parts.extensions.get::<Arc<AuthenticatedSubject>>()?;

    Some(Subject {
        // 与 HTTP 面一致：凭证是某个人的登录态，发起主体就是那个人
        kind: SubjectKind::Human as i32,
        id: subject.user_code.clone(),
        scopes: subject.scopes.clone(),
        ..Default::default()
    })
}

/// agent 未提供 message_id 时生成一个。
///
/// 用「插件名 + 毫秒时间戳 + 进程内计数」而不是引入 ULID：这里的 id 只需要把一次调用
/// 串起来、并在同一毫秒内不重样，不承担全局有序的职责。
fn generate_message_id(plugin: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{plugin}-{}-{n}", chrono::Utc::now().timestamp_millis())
}

// ---------------------------------------------------------------- 人工确认的纯函数测试

#[cfg(test)]
mod approval_gate_tests {
    use super::*;
    use serde_json::json;

    /// 一张 sql-executor 的 confirm 卡（形状照抄插件的真实输出）。
    fn confirm_card() -> serde_json::Value {
        json!({
            "plugin": "sql-executor",
            "version": "0.2.0",
            "decision": "confirm",
            "sql": "UPDATE orders SET status='done' WHERE id = 1",
            "rollback_sql": "UPDATE orders SET status='pending' WHERE id = 1",
            "statement": {"kind": "UPDATE", "tables": ["orders"], "write": true, "ddl": false},
            "findings": [
                {"rule": "R-009", "severity": "warning", "message": "读系统表"},
                {"rule": "R-011", "severity": "warning", "message": "隐式类型转换"}
            ],
            "impact": {"est_scan_rows": 48000},
            "next_action": {
                "tool": "sql_execute",
                "confirmation_token": "ct_TESTTESTTESTTESTTESTTEST",
                "human_approval_required": true,
                "note": "这条写入尚未执行。"
            }
        })
    }

    #[test]
    fn 没有标记或标记为假都不拦() {
        assert!(!approval_required(&json!({"decision": "allow"})));
        assert!(!approval_required(
            &json!({"next_action": {"human_approval_required": false}})
        ));
        assert!(approval_required(&confirm_card()));
    }

    #[test]
    fn 确认框正文带齐人要看的要素() {
        let message = format_approval_message(&confirm_card());
        assert!(message.contains("sql-executor"), "插件名：{message}");
        assert!(
            message.contains("UPDATE orders SET status='done'"),
            "待执行 SQL 原文：{message}"
        );
        assert!(message.contains("回滚 SQL"), "{message}");
        assert!(message.contains("[R-009/warning]"), "审核发现：{message}");
        assert!(message.contains("48000"), "影响面：{message}");
        assert!(message.contains("尚未执行"), "插件自己的说明：{message}");
    }

    #[test]
    fn 超长文本截断到人读得动的长度() {
        let mut card = confirm_card();
        card["sql"] = json!(format!(
            "UPDATE orders SET note='{}' WHERE id = 1",
            "x".repeat(5000)
        ));
        let message = format_approval_message(&card);
        assert!(message.contains("已截断"), "{message}");
        assert!(message.chars().count() < 6000, "整卡不该被一段 SQL 撑爆");
    }

    #[test]
    fn 拒绝卡不含令牌且与插件拒绝卡同形状() {
        let envelope =
            json!({"plugin": "sql-executor", "version": "0.2.0", "payload": confirm_card()});
        let card = approval_rejection(&envelope, "timeout", "等待用户确认超时，本次写入未获同意");
        assert_eq!(card["status"], "rejected");
        assert_eq!(card["decision"], "reject");
        assert_eq!(card["plugin"], "sql-executor");
        assert!(card.get("next_action").is_none(), "拒绝卡绝不能带令牌");
        assert_eq!(card["human_approval"]["outcome"], "timeout");
        assert!(card["message"].as_str().unwrap().contains("未获同意"));
    }
}

// ---------------------------------------------------------------- 登录闸门的纯函数测试

#[cfg(test)]
mod login_gate_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 登录成功解析出身份与权限位() {
        let payload = json!({
            "authenticated": true,
            "masToken": "mt_xxx",
            "account": "maqb11",
            "userCode": "maqb11",
            "scopes": ["hub:read", "hub:invoke"],
            "expiresAt": 1758000000000_i64
        });
        let identity = parse_login_response(&payload, "maqb11").expect("应建立身份");
        assert_eq!(identity.subject.kind, SubjectKind::Human as i32);
        assert_eq!(identity.subject.id, "maqb11");
        assert_eq!(identity.subject.scopes, vec!["hub:read", "hub:invoke"]);
        assert_eq!(identity.subject.origin, "mcp-login(account=maqb11)");
        assert_eq!(identity.expires_at_ms, Some(1758000000000));
        // masToken 必须原样保留——它要随信封 meta 透传给 dc-dict 这类插件
        assert_eq!(identity.mas_token.as_deref(), Some("mt_xxx"));
    }

    #[test]
    fn 凭证错是业务结果并带回原因() {
        let payload = json!({"authenticated": false, "reason": "账号或密码错误"});
        let err = parse_login_response(&payload, "maqb11").unwrap_err();
        assert_eq!(err, "账号或密码错误");
    }

    #[test]
    fn 缺身份视为不可接受() {
        let payload = json!({"authenticated": true, "masToken": "mt_xxx"});
        let err = parse_login_response(&payload, "maqb11").unwrap_err();
        assert!(err.contains("userCode"), "{err}");
    }

    #[test]
    fn 没有过期时间也能建立身份() {
        let payload = json!({"authenticated": true, "userCode": "u-1", "scopes": []});
        let identity = parse_login_response(&payload, "u-1").expect("应建立身份");
        assert!(identity.expires_at_ms.is_none());
        assert!(identity.subject.scopes.is_empty());
        // 旧版鉴权插件不返回 masToken：登录照常建立，插件侧以缺键识别无登录态
        assert!(identity.mas_token.is_none());
    }

    #[test]
    fn 空串_mas_token_视同缺失() {
        let payload = json!({"authenticated": true, "userCode": "u-1", "masToken": ""});
        let identity = parse_login_response(&payload, "u-1").expect("应建立身份");
        assert!(
            identity.mas_token.is_none(),
            "空串绝不能被注入成 meta 空壳键：{:?}",
            identity.mas_token
        );
    }
}

// ---------------------------------------------------------------- 登录缓存的测试

#[cfg(test)]
mod login_cache_tests {
    use super::*;

    /// 造一条已建立的登录身份（绕过 parse，直接测缓存语义）。
    fn identity(user: &str, mas_token: Option<&str>, expires_at_ms: Option<i64>) -> EstablishedIdentity {
        EstablishedIdentity {
            subject: Subject {
                kind: SubjectKind::Human as i32,
                id: user.to_string(),
                ..Default::default()
            },
            mas_token: mas_token.map(str::to_string),
            expires_at_ms,
        }
    }

    /// 登录闸门（elicitation）与显式 login 工具两条入口都汇入
    /// `LoginCache::store`，所以缓存语义在这里测一份即覆盖两条入口。
    #[test]
    fn 登录身份连同mas_token一起入缓存() {
        let cache = LoginCache::new();
        let subject = cache.store(identity("u-1", Some("mt_aaa"), None));
        assert_eq!(subject.id, "u-1");
        assert_eq!(
            cache.valid_mas_token().as_deref(),
            Some("mt_aaa"),
            "masToken 必须随身份一起被缓存"
        );
        assert_eq!(cache.valid_subject().map(|s| s.id), Some("u-1".to_string()));
    }

    #[test]
    fn 单槽语义_后登录整体顶掉先登录() {
        let cache = LoginCache::new();
        cache.store(identity("u-1", Some("mt_aaa"), None));
        cache.store(identity("u-2", Some("mt_bbb"), None));
        assert_eq!(
            cache.valid_subject().map(|s| s.id),
            Some("u-2".to_string()),
            "单槽：最近一次登录的账号代表当前调用方"
        );
        assert_eq!(
            cache.valid_mas_token().as_deref(),
            Some("mt_bbb"),
            "旧 token 必须随旧身份一起被顶掉，不能出现身份与 token 错位的拼装态"
        );
    }

    #[test]
    fn 没有或为空的mas_token不产生登录态键() {
        let cache = LoginCache::new();
        cache.store(identity("u-1", None, None));
        assert_eq!(cache.valid_mas_token(), None);
        cache.store(identity("u-1", Some(""), None));
        assert_eq!(cache.valid_mas_token(), None, "空串同样不算有值");
    }

    #[test]
    fn 到期后整条身份连同token一并作废() {
        let cache = LoginCache::new();
        // expiresAt 给 50ms 后：落在这个窗口内一定算出非零 TTL；睡过窗口必过期
        let soon = chrono::Utc::now().timestamp_millis() + 50;
        cache.store(identity("u-1", Some("mt_aaa"), Some(soon)));
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(cache.valid_mas_token(), None, "过期缓存要被惰性清除");
        assert_eq!(cache.valid_subject(), None, "subject 与 token 同生共死");
    }
}

// ---------------------------------------------------------------- 审计脱敏的测试

#[cfg(test)]
mod mas_token_redact_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 顶层与嵌套里的登录态键都被剔除() {
        let redacted = redact_mas_token(json!({
            "hub.mas_token": "mt_SECRET",
            "plugin": "dc-dict",
            "meta": {"hub.tool": "dict_query", "hub.mas_token": "mt_SECRET"},
            "nested": [{"deep": {"hub.mas_token": "mt_SECRET"}}]
        }));
        let text = serde_json::to_string(&redacted).expect("应可序列化");
        assert!(!text.contains("mt_SECRET"), "token 值不得残留：{text}");
        // 其它键一律原样保留
        assert_eq!(redacted["plugin"], "dc-dict");
        assert_eq!(redacted["meta"]["hub.tool"], "dict_query");
    }

    #[test]
    fn 非容器值原样通过() {
        assert_eq!(redact_mas_token(json!("mt_x")), json!("mt_x"));
        assert_eq!(redact_mas_token(json!(42)), json!(42));
        assert_eq!(redact_mas_token(json!(null)), json!(null));
        assert_eq!(redact_mas_token(json!([1, "a"])), json!([1, "a"]));
    }
}
