//! 插件网关（`PluginGateway`）：插件间发现与互调的中台代调面。
//!
//! 插件之间不直连：直连会绕过中台的治理、审计、熔断与身份体系。同步互调一律
//! A→hub→B，下游复用与 flow 执行同一条 [`hub_engine::Invoker`] 链路；发现三件套
//! 只是把 hub-store 已有的注册表查询翻成插件够得着的 gRPC，不另立事实源。
//!
//! **结果语义**（与 `StateService::publish` 的 `accepted/reason` 同一哲学）：
//! 被策略、配额、防环拦下，以及下游拒绝或出错，都是**业务结果**——走响应字段
//! （`outcome` + `reason` / `issues`），调用方据此决定是改逻辑还是退避重试；
//! 只有基础设施故障（未鉴权、本实例没装配调用能力、记账失败）才返回 gRPC Status。

use std::collections::HashMap;
use std::time::Instant;

use chrono::Utc;
use hub_engine::{InvokeError, InvokeOutcome as EngineOutcome, Invoker};
use hub_proto::v1::plugin_gateway_server::PluginGateway;
use hub_proto::v1::{
    DescribeMessageRequest, DescribeMessageResponse, GetContractRequest, GetContractResponse,
    InvokeOutcome as ProtoOutcome, InvokeRequest, InvokeResponse, ListPluginsRequest,
    ListPluginsResponse, MessageContract, MessageEndpoint, PayloadType, PluginManifest,
    PluginSummary, ToolDecl,
};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
// manifest 字节 → PluginManifest 结构的解码走 prost 的 Message trait
use prost::Message as _;
use sqlx::PgPool;
use tonic::{Request, Response, Status};
use ulid::Ulid;

use crate::state::{STATE_TOKEN_METADATA, stamp_plugin_subject};

/// 互调链的 meta 键：链上是「已处理过该消息的插件名」序列，逗号分隔。
///
/// `Envelope.meta` 是信封里唯一的自由携带通道，链信息只能走这里。
/// 与 `hub.publish_chain`（flow 触发链）是两套互不知晓的链：跨机制成环
/// （互调里发 Publish、Publish 的 flow 里再互调）在本期不严防，
/// 双向各 8 层深度 + 各自的配额兜底，见设计文档的风险节。
pub const CALL_CHAIN_META: &str = "hub.call_chain";

/// 互调链长度上限（含本次 caller）。
///
/// 与「环检测」互补：环检测挡 A→B→A 这种短环，这一条挡「没有重复节点却
/// 无限接力」的链。取 8 与 Publish 的深度上限同值——正常的调用链 2–4 跳，
/// 8 已相当宽松，而它越大，一次风暴波及的插件越多。
pub const MAX_INVOKE_DEPTH: usize = 8;

/// 每个插件每分钟最多发起多少次互调。
///
/// 先用常量不给旋钮的理由与 Publish 的配额相同：它需要按真实流量校准，
/// 但没有证据之前，配错的旋钮等于没限额。
pub const INVOKE_QUOTA_PER_MINUTE: i64 = 60;

/// 互调配额的 Redis 键前缀。
///
/// 常量住在这里并导出：跨语言契约文件（`hub-rules.json` 的 gateway 小节）
/// 钉住它，SDK 与本实现的键名漂移会在契约测试里红。
pub const INVOKE_QUOTA_KEY_PREFIX: &str = "hub:invoke_quota";

/// `timeout_ms` 为 0 时的兜底预算（毫秒）。
///
/// 0 是「没填」而不是「不等结果」：把没填直译成零预算，等于让所有没配
/// 超时的调用瞬间失败——那是最难查的一类「中台随机抽风」。
const DEFAULT_INVOKE_TIMEOUT_MS: i64 = 30_000;

/// 互调的权限策略。
///
/// manifest 的 `invokes` 声明是**可选**的（老插件没有这个字段），没声明时
/// 听谁的就是这个开关：[`CallPolicy::Allow`] 放行（默认，存量插件零改动）；
/// [`CallPolicy::Declared`] 要求 caller 在其注册版 manifest 里声明过目标插件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPolicy {
    /// 放行未声明 `invokes` 的互调
    Allow,
    /// 只放行 manifest 声明过的目标
    Declared,
}

