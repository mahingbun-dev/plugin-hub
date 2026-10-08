//! **插件发现与互调（PluginGateway）客户端**——找得到谁、调得动谁，但都经过中台。
//!
//! 插件之间**不直连**：直连会绕过中台的治理、审计、熔断与身份体系。同步互调走
//! `A→hub→B` 的代调，下游复用与 flow 执行同一条 Invoker / Governor 链路；发现
//! （清单 / 消息端点 / 契约）也只从中台取，不要旁路维护一份硬编码名单——那份名单
//! 会在别人改契约的第一个小时里过时。
//!
//! # 怎么拿到客户端
//!
//! 与 [`StateClient`](crate::StateClient) 同一款：**不要自己构造它**（本 SDK 也没给
//! 公开构造函数）。四个 RPC 全部凭注册时下发的凭证鉴权（metadata
//! `x-hub-state-token`，与状态面同一个凭证），而凭证只有骨架知道。实现
//! [`Plugin::set_gateway`](crate::Plugin::set_gateway) 即可，骨架在**每次注册成功后**
//! 注入，并随凭证轮换更新：
//!
//! ```no_run
//! # use std::sync::{Arc, Mutex};
//! # use hubkit::{Plugin, PluginError, PluginManifest, GatewayClient, ValidateResponse};
//! # use hubkit::proto::Envelope;
//! struct MyPlugin {
//!     // 骨架从注册循环调用 set_gateway，而 handle 跑在别的任务上——**同步是实现方的责任**
//!     gateway: Mutex<Option<Arc<GatewayClient>>>,
//! }
//!
//! #[hubkit::async_trait]
//! impl Plugin for MyPlugin {
//!     fn manifest(&self) -> PluginManifest { unimplemented!() }
//!     async fn validate(&self, _e: &Envelope) -> Result<ValidateResponse, PluginError> { unimplemented!() }
//!
//!     async fn handle(&self, env: Envelope) -> Result<Envelope, PluginError> {
//!         let gateway = self.gateway.lock().unwrap().clone();
//!         if let Some(gateway) = gateway {
//!             // 传当前信封让 trace 贯通、链路防环生效；GatewayError 可直接 `?` 成 PluginError
//!             let out = gateway
//!                 .invoke_plugin("order-pricer", serde_json::json!({"sku": "A-1"}), hubkit::InvokeOptions {
//!                     current_envelope: Some(&env),
//!                     ..Default::default()
//!                 })
//!                 .await?;
//!             let _ = out;
//!         }
//!         Ok(env)
//!     }
//!
//!     fn set_gateway(&self, gateway: Arc<GatewayClient>) {
//!         *self.gateway.lock().unwrap() = Some(gateway);
//!     }
//! }
//! ```
//!
//! # 结果语义：业务结果不是错误
//!
//! 中台把「下游拒绝、未授权、超限、成环」这些**业务结果**放在响应字段里
//! （outcome + reason / issues），只有基础设施故障（未鉴权、中台内部错误）才返回
//! gRPC Status。[`invoke_plugin`] 据此映射成 [`GatewayError`] 的不同变体——
//! 调用方 `match` 一下就知道该改数据重试（`Rejected`）还是该退避（`Rpc`），
//! 不必解析错误字符串。
//!
//! # 链路防环：链是「原样复制」的
//!
//! [`invoke_plugin`] 传入 `current_envelope` 时，会把它的 `meta["hub.call_chain"]`
//! **原样**带进新信封——**不要**自己在链上追加自己：链的语义是「已处理过该消息的
//! 插件序列」，中台在整备时会把**本次 caller**（凭证反查出来的那个，不是信封里
//! 自报的）追加进去，target 则要等下一次调用它被调到时才入链。SDK 若自行追加，
//! 轻则链上出现重复段，重则把没参与过的插件写进审计与防环判断。
//!
//! # 与 Publish（[`StateClient::publish`](crate::StateClient::publish)）的分工
//!
//! Publish 是异步触发：投给 flow 就返回，**拿不到处理结果**。需要下游结果、
//! 且愿意同步等的场景走这里的 `invoke_plugin`；反之「发了就走」的用 Publish，
//! 别为了一个「已受理」在互调上干等。

use std::fmt;
use std::future::Future;
use std::sync::RwLock;
use std::time::Duration;

use serde_json::Value as Json;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};

use crate::config::DEFAULT_GATEWAY_CALL_TIMEOUT;
use crate::proto::plugin_gateway_client::PluginGatewayClient;
use crate::proto::{
    DescribeMessageRequest, DescribeMessageResponse, Envelope, GetContractRequest,
    GetContractResponse, InvokeOutcome, InvokeRequest, InvokeResponse, ListPluginsRequest,
    ListPluginsResponse, PayloadType, ValidationIssue,
};
use crate::state::STATE_TOKEN_METADATA;

/// 互调链的 meta 键：链上插件名，逗号分隔。
///
/// 事实源是 `sdk/go/hubkit/testdata/hub-rules.json` 的 `gateway.callChainMeta`
/// （`tests/rules.rs` 拿它钉住本常量）。写错的表现很隐蔽：中台读不到链，
/// 防环与链深限制全部静默失效，等到出现互调成环打爆下游才暴露。
pub const CALL_CHAIN_META: &str = "hub.call_chain";

/// `timeout_ms` 缺省时中台侧的兜底预算（毫秒）。
///
/// 取自中台实现（`crates/hub-grpc/src/gateway.rs` 的 `DEFAULT_INVOKE_TIMEOUT_MS`）。
/// 0 在线协议里是「没填」而不是「不等结果」——SDK 用它对齐客户端侧的等待上限与
/// 信封 deadline 的起算，让「没配超时的调用」与中台的裁量一致，而不是客户端
/// 先在中台还在干活时放弃。
pub const DEFAULT_INVOKE_TIMEOUT_MS: u32 = 30_000;

/// 客户端等待上限在中台预算之外加的余量。
///
/// 预算管的是「下游干活的时长」，而一次 RPC 还要花在往返与中台整备上——
/// 恰好等于预算的上限会让客户端在响应回来的路上放弃，把一次成功的调用
/// 变成一次客户端侧超时。2s 对同机房往返是宽裕的。
const INVOKE_TIMEOUT_MARGIN: Duration = Duration::from_secs(2);

/// 互调的**可选项**。与其它 SDK 的同名参数一一对应；没提的字段用 [`Default`]。
///
/// ```no_run
/// # use hubkit::{InvokeOptions};
/// # use hubkit::proto::Envelope;
/// # fn demo(env: &Envelope) {
/// let opts = InvokeOptions {
///     current_envelope: Some(env),
///     ..Default::default()
/// };
/// # let _ = opts;
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct InvokeOptions<'a> {
    /// 目标版本；`None` = 目标最新版本。多版本共存时**锁版本**是更稳的选择：
    /// 最新版可能在你调到一半时被人换掉。
    pub version: Option<&'a str>,

    /// 本次调用的超时预算（毫秒）。中台把信封 deadline 夹紧到
    /// `min(传入 deadline, now + timeout_ms)`；`None` 走中台兜底
    /// （[`DEFAULT_INVOKE_TIMEOUT_MS`]）。
    pub timeout_ms: Option<u32>,

    /// 调用方**正在处理的**那条信封。给了它，本次调用才算「同一条链路里的下游」：
    /// `trace_id` / `run_id` / `node_id` 复制过去（trace 贯通），`meta` 里的互调链
    /// 原样带过去（防环生效）。顶层代码手里没有信封时就给 `None`。
    pub current_envelope: Option<&'a Envelope>,
}

/// 网关调用的错误。
///
/// 变体的划分与 [`StateError`](crate::StateError) 同一条思路：让调用方**据错误
/// 决定动作**。`Rejected` 是下游的意见（改数据重试有意义），`Error` 是中台或
/// 下游的裁决（重试同一条多半还是被拒），`Unauthenticated` 说明凭证轮换了
/// （骨架已在后台重注册），其余按普通远端故障处理。
///
/// 它实现了 [`std::error::Error`]，在插件里可以 `?` 成
/// [`PluginError`](crate::PluginError)。
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayError {
    /// 参数在本地就被拦下。
    ///
    /// 中台同样会拒（`INVALID_ARGUMENT`），但本地先拦省一次网络往返，
    /// 而且能说清是哪个参数、怎么改。
    Invalid(String),

    /// 中台拒绝：凭证缺失或已失效。
    ///
    /// **收到它时客户端已经叫醒了注册循环**去换新凭证（与状态调用同一条路），
    /// 调用方照常处理这次失败即可，不必自己重试注册。
    Unauthenticated(String),

    /// 目标插件的校验器拒绝了这次调用（outcome=REJECTED）。
    ///
    /// `issues` 是下游给出的**结构化**校验问题（字段路径 + 原因），照着改数据
    /// 重试是有意义的——这与 [`Error`](GatewayError::Error) 的本质区别。
    Rejected {
        /// 下游给的人类可读原因，可能为空（此时看 `issues`）。
        reason: String,
        /// 下游 Validate 的逐条问题。
        issues: Vec<ValidationIssue>,
    },

    /// 中台或下游裁决的业务错误（outcome=ERROR）：未授权、超限、成环、链深超限、
    /// 下游处理出错等。`reason` 说明原因；**重试同一条请求大概率还是它**。
    Error {
        /// 人类可读原因。
        reason: String,
    },

    /// 其它 gRPC 错误：连不上、超时、中台内部故障等。
    Rpc {
        /// gRPC 状态码。超时固定是 [`Code::DeadlineExceeded`]。
        code: Code,
        /// 中台给的信息，或本地生成的超时说明。
        message: String,
    },
}

impl GatewayError {
    /// 是不是「凭证被拒」。
    pub fn is_unauthenticated(&self) -> bool {
        matches!(self, GatewayError::Unauthenticated(_))
    }