/// 校验状态凭证并返回插件名。`StateService` 与 [`GatewayService`] 共用的认证原语。
///
/// 身份来自**注册时下发的凭证**（metadata `x-hub-state-token`），不是插件自报的
/// 任何字段。每次调用都直查 PG：库是凭证的权威来源，缓存到内存会让「中台重启后
/// 凭证还在但内存表空了」这类问题重新出现；注册与状态调用都不频繁，先直查。
pub(crate) async fn authenticate_plugin<T>(
    pool: &PgPool,
    request: &Request<T>,
) -> Result<String, Status> {
    let token = request
        .metadata()
        .get(STATE_TOKEN_METADATA)
        .and_then(|value| value.to_str().ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Status::unauthenticated("缺少 x-hub-state-token"))?;

    hub_store::instances::plugin_of_state_token(pool, token)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "状态凭证校验失败");
            Status::internal("状态凭证校验失败")
        })?
        .ok_or_else(|| Status::unauthenticated("状态凭证无效或已失效"))
}

/// 从信封的 `meta` 里取出互调链。
///
/// 编码是逗号分隔的插件名（见 [`CALL_CHAIN_META`]）。**空段必须滤掉**：上游没设
/// 这个键时它可能是空串，`split(',')` 会给出空元素，而空元素会作为一个「叫空
/// 字符串的插件」参与环检测——脏数据不该有语义。与 `state::publish_chain`
/// 同一写法，只是键不同。
fn call_chain(meta: &HashMap<String, String>) -> Vec<String> {
    meta.get(CALL_CHAIN_META)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 链校验。`Err(原因)` = 这次互调必须以 `outcome=ERROR` 拒绝。
///
/// 链的语义是「已处理过该消息的插件序列」：caller 若已在链上，说明它正在处理
/// 这条消息又试图调回来——那就是环。链深判的是 `len + 1`：本次 caller 即将入链。
fn check_call_chain(chain: &[String], caller: &str) -> Result<(), String> {
    if chain.iter().any(|node| node == caller) {
        return Err(format!(
            "检测到互调环: {caller} 已在调用链上（{}）",
            chain.join(" → ")
        ));
    }
    if chain.len() + 1 > MAX_INVOKE_DEPTH {
        return Err(format!(
            "互调链已达上限（{MAX_INVOKE_DEPTH}），当前 {} 段（{}）",
            chain.len(),
            chain.join(" → ")
        ));
    }
    Ok(())
}

/// deadline 夹紧：`min(传入 deadline, now + timeout)`。
///
/// - 传入缺失（`<=0`）或**已过期**时用 `now + timeout` 重新起算：沿用一个已过期
///   的 deadline 会让调用瞬间超时，调用方多半只是忘了填或时钟有偏差，不值得
///   一票否决；「更晚」的 deadline 则必须被夹下来——本次调用的超时预算是调用方
///   显式给的意愿，不允许被顶掉。
/// - `timeout_ms == 0` 视为没填，给 [`DEFAULT_INVOKE_TIMEOUT_MS`] 兜底。
fn clamp_deadline(requested_ms: i64, timeout_ms: u32, now_ms: i64) -> i64 {
    let budget = if timeout_ms == 0 {
        DEFAULT_INVOKE_TIMEOUT_MS
    } else {
        timeout_ms as i64
    };
    let ceiling = now_ms.saturating_add(budget);
    if requested_ms <= 0 || requested_ms < now_ms {
        ceiling
    } else {
        requested_ms.min(ceiling)
    }
}

/// 插件网关服务。
#[derive(Clone)]
pub struct GatewayService {
    pool: PgPool,
    redis: ConnectionManager,

    /// 下游代调能力。`None` = 这个实例没装配（`Invoke` 明确回 unavailable，
    /// 而不是假装调用成功——静默谎报是最难查的一类问题）。
    invoker: Option<Invoker>,

    /// 互调权限策略（默认 [`CallPolicy::Allow`]，生产由 `HUB_PLUGIN_CALL_POLICY` 驱动）。
    policy: CallPolicy,

    /// 每个插件每分钟的互调上限。生产用 [`INVOKE_QUOTA_PER_MINUTE`]，
    /// **测试里调小**——否则验一次「打满之后被拒」要发 60 条消息。
    invoke_quota: i64,
}

impl GatewayService {
    /// `redis` 由 `state::connect_redis` 建好后传进来——超时配置只能在建连时给。
    pub fn new(pool: PgPool, redis: ConnectionManager) -> Self {
        Self {
            pool,
            redis,
            invoker: None,
            policy: CallPolicy::Allow,
            invoke_quota: INVOKE_QUOTA_PER_MINUTE,
        }
    }

    /// 装上下游代调能力，开启 `Invoke`。
    pub fn with_invoker(mut self, invoker: Invoker) -> Self {
        self.invoker = Some(invoker);
        self
    }

    /// 改互调权限策略。
    pub fn with_call_policy(mut self, policy: CallPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// 改互调配额。生产不走这条路（用默认值），测试用它把窗口调小。
    pub fn with_invoke_quota(mut self, quota: i64) -> Self {
        self.invoke_quota = quota;
        self
    }

    /// 这个插件本分钟还能不能发起互调。返回 `Some(原因)` 表示已经超了。
    ///
    /// 固定窗口 `INCR` + 首次 `EXPIRE`，与 `StateService::over_quota` 同一写法
    /// （理由也相同：先加再判靠 `INCR` 的原子性；过期只在第一次设，否则窗口
    /// 永远停在「本分钟」，计数只增不减）。
    async fn over_quota(&self, plugin: &str) -> Result<Option<String>, Status> {
        let window = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() / 60)
            .unwrap_or(0);
        let key = format!("{INVOKE_QUOTA_KEY_PREFIX}:{plugin}:{window}");

        let mut conn = self.redis.clone();
        let count: i64 = conn.incr(&key, 1).await.map_err(|err| {
            tracing::error!(error = %err, plugin, "互调配额记账失败");
            Status::internal("互调配额记账失败")
        })?;

        if count == 1 {
            let _: Result<(), _> = conn.expire(&key, 120).await;
        }

        if count > self.invoke_quota {
            return Ok(Some(format!(
                "本分钟互调已达上限 {} 次（本次是第 {count} 次）",
                self.invoke_quota
            )));
        }
        Ok(None)
    }

    /// `Declared` 策略下 caller 声明的可调名单；manifest 没声明（老插件）时为空。
    ///
    /// 链路是 hub-store 的两个查询：凭证反查拿到**实际持有这份凭证的实例**注册的
    /// 版本 id，再取该版本的 manifest 字节解出 `invokes`。多版本共存时以实例注册
    /// 的版本为准——那份 manifest 才是这个实例注册时自述的权限边界，比「最新登记
    /// 版」更贴合「谁在调用就查谁的自述」。
    async fn declared_invokes(&self, token: &str) -> Result<Vec<String>, Status> {
        let (_, version_id) = hub_store::instances::caller_of_state_token(&self.pool, token)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "互调策略查询失败");
                Status::internal("互调策略查询失败")
            })?
            .ok_or_else(|| Status::unauthenticated("状态凭证无效或已失效"))?;

        let manifest = hub_store::plugins::manifest_of_version(&self.pool, version_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, version_id, "互调策略查询失败");
                Status::internal("互调策略查询失败")
            })?
            .unwrap_or_default();

        Ok(PluginManifest::decode(manifest.as_slice())
            .map(|m| m.invokes)
            .unwrap_or_default())
    }

    /// 落一条互调的审计 span（进 trace 体系，`list_traces` / `get_trace` 可查）。
    ///
    /// **旁路语义**（与 hub-mcp 的 `record_invocation_span` 同一先例）：写入失败只记
    /// 日志不挡调用——调用能不能成与审计存不存得进是两件事。每次 `Invoke` 鉴权
    /// 通过后的**每个出口**都写一条（含被拒的），`outcome` / `reason` 记在属性里；
    /// 未鉴权的请求没有 caller 身份可记，不写。
    #[allow(clippy::too_many_arguments)]
    async fn record_invoke_span(
        &self,
        trace_id: &str,
        started_at: chrono::DateTime<Utc>,
        caller: &str,
        target: &str,
        version: &str,
        message_id: &str,
        run_id: Option<&str>,
        node_id: Option<&str>,
        call_chain: &[String],
        outcome: &str,
        status: &str,
        reason: Option<&str>,
    ) {
        let attributes = serde_json::json!({
            "caller": caller,
            "target": target,
            "version": version,
            "message_id": message_id,
            "outcome": outcome,
            "elapsed_ms": (Utc::now() - started_at).num_milliseconds().max(0),
            "call_chain": call_chain,
            "reason": reason,
        });
        // 入参不合法时 target 还没解析出来（或就是空的）：名字退回不带目标的
        // `gateway.invoke`，让这次拒绝仍有迹可循
        let span_name = if target.is_empty() {
            "gateway.invoke".to_string()
        } else {
            format!("gateway.invoke.{target}")
        };
        let span = hub_store::spans::NewSpan {
            trace_id,
            span_id: &Ulid::generate().to_string(),
            parent_span_id: None,
            run_id,
            node_id,
            name: &span_name,
            started_at,
            duration_ms: (Utc::now() - started_at).num_milliseconds().max(0),
            status,
            attributes: Some(&attributes),
        };
        if let Err(err) = hub_store::spans::insert_span(&self.pool, &span).await {
            tracing::warn!(trace_id, name = %span.name, %err, "互调审计 span 写入失败");
        }
    }
}