    /// 是不是远端（含超时）错误——即「这次调用没走通」，而不是「被谁拒绝了」。
    pub fn is_rpc(&self) -> bool {
        matches!(self, GatewayError::Rpc { .. })
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GatewayError::Invalid(what) => write!(f, "hubkit: 网关调用参数不合法：{what}"),
            GatewayError::Unauthenticated(msg) => write!(
                f,
                "hubkit: 状态凭证被中台拒绝（{msg}）——骨架已在后台重新注册换新凭证，\
                 这次调用请按失败处理"
            ),
            GatewayError::Rejected { reason, issues } => {
                write!(
                    f,
                    "hubkit: 目标插件拒绝了调用：{}",
                    if reason.is_empty() {
                        "（未给出原因，看 issues）"
                    } else {
                        reason
                    }
                )?;
                for issue in issues {
                    write!(f, "\n  - {}: {}", issue.path, issue.message)?;
                }
                Ok(())
            }
            GatewayError::Error { reason } => write!(f, "hubkit: 互调未被执行：{reason}"),
            GatewayError::Rpc { code, message } => {
                write!(f, "hubkit: 网关调用失败（{}）：{message}", code_name(*code))
            }
        }
    }
}

impl std::error::Error for GatewayError {}

/// 把 gRPC 状态码翻成人读得懂的名字。
///
/// 与 [`crate::state`] 里那份是同一件事的同一个理由：日志要能和中台侧
/// `UNAUTHENTICATED` 那套写法对着看。刻意不抽成公共模块——两处各十来行，
/// 抽出来反而多一个「改一处忘一处」的公共表面。
fn code_name(code: Code) -> &'static str {
    match code {
        Code::Ok => "OK",
        Code::Cancelled => "CANCELLED",
        Code::Unknown => "UNKNOWN",
        Code::InvalidArgument => "INVALID_ARGUMENT",
        Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        Code::NotFound => "NOT_FOUND",
        Code::AlreadyExists => "ALREADY_EXISTS",
        Code::PermissionDenied => "PERMISSION_DENIED",
        Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        Code::FailedPrecondition => "FAILED_PRECONDITION",
        Code::Aborted => "ABORTED",
        Code::OutOfRange => "OUT_OF_RANGE",
        Code::Unimplemented => "UNIMPLEMENTED",
        Code::Internal => "INTERNAL",
        Code::Unavailable => "UNAVAILABLE",
        Code::DataLoss => "DATA_LOSS",
        Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

/// 当前时刻的 Unix 毫秒。时钟异常时给 0——信封 deadline 只会被中台的夹紧
/// 逻辑兜住，客户端不必在这里做更多事。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 组装 `invoke_plugin` 要发出去的信封。
///
/// 拆成纯函数是因为这里的每一条复制规则都值得被单独断言（链是**原样**复制的、
/// trace 是贯通的、deadline 是夹紧的）——对着网络测这些，要么测不到、要么测得很脆。
///
/// 复制清单是设计定死的，每一条都有理由：
///
/// - `trace_id` / `run_id` / `node_id` 复制——同一链路的 trace 才能贯通；
///   `span_id` **不**复制，每一跳有自己的 span。
/// - `meta[CALL_CHAIN_META]` **原样**复制，不追加自己——中台负责把本次 caller
///   追加进链（见模块文档「链路防环」）。
/// - `deadline_ms = min(now + 预算, 当前信封 deadline)`——互调不允许活得比
///   包着它的那次处理更久，那是在替已经放弃的调用方干活。
/// - `message_id` 生成新 ULID：它是**幂等键**，复制调用方的会让两次不同的互调
///   在下游被当成同一条消息去重掉。
/// - `type` 置 REQUEST：同步互调的载荷语义就是「请求」。
fn assemble_invoke_envelope(
    payload: Json,
    timeout_budget_ms: u64,
    current: Option<&Envelope>,
) -> Result<Envelope, GatewayError> {
    if !payload.is_object() {
        // 中台对非对象载荷同样拒（直接调用的载荷契约就是 JSON 对象），
        // 本地先拦能给一句「改成对象」的话，省一轮网络往返。
        return Err(GatewayError::Invalid(
            "payload 必须是 JSON 对象（直接调用的载荷契约）".to_string(),
        ));
    }

    let now = now_ms();
    // 外层 deadline 更早（且有效）时以它为准：互调不该顶掉包着它的那次处理的预算
    let deadline_ms = match current {
        Some(env) if env.deadline_ms > 0 => (now + timeout_budget_ms as i64).min(env.deadline_ms),
        _ => now + timeout_budget_ms as i64,
    };

    let mut env = Envelope {
        message_id: ulid::Ulid::generate().to_string(),
        trace_id: current.map(|e| e.trace_id.clone()).unwrap_or_else(|| {
            // 顶层直调：这是一条新链路的开始，trace 从这里起算
            ulid::Ulid::generate().to_string()
        }),
        run_id: current.map(|e| e.run_id.clone()).unwrap_or_default(),
        node_id: current.map(|e| e.node_id.clone()).unwrap_or_default(),
        deadline_ms,
        r#type: PayloadType::Request as i32,
        ..Default::default()
    };

    // 链**原样**复制：有没有、是什么，都由调用方收到的信封说了算。
    // 中台读不到这个键时按「链为空」处理，防环与链深限制也就无从谈起——
    // 所以「在插件互调里传当前信封」不只是为了 trace 好看。
    if let Some(chain) = current.and_then(|e| e.meta.get(CALL_CHAIN_META)) {
        env.meta.insert(CALL_CHAIN_META.to_string(), chain.clone());
    }

    // 载荷打包复用直接调用同一条路：Struct + type_url 的规则只在 envelope 模块有一份
    let packaged = crate::envelope::with_payload_json(&Envelope::default(), payload)
        .map_err(|e| GatewayError::Invalid(e.to_string()))?;
    env.payload = packaged.payload;
    Ok(env)
}

/// 把 `Invoke` 的响应映射成「信封或类型化错误」。
///
/// 单独成函数是为了能被直接断言——这是 [`GatewayClient::invoke_plugin`] 对调用方
/// 的核心承诺（`match` 变体决定动作），对着网络测既测不全也测不稳。
///
/// UNSPECIFIED 不是合法的业务结果（proto 里 0 只是占位）。当 ERROR 处理而不是当
/// HANDLED——把说不清的结果当成成功，是比失败更糟的错误。
fn map_invoke_result(response: InvokeResponse) -> Result<Envelope, GatewayError> {
    match InvokeOutcome::try_from(response.outcome).unwrap_or(InvokeOutcome::Unspecified) {
        // 下游把处理后的信封交回来了——调用方从它取结果载荷
        InvokeOutcome::Handled => response.envelope.ok_or_else(|| GatewayError::Rpc {
            code: Code::Internal,
            message: "中台回了 HANDLED 却没有返回信封——线契约被破坏，请排查中台版本".to_string(),
        }),
        InvokeOutcome::Rejected => Err(GatewayError::Rejected {
            reason: response.reason,
            issues: response.issues,
        }),
        InvokeOutcome::Unspecified | InvokeOutcome::Error => {
            Err(GatewayError::Error { reason: response.reason })
        }
    }
}

/// 中台插件网关（PluginGateway）的客户端：发现三件套 + 同步互调。
///
/// 由骨架在注册成功后注入给插件（见 [`crate::Plugin::set_gateway`]），
/// **插件作者不该自己构造它**——凭证只有中台知道。凭证与
/// [`StateClient`](crate::StateClient) 同一份、同一条轮换与 401 自愈路径，
/// 两个客户端只是各自持有一份引用。
///
/// 所有方法都可以并发调用，内部不需要外层再加锁：gRPC 客户端每次调用克隆一份
/// （tonic 的客户端方法要 `&mut self`，但克隆只复制一个句柄，底层连接是共享的），
/// 凭证在 [`RwLock`] 后面读写。
pub struct GatewayClient {
    /// 与注册共用的那条连接。每次调用克隆一份——tonic 的客户端要 `&mut self`，
    /// 克隆是它给的并发手段，代价只是复制一个句柄。
    client: PluginGatewayClient<Channel>,

    /// 当前凭证。会被重新注册轮换，而注册循环跑在另一个任务上，所以读写都要过锁。
    ///
    /// 用 `std::sync::RwLock` 而不是 `tokio` 的：临界区里只有一次 `clone`，
    /// 不跨 `.await`，没有 await 的锁不需要异步版本。
    token: RwLock<String>,

    /// 撞上 401 时给注册循环的信号，与状态客户端共用**同一条**通道（见
    /// [`StateClient`](crate::StateClient) 的模块文档）：401 的处置是「重新注册」，
    /// 与它是哪个面的调用无关，分成两条通道只会让注册循环多等一种信号。
    denied: tokio::sync::mpsc::Sender<()>,

    /// 发现类调用（list / describe / contract）的时间上限。
    ///
    /// 这些是**基础设施查询**，卡住了就该早点放弃把线程还给业务——与状态调用
    /// 同一个理由、同一个缺省值。互调（[`GatewayClient::invoke`]）**不**用它：
    /// 同步互调的等待时长由调用方声明的预算决定，拿这个上限去套会把合法的
    /// 慢下游全部掐死在客户端。
    call_timeout: Duration,
}

impl GatewayClient {
    /// 建一个客户端。**给骨架用**：`pub(crate)` 是有意的，插件作者拿不到凭证。
    pub(crate) fn new(
        client: PluginGatewayClient<Channel>,
        denied: tokio::sync::mpsc::Sender<()>,
        call_timeout: Duration,
    ) -> Self {
        Self {
            client,
            token: RwLock::new(String::new()),
            denied,
            call_timeout: if call_timeout.is_zero() {
                DEFAULT_GATEWAY_CALL_TIMEOUT
            } else {
                call_timeout
            },
        }
    }

    /// 换一张凭证。由注册循环在每次注册成功后调用。
    pub(crate) fn set_token(&self, token: &str) {
        // 锁中毒不该让插件停摆：凭证只是一个字符串，没有「写了一半」的中间态，
        // 恢复内部值继续用即可。
        let mut guard = self.token.write().unwrap_or_else(|e| e.into_inner());
        guard.clear();
        guard.push_str(token);
    }

    /// 当前凭证。没有时返回空串——中台会把它判成「缺少凭证」并回 401，
    /// 那正是我们要的信号（触发重注册）。
    pub(crate) fn current_token(&self) -> String {
        self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 凭证被中台拒绝时叫醒注册循环去换一张新的。
    ///
    /// 只认 `UNAUTHENTICATED`：其它错误（网络抖动、参数非法）与凭证无关，
    /// 重注册解决不了，反而会让注册循环空转。与状态客户端同一把尺子。
    fn note_denied(&self, status: &Status) {
        if status.code() != Code::Unauthenticated {
            return;
        }
        // 满了就丢——已经有一个待处理的信号，重复的没有信息量
        let _ = self.denied.try_send(());
    }

    /// 给一次调用加上时间上限，并把结果收成 `T`。
    ///
    /// 超时**刻意**产生 [`Code::DeadlineExceeded`] 而不是 `UNAUTHENTICATED`，
    /// 所以 [`GatewayClient::note_denied`] 不会被触发，注册循环不会被中台的一次
    /// 卡顿叫醒。
    async fn call<T, F>(&self, what: &str, fut: F) -> Result<T, GatewayError>
    where
        F: Future<Output = Result<Response<T>, Status>>,
    {
        self.call_within(what, self.call_timeout, fut).await
    }

    /// [`GatewayClient::call`] 的指定时长版本——互调按预算放宽等待上限用。
    async fn call_within<T, F>(
        &self,
        what: &str,
        limit: Duration,
        fut: F,
    ) -> Result<T, GatewayError>
    where
        F: Future<Output = Result<Response<T>, Status>>,
    {
        match tokio::time::timeout(limit, fut).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => {
                let code = status.code();
                let message = status.message().to_string();
                self.note_denied(&status);
                Err(match code {
                    Code::Unauthenticated => GatewayError::Unauthenticated(message),
                    other => GatewayError::Rpc {
                        code: other,
                        message,
                    },
                })
            }
            Err(_) => Err(GatewayError::Rpc {
                code: Code::DeadlineExceeded,
                message: format!("{what}超过 {:?} 的时间上限", limit),
            }),
        }
    }

    /// 把凭证挂到请求的 metadata 上。
    fn attach_token<T>(&self, request: &mut Request<T>) -> Result<(), GatewayError> {
        let token = self.current_token();
        let value = MetadataValue::try_from(token.as_str()).map_err(|_| {
            // 凭证由中台下发，走到这里说明它含非 ASCII 字符——不是插件能修的问题，
            // 但要说清是**凭证**的问题，而不是「键写错了」。
            GatewayError::Invalid(
                "中台下发的状态凭证含非 ASCII 字符，无法作为 gRPC metadata 发送".to_string(),
            )
        })?;
        request.metadata_mut().insert(STATE_TOKEN_METADATA, value);
        Ok(())
    }

    /// 在线插件清单。
    ///
    /// 插件用它回答「现在能调谁」——比旁路维护一份硬编码名单强的地方在于：
    /// 它反映的是**当下**谁注册着、谁有健康实例。
    ///
    /// `include_offline=false`（常规用法）只列有健康实例的插件；排查「我声明的
    /// 上游为什么不在列表里」时给 `true`，把离线的也带出来看。
    pub async fn list_plugins(
        &self,
        include_offline: bool,
    ) -> Result<ListPluginsResponse, GatewayError> {
        let mut request = Request::new(ListPluginsRequest { include_offline });
        self.attach_token(&mut request)?;

        let mut client = self.client.clone();
        self.call("列插件清单", client.list_plugins(request)).await
    }

    /// 消息类型 → 生产者 / 消费者。
    ///
    /// 按消息反查上下游：想订阅某条数据流、或想知道「谁在产出我能消费的东西」时用。
    /// `fq_name` 与 `MessageContract.fq_name` 同口径（如 `wms.v1.OrderCreated`）。
    pub async fn describe_message(
        &self,
        fq_name: &str,
    ) -> Result<DescribeMessageResponse, GatewayError> {
        let fq_name = fq_name.trim();
        if fq_name.is_empty() {
            return Err(GatewayError::Invalid("fq_name 不能为空".to_string()));
        }

        let mut request = Request::new(DescribeMessageRequest {
            fq_name: fq_name.to_string(),
        });
        self.attach_token(&mut request)?;

        let mut client = self.client.clone();
        self.call("查消息端点", client.describe_message(request))
            .await
    }

    /// 插件契约：produces / consumes / invokes / tools，需要时附带字段级 schema。
    ///
    /// 调别人之前先看它吃什么、吐什么。`version` 给 `None` 取最新已注册版本；
    /// `fq_name` 给 `None` 不展开 schema（`schema_json` 为空串），给了则返回该
    /// 消息摊平成的 JSON。
    pub async fn get_contract(
        &self,
        plugin: &str,
        version: Option<&str>,
        fq_name: Option<&str>,
    ) -> Result<GetContractResponse, GatewayError> {
        let plugin = plugin.trim();
        if plugin.is_empty() {
            return Err(GatewayError::Invalid("plugin 不能为空".to_string()));
        }

        let mut request = Request::new(GetContractRequest {
            plugin: plugin.to_string(),
            version: version.unwrap_or_default().trim().to_string(),
            fq_name: fq_name.unwrap_or_default().trim().to_string(),
        });
        self.attach_token(&mut request)?;

        let mut client = self.client.clone();
        self.call("查插件契约", client.get_contract(request)).await
    }

    /// 同步互调的**原始**形式：请求与响应都是线协议消息，信封由调用方自己组装。
    ///
    /// 业务结果（REJECTED / ERROR）**不**在这里映射成错误——那是
    /// [`GatewayClient::invoke_plugin`] 的职责；本方法把它们原样带给调用方，
    /// 给需要看 `elapsed_ms`、或要发非 JSON 载荷（业务类型）的高级用法。
    /// 走这条路时请自己遵守信封契约：`message_id` 必填（幂等键）、链经
    /// `meta[CALL_CHAIN_META]` 传递、`subject` 填了也会被中台覆盖。
    pub async fn invoke(&self, request: InvokeRequest) -> Result<InvokeResponse, GatewayError> {
        let target = request.plugin.trim().to_string();
        if target.is_empty() {
            return Err(GatewayError::Invalid("plugin 不能为空".to_string()));
        }
        // message_id 是下游去重的幂等键；缺失不会立刻报错，而是让「两次不同的调用」
        // 有概率被下游当成同一条消息——那种错只在重放与对账时暴露，值得本地拦一道。
        if request
            .envelope
            .as_ref()
            .is_some_and(|e| e.message_id.trim().is_empty())
        {
            return Err(GatewayError::Invalid(
                "envelope.message_id 不能为空——它是下游去重的幂等键".to_string(),
            ));
        }

        // 客户端等待上限跟随本次预算：中台会把 deadline 夹紧到 now + timeout_ms，
        // 下游不可能干得比它久；再加一份往返余量，别在响应回来的路上放弃。
        // timeout_ms=0（没填）按中台的兜底预算放宽，理由见 [DEFAULT_INVOKE_TIMEOUT_MS]。
        let budget_ms = if request.timeout_ms == 0 {
            DEFAULT_INVOKE_TIMEOUT_MS as u64
        } else {
            request.timeout_ms as u64
        };
        let limit = INVOKE_TIMEOUT_MARGIN + Duration::from_millis(budget_ms);

        let mut request = Request::new(InvokeRequest { plugin: target, ..request });
        self.attach_token(&mut request)?;

        let mut client = self.client.clone();
        self.call_within("同步互调", limit, client.invoke(request))
            .await
    }

    /// 同步互调的**便捷**形式：JSON 载荷进、下游返回的信封出。
    ///
    /// ```no_run
    /// # use std::sync::{Arc, Mutex};
    /// # use hubkit::{GatewayClient, InvokeOptions, Plugin, PluginError, PluginManifest, ValidateResponse};
    /// # use hubkit::proto::Envelope;
    /// # struct P { gateway: Mutex<Option<Arc<GatewayClient>>> }
    /// #[hubkit::async_trait]
    /// impl Plugin for P {
    ///     # fn manifest(&self) -> PluginManifest { unimplemented!() }
    ///     # async fn validate(&self, _e: &Envelope) -> Result<ValidateResponse, PluginError> { unimplemented!() }
    ///     async fn handle(&self, env: Envelope) -> Result<Envelope, PluginError> {
    ///         let gateway = self.gateway.lock().unwrap().clone().expect("骨架已注入网关客户端");
    ///         // 传当前信封：trace 贯通 + 链路防环生效
    ///         let out = gateway
    ///             .invoke_plugin("order-pricer", serde_json::json!({"sku": "A-1"}), InvokeOptions {
    ///                 current_envelope: Some(&env),
    ///                 ..Default::default()
    ///             })
    ///             .await?;   // GatewayError 直接 `?` 成 PluginError
    ///         let payload = hubkit::envelope::payload_json(&out);
    ///         # let _ = payload;
    ///         Ok(env)
    ///     }
    ///     # fn set_gateway(&self, gateway: Arc<GatewayClient>) {
    ///     #     *self.gateway.lock().unwrap() = Some(gateway);
    ///     # }
    /// }
    /// ```
    ///
    /// 需要发业务类型（非 JSON）载荷、或要看 `elapsed_ms` 时，用
    /// [`GatewayClient::invoke`] 自己组装信封。
    pub async fn invoke_plugin(
        &self,
        plugin: &str,
        payload: Json,
        options: InvokeOptions<'_>,
    ) -> Result<Envelope, GatewayError> {
        let plugin = plugin.trim();
        if plugin.is_empty() {
            return Err(GatewayError::Invalid("plugin 不能为空".to_string()));
        }

        // 没给预算按中台的兜底预算起算 deadline：信封契约里 deadline 必填，
        // 而「没填预算」的含义就是「按惯例的时长来」，两侧取同一个数才不会
        // 出现「信封说一套、中台裁量另一套」的漂移。
        let budget_ms = options.timeout_ms.unwrap_or(DEFAULT_INVOKE_TIMEOUT_MS) as u64;
        let envelope = assemble_invoke_envelope(payload, budget_ms, options.current_envelope)?;

        let response = self
            .invoke(InvokeRequest {
                plugin: plugin.to_string(),
                version: options.version.unwrap_or_default().trim().to_string(),
                envelope: Some(envelope),
                timeout_ms: options.timeout_ms.unwrap_or(0),
            })
            .await?;
        map_invoke_result(response)
    }
}

impl fmt::Debug for GatewayClient {
    /// 手写：凭证绝不能进日志——`Debug` 是最容易漏的一条泄露路径
    /// （`tracing::debug!(?client)`、断言失败时的 `{:?}` 都会带上它）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayClient")
            .field("token", &"<已隐藏>")
            .field("call_timeout", &self.call_timeout)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::payload_json;
    use crate::proto::{Severity, ValidationIssue as Issue};
    use std::collections::HashMap;

    /// 只用来验本地校验与信号逻辑的客户端：地址指向一个必然连不上的端口，
    /// 因此**任何真的走到网络的用例都会失败**——这正好保证这些用例不会偷偷依赖网络。
    ///
    /// 与 `state.rs` 的 `offline_client` 同一个理由：建 `Channel` 本身就要在一个
    /// runtime 上下文里（hyper-util 的连接器在构造时取当前 reactor），用例里的
    /// `async` 就是为此。
    async fn offline_client() -> (GatewayClient, tokio::sync::mpsc::Receiver<()>) {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        (
            GatewayClient::new(PluginGatewayClient::new(channel), tx, Duration::from_secs(1)),
            rx,
        )
    }

    fn current_envelope() -> Envelope {
        Envelope {
            message_id: "01J0CURRENT".into(),
            trace_id: "01J0TRACE".into(),
            run_id: "01J0RUN".into(),
            node_id: "node-3".into(),
            deadline_ms: now_ms() + 60_000,
            meta: HashMap::from([(CALL_CHAIN_META.to_string(), "upstream-plugin".to_string())]),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn 参数不合法时本地就拦住_不发网络请求() {
        let (client, _rx) = offline_client().await;

        for err in [
            client.describe_message("").await.unwrap_err(),
            client.describe_message("   ").await.unwrap_err(),
            client.get_contract("", None, None).await.unwrap_err(),
            client.get_contract(" ", None, None).await.unwrap_err(),
            client
                .invoke_plugin("", serde_json::json!({}), InvokeOptions::default())
                .await
                .unwrap_err(),
            // 载荷不是 JSON 对象：直接调用的载荷契约就是对象
            client
                .invoke_plugin("callee", serde_json::json!([1]), InvokeOptions::default())
                .await
                .unwrap_err(),
            // message_id 是幂等键，空了要在本地拦住
            client
                .invoke(InvokeRequest {
                    plugin: "callee".into(),
                    envelope: Some(Envelope::default()),
                    ..Default::default()
                })
                .await
                .unwrap_err(),
        ] {
            assert!(matches!(err, GatewayError::Invalid(_)), "{err:?}");
        }
    }

    #[test]
    fn 传当前信封时_trace_与链被复制_且链不被追加自己() {
        let current = current_envelope();
        let out = assemble_invoke_envelope(
            serde_json::json!({"text": "hi"}),
            DEFAULT_INVOKE_TIMEOUT_MS as u64,
            Some(&current),
        )
        .unwrap();

        // trace / run / node 原样复制：这是「同一条链路里的下游」的凭据
        assert_eq!(out.trace_id, "01J0TRACE");
        assert_eq!(out.run_id, "01J0RUN");
        assert_eq!(out.node_id, "node-3");
        // span_id 不复制：每一跳有自己的 span
        assert_eq!(out.span_id, "");

        // 链**原样**复制——没有追加本次调用方，也没有动上游写的内容。
        // 中台负责追加 caller；SDK 多写一笔，防环的判断对象就错了。
        assert_eq!(
            out.meta.get(CALL_CHAIN_META).map(String::as_str),
            Some("upstream-plugin")
        );
        assert_eq!(out.meta.len(), 1, "链之外不该凭空多出别的 meta");

        // message_id 是新 ULID：复制调用方的会让两次互调被下游当成同一条消息去重
        assert_ne!(out.message_id, current.message_id);
        assert_eq!(out.message_id.len(), 26, "ULID 是 26 个字符");
        assert!(
            out.message_id.bytes().all(|c| {
                if c.is_ascii_digit() {
                    return true;
                }
                // Crockford base32 的大写字母表里没有 I / L / O / U
                c.is_ascii_uppercase() && !matches!(c, b'I' | b'L' | b'O' | b'U')
            }),
            "ULID 用 Crockford base32，收到 {}",
            out.message_id
        );

        // type 置 REQUEST、载荷装成 Struct
        assert_eq!(out.r#type, PayloadType::Request as i32);
        assert_eq!(payload_json(&out).unwrap(), serde_json::json!({"text": "hi"}));

        // deadline 被夹到 now + 预算以内（外层还有 60s，取的是本次预算）
        assert!(out.deadline_ms <= now_ms() + DEFAULT_INVOKE_TIMEOUT_MS as i64);
    }

    #[test]
    fn 不传当前信封时_新_trace_且不带链() {
        let out = assemble_invoke_envelope(
            serde_json::json!({}),
            DEFAULT_INVOKE_TIMEOUT_MS as u64,
            None,
        )
        .unwrap();

        // 顶层直调是一条新链路：trace 从这里起算，meta 里没有链
        assert!(!out.trace_id.is_empty());
        assert_eq!(out.trace_id.len(), 26);
        assert_eq!(out.run_id, "");
        assert_eq!(out.node_id, "");
        assert!(
            !out.meta.contains_key(CALL_CHAIN_META),
            "没有上游就不该凭空造一段链"
        );
    }

    #[test]
    fn 外层_deadline_更早时_以它为准() {
        let mut current = current_envelope();
        // 外层还剩 5s，比本次预算短：互调不允许活得比包着它的那次处理更久
        current.deadline_ms = now_ms() + 5_000;

        let out =
            assemble_invoke_envelope(serde_json::json!({}), 30_000, Some(&current)).unwrap();
        assert_eq!(out.deadline_ms, current.deadline_ms);

        // 外层 deadline 已过期：夹出来的是一个已过期的 deadline。这**不是** SDK 该
        // 自作主张替调的——中台会把过期 deadline 重新起算，客户端如实传递即可。
        current.deadline_ms = now_ms() - 1_000;
        let out = assemble_invoke_envelope(serde_json::json!({}), 10_000, Some(&current)).unwrap();
        assert_eq!(out.deadline_ms, current.deadline_ms);
    }

    #[tokio::test]
    async fn 只有_401_会叫醒注册循环_且只留一个信号() {
        let (client, mut rx) = offline_client().await;

        // 与凭证无关的错误不该有信号：重注册解决不了网络抖动与参数问题，
        // 发信号只会让注册循环空转
        client.note_denied(&Status::internal("网关调用失败"));
        client.note_denied(&Status::deadline_exceeded("超时"));
        assert!(rx.try_recv().is_err(), "非 401 不该产生信号");

        client.note_denied(&Status::unauthenticated("状态凭证无效或已失效"));
        assert!(rx.try_recv().is_ok(), "401 必须产生信号");

        // 缓冲 1 + 非阻塞：并发的一堆 401 只留一个信号，不会把注册循环叫成风暴
        for _ in 0..8 {
            client.note_denied(&Status::unauthenticated("状态凭证无效或已失效"));
        }
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "重复的 401 应当被丢掉");

        // 凭证轮换：与状态客户端同一条路，注册循环每次注册成功后换新
        client.set_token("t1");
        assert_eq!(client.current_token(), "t1");
        client.set_token("");
        assert_eq!(client.current_token(), "");
    }

    #[test]
    fn 业务结果映射_rejected_带_issues_error_带_reason() {
        // map_invoke_result 是 invoke_plugin 对调用方的核心承诺：
        // match 变体就能决定动作，不必解析错误字符串。改映射等于改承诺，必须钉住。
        let err = map_invoke_result(InvokeResponse {
            outcome: InvokeOutcome::Rejected as i32,
            reason: "库存不足".into(),
            issues: vec![Issue {
                path: "payload.sku".into(),
                message: "不存在".into(),
                severity: Severity::Error as i32,
            }],
            ..Default::default()
        })
        .unwrap_err();
        let GatewayError::Rejected { reason, issues } = &err else {
            panic!("应当映射成 Rejected，实际 {err:?}")
        };
        assert_eq!(reason, "库存不足");
        assert_eq!(issues.len(), 1);
        let rendered = err.to_string();
        assert!(rendered.contains("payload.sku"), "{rendered}");

        let err = map_invoke_result(InvokeResponse {
            outcome: InvokeOutcome::Error as i32,
            reason: "未声明对 x 的调用授权".into(),
            ..Default::default()
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("未声明对 x 的调用授权"),
            "{err}"
        );

        // HANDLED 必须把信封交回来
        let env = map_invoke_result(InvokeResponse {
            outcome: InvokeOutcome::Handled as i32,
            envelope: Some(Envelope {
                message_id: "01J0OUT".into(),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(env.message_id, "01J0OUT");

        // UNSPECIFIED 不是成功：把说不清的结果当成成功是比失败更糟的错误
        assert!(matches!(
            map_invoke_result(InvokeResponse::default()),
            Err(GatewayError::Error { .. })
        ));
    }

    #[tokio::test]
    async fn debug_不打印凭证() {
        let (client, _rx) = offline_client().await;
        client.set_token("super-secret-token");
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("已隐藏"), "{rendered}");
    }
}