#[tonic::async_trait]
impl PluginGateway for GatewayService {
    /// 在线插件清单：`include_offline=false` 只列有健康实例的。
    ///
    /// 总览行里没有「最新版本号」（那是逐版本行的事），逐插件补一次
    /// `latest_version`——插件数是几十的量级，这笔 N+1 买得起，
    /// 换来的是不用为列表页单写一条聚合 SQL。
    async fn list_plugins(
        &self,
        request: Request<ListPluginsRequest>,
    ) -> Result<Response<ListPluginsResponse>, Status> {
        authenticate_plugin(&self.pool, &request).await?;
        let include_offline = request.into_inner().include_offline;

        let rows = hub_store::plugins::list_plugins(&self.pool)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "插件清单查询失败");
                Status::internal("插件清单查询失败")
            })?;

        let mut plugins = Vec::with_capacity(rows.len());
        for row in rows {
            // 口径与 MCP 工具面一致：instance_count 统计的是 status='healthy' 的实例
            let online = row.instance_count > 0;
            if !include_offline && !online {
                continue;
            }
            let latest = hub_store::plugins::latest_version(&self.pool, row.id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, plugin = %row.name, "最新版本查询失败");
                    Status::internal("插件清单查询失败")
                })?;
            plugins.push(PluginSummary {
                name: row.name,
                latest_version: latest.map(|v| v.version).unwrap_or_default(),
                online,
                // 库里是 i64；计数值不会超出 u32，截断只是让编译器满意
                instance_count: row.instance_count as u32,
                description: row.description,
            });
        }

        Ok(Response::new(ListPluginsResponse { plugins }))
    }

    /// 消息类型 → 生产者 / 消费者。`versions_by_fq_name` 现成，两个方向各查一次。
    async fn describe_message(
        &self,
        request: Request<DescribeMessageRequest>,
    ) -> Result<Response<DescribeMessageResponse>, Status> {
        authenticate_plugin(&self.pool, &request).await?;
        let inner = request.into_inner();

        let into_endpoints = |rows: Vec<(String, String)>| -> Vec<MessageEndpoint> {
            rows.into_iter()
                .map(|(plugin, version)| MessageEndpoint { plugin, version })
                .collect()
        };
        let producers = hub_store::plugins::versions_by_fq_name(
            &self.pool,
            &inner.fq_name,
            hub_store::model::ContractRow::PRODUCES,
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "消息端点查询失败");
            Status::internal("消息端点查询失败")
        })?;
        let consumers = hub_store::plugins::versions_by_fq_name(
            &self.pool,
            &inner.fq_name,
            hub_store::model::ContractRow::CONSUMES,
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "消息端点查询失败");
            Status::internal("消息端点查询失败")
        })?;

        Ok(Response::new(DescribeMessageResponse {
            producers: into_endpoints(producers),
            consumers: into_endpoints(consumers),
        }))
    }

    /// 插件契约 + 字段级 schema。
    ///
    /// 契约行不存 description（注册时只落了 fq_name），这里给空串；
    /// `invokes` 从存库的 manifest 字节解出——它只存在 manifest 里，
    /// 没有也不值得为它单独立表。
    async fn get_contract(
        &self,
        request: Request<GetContractRequest>,
    ) -> Result<Response<GetContractResponse>, Status> {
        authenticate_plugin(&self.pool, &request).await?;
        let inner = request.into_inner();

        let name = inner.plugin.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("plugin 不能为空"));
        }
        let plugin = hub_store::plugins::find_plugin(&self.pool, name)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "插件查询失败");
                Status::internal("插件查询失败")
            })?
            .ok_or_else(|| Status::not_found(format!("插件 {name} 未注册")))?;

        // version 空 = 最新登记版本（与调用路由「不指定版本走最新」的口径一致）
        let version_row = if inner.version.is_empty() {
            hub_store::plugins::latest_version(&self.pool, plugin.id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, plugin = %plugin.name, "最新版本查询失败");
                    Status::internal("插件查询失败")
                })?
                .ok_or_else(|| Status::not_found(format!("插件 {name} 还没有任何版本")))?
        } else {
            hub_store::plugins::find_version(&self.pool, plugin.id, &inner.version)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, plugin = %plugin.name, "版本查询失败");
                    Status::internal("插件查询失败")
                })?
                .ok_or_else(|| {
                    Status::not_found(format!("插件 {name} 没有版本 {}", inner.version))
                })?
        };

        let contracts = hub_store::plugins::contracts_of(&self.pool, version_row.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, version_id = version_row.id, "契约查询失败");
                Status::internal("契约查询失败")
            })?;
        let tools = hub_store::plugins::tools_of(&self.pool, version_row.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, version_id = version_row.id, "工具查询失败");
                Status::internal("契约查询失败")
            })?;

        // manifest 注册时已过 registry 的自洽校验，解码失败只能是存储损坏——
        // 回落空名单而不是让整个契约查询报错（与 MCP 面 get_plugin 同一取舍）
        let invokes = PluginManifest::decode(version_row.manifest.as_slice())
            .map(|m| m.invokes)
            .unwrap_or_default();

        let schema_json = if inner.fq_name.is_empty() {
            String::new()
        } else {
            flatten_schema(&version_row.descriptor, &inner.fq_name)
        };

        Ok(Response::new(GetContractResponse {
            name: plugin.name,
            version: version_row.version,
            produces: contracts
                .iter()
                .filter(|c| c.direction == hub_store::model::ContractRow::PRODUCES)
                .map(|c| MessageContract {
                    fq_name: c.fq_name.clone(),
                    description: String::new(),
                })
                .collect(),
            consumes: contracts
                .iter()
                .filter(|c| c.direction == hub_store::model::ContractRow::CONSUMES)
                .map(|c| MessageContract {
                    fq_name: c.fq_name.clone(),
                    description: String::new(),
                })
                .collect(),
            invokes,
            tools: tools
                .iter()
                .map(|t| ToolDecl {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema_json: t.input_schema_json.clone(),
                    requires_approval: t.requires_approval,
                })
                .collect(),
            schema_json,
        }))
    }

    /// 同步互调 A→hub→B。
    ///
    /// 流程按设计固定为：鉴权 → 配额 → 策略 → 链校验与整备 → 代调 → 映射 → 审计。
    /// 业务结果（超限 / 未授权 / 成环 / 下游拒绝或出错）走 `outcome` + `reason`，
    /// 不返回 gRPC 错误——与 Publish 的 `accepted/reason` 同一分工。
    async fn invoke(
        &self,
        request: Request<InvokeRequest>,
    ) -> Result<Response<InvokeResponse>, Status> {
        // ---- 1. 鉴权 ----
        let caller = authenticate_plugin(&self.pool, &request).await?;
        // Declared 策略要凭**同一个凭证**反查 caller 注册的版本，先把 token 留住
        let token = request
            .metadata()
            .get(STATE_TOKEN_METADATA)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();

        let inner = request.into_inner();
        // 起点计时放在所有出口之前：鉴权之后的每个出口（含入参不合法）都写审计 span
        let started_at = Utc::now();
        let started = Instant::now();

        let envelope = inner.envelope;
        // trace_id 尽早定形：鉴权之后的每个出口都要写审计 span，都得有它。
        // 调用方带了就沿用（同一个 trace 才能从上游贯通到下游），没带（或整个信封
        // 都没带）就生成新的——被拒的请求也值得一条可查的审计记录。
        let trace_id = match envelope.as_ref() {
            Some(e) if !e.trace_id.trim().is_empty() => e.trace_id.clone(),
            _ => Ulid::generate().to_string(),
        };
        // run_id/node_id/message_id 从调用方信封原样取用：run/node 在 flow 节点里
        // 发起时会带、顶层直调为空，被拒的调用也要能按 run 归位；message_id 是
        // 数据幂等键，不改动。
        let (run_id, node_id, message_id) = match envelope.as_ref() {
            Some(e) => (
                (!e.run_id.is_empty()).then_some(e.run_id.clone()),
                (!e.node_id.is_empty()).then_some(e.node_id.clone()),
                e.message_id.clone(),
            ),
            None => (None, None, String::new()),
        };

        let target = inner.plugin.trim().to_string();
        if target.is_empty() {
            self.record_invoke_span(
                &trace_id,
                started_at,
                &caller,
                "",
                &inner.version,
                &message_id,
                run_id.as_deref(),
                node_id.as_deref(),
                &[],
                "ERROR",
                "error",
                Some("plugin 不能为空"),
            )
            .await;
            return Err(Status::invalid_argument("plugin 不能为空"));
        }
        let Some(mut envelope) = envelope else {
            // 缺信封连 message_id 都没有：trace 用上面的新 ULID 兜底，
            // 至少这次拒绝本身是可查的
            self.record_invoke_span(
                &trace_id,
                started_at,
                &caller,
                &target,
                &inner.version,
                "",
                run_id.as_deref(),
                node_id.as_deref(),
                &[],
                "ERROR",
                "error",
                Some("缺少信封"),
            )
            .await;
            return Err(Status::invalid_argument("缺少信封"));
        };
        let request_version = inner.version.clone();

        // ---- 2. 配额 ----
        if let Some(reason) = self.over_quota(&caller).await? {
            self.record_invoke_span(
                &trace_id,
                started_at,
                &caller,
                &target,
                &request_version,
                &message_id,
                run_id.as_deref(),
                node_id.as_deref(),
                &[],
                "ERROR",
                "error",
                Some(&reason),
            )
            .await;
            return Ok(Response::new(invoke_error(reason, &started)));
        }

        // ---- 3. 策略校验 ----
        if self.policy == CallPolicy::Declared {
            let declared = self.declared_invokes(&token).await?;
            if !declared.iter().any(|name| name == &target) {
                let reason = format!("未声明对 {target} 的调用授权");
                self.record_invoke_span(
                    &trace_id,
                    started_at,
                    &caller,
                    &target,
                    &request_version,
                    &message_id,
                    run_id.as_deref(),
                    node_id.as_deref(),
                    &[],
                    "ERROR",
                    "error",
                    Some(&reason),
                )
                .await;
                return Ok(Response::new(invoke_error(reason, &started)));
            }
        }

        // ---- 4. 链校验与整备 ----
        //
        // 「链 = 已处理过该消息的插件序列」。本跳的处理者是 caller 而非 target：
        // target 要等下一次调用它自己被调到时才入链。A→B→A 的环因此表现为——
        // A 第二次收到这条消息时，链里已经有 A 了。
        let mut chain = call_chain(&envelope.meta);
        if let Err(reason) = check_call_chain(&chain, &caller) {
            self.record_invoke_span(
                &trace_id,
                started_at,
                &caller,
                &target,
                &request_version,
                &message_id,
                run_id.as_deref(),
                node_id.as_deref(),
                &chain,
                "ERROR",
                "error",
                Some(&reason),
            )
            .await;
            return Ok(Response::new(invoke_error(reason, &started)));
        }
        chain.push(caller.clone());
        envelope
            .meta
            .insert(CALL_CHAIN_META.to_string(), chain.join(","));

        // 身份：无条件覆盖为 caller（与 Publish 的 `stamp_plugin_subject` 同一语义）。
        // 下游据此做的审计与二次确认才有意义。
        stamp_plugin_subject(&mut envelope, &caller);

        // deadline 夹紧（理由见 `clamp_deadline`）
        envelope.deadline_ms = clamp_deadline(
            envelope.deadline_ms,
            inner.timeout_ms,
            Utc::now().timestamp_millis(),
        );

        // type 未指定时置 REQUEST：互调是调用方主动发起的请求，不该让下游
        // 收到一个语义未知的信封
        if envelope.r#type == PayloadType::Unspecified as i32 {
            envelope.r#type = PayloadType::Request as i32;
        }
        envelope.trace_id = trace_id.clone();

        // ---- 5. 执行 ----
        let Some(invoker) = self.invoker.as_ref() else {
            let reason = "本实例未装配插件调用能力";
            self.record_invoke_span(
                &trace_id,
                started_at,
                &caller,
                &target,
                &request_version,
                &message_id,
                run_id.as_deref(),
                node_id.as_deref(),
                &chain,
                "ERROR",
                "error",
                Some(reason),
            )
            .await;
            // 基础设施配置问题用 gRPC Status：调用方重试也没用，重试是给
            // 「下游暂时不可达」准备的
            return Err(Status::unavailable(reason));
        };

        let version_arg = if request_version.is_empty() {
            None
        } else {
            Some(request_version.as_str())
        };
        let outcome = invoker.invoke(&target, version_arg, envelope).await;

        // ---- 6. 结果映射 + 7. 审计 ----
        match outcome {
            Ok(EngineOutcome::Handled {
                target,
                envelope,
                ..
            }) => {
                self.record_invoke_span(
                    &trace_id,
                    started_at,
                    &caller,
                    &target.plugin_name,
                    &target.version,
                    &message_id,
                    run_id.as_deref(),
                    node_id.as_deref(),
                    &chain,
                    "HANDLED",
                    "ok",
                    None,
                )
                .await;
                Ok(Response::new(InvokeResponse {
                    outcome: ProtoOutcome::Handled as i32,
                    envelope: Some(*envelope),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    ..Default::default()
                }))
            }
            Ok(EngineOutcome::Rejected { target, issues, .. }) => {
                let reason = "目标插件校验未通过，见 issues".to_string();
                self.record_invoke_span(
                    &trace_id,
                    started_at,
                    &caller,
                    &target.plugin_name,
                    &target.version,
                    &message_id,
                    run_id.as_deref(),
                    node_id.as_deref(),
                    &chain,
                    "REJECTED",
                    "rejected",
                    Some(&reason),
                )
                .await;
                Ok(Response::new(InvokeResponse {
                    outcome: ProtoOutcome::Rejected as i32,
                    issues,
                    reason,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    ..Default::default()
                }))
            }
            // 下游出错（解析不到实例 / 熔断 / 超时 / 调用失败）是**业务结果**：
            // 调用方该看 reason 决定重试还是降级，而不是解析 gRPC 错误字符串
            Err(err) => {
                let reason = invoke_error_reason(&err);
                self.record_invoke_span(
                    &trace_id,
                    started_at,
                    &caller,
                    &target,
                    &request_version,
                    &message_id,
                    run_id.as_deref(),
                    node_id.as_deref(),
                    &chain,
                    "ERROR",
                    "error",
                    Some(&reason),
                )
                .await;
                Ok(Response::new(invoke_error(reason, &started)))
            }
        }
    }
}

/// `outcome=ERROR` 的应答。统一从这里构造：业务错误绝不能悄悄变成 gRPC Status。
fn invoke_error(reason: impl Into<String>, started: &Instant) -> InvokeResponse {
    InvokeResponse {
        outcome: ProtoOutcome::Error as i32,
        reason: reason.into(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        ..Default::default()
    }
}

/// 下游调用失败的 reason。`Resolve` 类错误（插件不存在 / 没有健康实例）的文案
/// hub-registry 已经写得很明白，直接透传；其余（熔断、超时、调用失败）同样
/// 取 `Display`——分叉点在错误类型自身，不在网关这边再翻译一遍。
fn invoke_error_reason(err: &InvokeError) -> String {
    err.to_string()
}

/// 把一个消息类型摊平成字段级 JSON（`schema_json`）。
///
/// 只为读取复用 hub-contract，不为它新增 API。查无该消息（fq_name 不在这个插件
/// 的 descriptor 里）返回空串而不是报错：注册校验保证 manifest 声明的类型必在
/// descriptor 中，走到空串基本是调用方传了个没见过的 fq_name——「这个插件没有
/// 这个类型」本身就是有效答案。descriptor 解析失败（存储损坏）同样回落空串：
/// 坏一个字段摊平不该挡住整份契约的返回。
fn flatten_schema(descriptor: &[u8], fq_name: &str) -> String {
    let index = match hub_contract::ContractIndex::from_descriptor_set(descriptor) {
        Ok(index) => index,
        Err(err) => {
            tracing::warn!(%err, fq_name, "契约 descriptor 解析失败，schema 置空");
            return String::new();
        }
    };
    let Some(entry) = index.message(fq_name) else {
        return String::new();
    };
    let fields: Vec<serde_json::Value> = entry
        .fields
        .iter()
        .map(|f| {
            serde_json::json!({
                "number": f.number,
                "name": f.name,
                "type": f.type_desc(),
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({ "fq_name": fq_name, "fields": fields }))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain_of(raw: &str) -> Vec<String> {
        let mut meta = HashMap::new();
        meta.insert(CALL_CHAIN_META.to_string(), raw.to_string());
        call_chain(&meta)
    }

    #[test]
    fn 互调链从_meta_解析且忽略空段() {
        let empty: HashMap<String, String> = HashMap::new();
        assert!(call_chain(&empty).is_empty(), "没有这个键时链为空");
        assert!(chain_of(" , ,").is_empty(), "空段不该变成「叫空串的插件」");
        assert_eq!(chain_of(" a , b ,c"), vec!["a", "b", "c"], "去空白且保序");
    }

    #[test]
    fn 链校验拒绝成环() {
        let chain = chain_of("a,b");
        let err = check_call_chain(&chain, "a").expect_err("caller 已在链上必须拒");
        assert!(err.contains("互调环"), "原因要说清是环：{err}");
        assert!(err.contains("a → b"), "原因里带链，排查时能对上：{err}");
    }

    #[test]
    fn 链校验深度_加本次_caller_后不得超过上限() {
        // 7 段链 + 本次 caller = 8，正好顶到上限，放行
        let chain: Vec<String> = (1..=7).map(|i| format!("p{i}")).collect();
        assert!(check_call_chain(&chain, "caller").is_ok());

        // 8 段链 + 本次 caller = 9，超限
        let chain: Vec<String> = (1..=8).map(|i| format!("p{i}")).collect();
        let err = check_call_chain(&chain, "caller").expect_err("第 9 跳必须拒");
        assert!(err.contains("上限"), "原因要说清是深度：{err}");
    }

    #[test]
    fn deadline_缺失或过期时用_now_加_timeout_重新起算() {
        let now = 10_000;
        // 缺失（0）与负值都算没填
        assert_eq!(clamp_deadline(0, 5_000, now), 15_000);
        assert_eq!(clamp_deadline(-1, 5_000, now), 15_000);
        // 已过期：沿用会瞬间超时，重新起算
        assert_eq!(clamp_deadline(9_999, 5_000, now), 15_000);
    }

    #[test]
    fn deadline_取传入值与本次预算的较小者() {
        let now = 10_000;
        // 更晚的 deadline 被夹到 now+timeout：本次调用的预算是调用方的显式意愿
        assert_eq!(clamp_deadline(99_999, 5_000, now), 15_000);
        // 更早的保持：不允许把整体 deadline 往后顶
        assert_eq!(clamp_deadline(12_345, 5_000, now), 12_345);
    }

    #[test]
    fn timeout_为零时给默认预算兜底() {
        let now = 10_000;
        assert_eq!(
            clamp_deadline(0, 0, now),
            now + DEFAULT_INVOKE_TIMEOUT_MS,
            "0 是「没填」不是「不等结果」"
        );
        // timeout 兜底不能顶掉更早的整体 deadline
        assert_eq!(clamp_deadline(now + 100, 0, now), now + 100);
    }

    #[test]
    fn 查无消息或_descriptor_损坏时_schema_为空串() {
        // 空 descriptor 是合法的（只用 Struct 载荷的插件）
        assert_eq!(flatten_schema(&[], "wms.v1.OrderCreated"), "");
        // 不是合法 descriptor 字节（byte string 只装得下 ASCII，取一段越界字节）
        assert_eq!(flatten_schema(&[0xff, 0xfe, 0xfd], "wms.v1.OrderCreated"), "");
    }
}
