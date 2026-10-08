//! **外置状态（HubState）客户端**——把「跨调用要保留的东西」放到中台去。
//!
//! 插件被**强制无状态**：实例内存不保证跨调用保留（见中台的 `docs/design.md`）。
//! 换来的是热切换零代价、水平扩展无障碍、重放与测试都简单。需要跨调用保留的东西
//! 走这里，不要再往插件的字段里存。
//!
//! [`StateClient::publish`]（触发下游 flow）也住在这个客户端里：它是 `HubState`
//! 服务的另一个 RPC，与 KV 共用同一条连接、同一份凭证——为它单开一个客户端
//! 只会多一份要轮换的凭证。
//!
//! 行为对齐 Go 侧（`sdk/go/hubkit/state.go`），几处刻意的不同都写在下面对应的条目上。
//!
//! # 怎么拿到客户端
//!
//! **不要自己构造它**（本 SDK 也没给公开构造函数）：身份来自注册时下发的凭证，
//! 而凭证只有骨架知道。实现 [`Plugin::set_state`](crate::Plugin::set_state) 即可，
//! 骨架在**每次注册成功后**注入一次：
//!
//! ```no_run
//! # use std::sync::{Arc, Mutex};
//! # use hubkit::{Plugin, PluginError, PluginManifest, StateClient, ValidateResponse};
//! # use hubkit::proto::{Envelope, ValidateResponse as VR};
//! struct MyPlugin {
//!     // 骨架从注册循环调用 set_state，而 handle 跑在别的任务上——**同步是实现方的责任**
//!     state: Mutex<Option<Arc<StateClient>>>,
//! }
//!
//! #[hubkit::async_trait]
//! impl Plugin for MyPlugin {
//!     fn manifest(&self) -> PluginManifest { unimplemented!() }
//!     async fn validate(&self, _e: &Envelope) -> Result<VR, PluginError> { unimplemented!() }
//!
//!     async fn handle(&self, env: Envelope) -> Result<Envelope, PluginError> {
//!         let state = self.state.lock().unwrap().clone();
//!         if let Some(state) = state {
//!             // 登录缓存、令牌桶这类状态放这里，别放实例内存。
//!             // StateError 实现了 std::error::Error，可以直接 `?` 成 PluginError。
//!             state.put("session", "token", b"abc", std::time::Duration::ZERO).await?;
//!             let cached = state.get("session", "token").await?;   // None = 键不存在
//!             let _ = cached;
//!         }
//!         Ok(env)
//!     }
//!
//!     fn set_state(&self, state: Arc<StateClient>) {
//!         *self.state.lock().unwrap() = Some(state);
//!     }
//! }
//! ```
//!
//! # 键长什么样
//!
//! 中台按凭证反查出插件名，强制把键拼成 `hub:state:{插件名}:{namespace}:{key}`。
//! 插件自报的 `namespace` 只是**子空间**，前缀不由插件拼。
//!
//! **但这不是插件间的隔离承诺**：凭证只证明「注册方自称是某个插件名并通过了该名字的
//! 校验」，不证明它就是那个插件。插件面按设计不鉴权，任何能连上该端口的进程都能注册
//! 一个与目标插件契约兼容的新版本、拿到反查为该名字的凭证，进而读写它的状态空间。
//! 需要真正的隔离时别指望这一层（见 `state.proto` 与中台的 `docs/design.md` 风险 11）。
//!
//! **键前缀不含版本号**：同一插件的所有版本共用一个状态空间。升版本不会清空状态
//! （对登录缓存这类状态正是要的），但两个版本往同一个 namespace 写就是**互相覆盖**。
//! 要按版本隔离，请自己把版本写进 namespace。
//!
//! # 凭证与 401
//!
//! 凭证随每次注册轮换（中台重启、实例被摘除后自愈都会重新注册）。客户端撞上
//! `UNAUTHENTICATED` 时会**叫醒注册循环**去换一张新凭证（见
//! [`StateClient::note_denied`]），但这一次调用仍以错误返回——重注册是后台的补救，
//! 不该掩盖这一次失败。
//!
//! 为什么会撞上 401：凭证是注册时下发的，而中台里的实例记录会因**中台重启**（内存/库里的
//! 凭证表重建、旧凭证不再对应任何实例）、**实例被心跳超时摘除**、或**同一实例被新注册覆盖**
//! 而失效。这些情况下插件自身的 gRPC 与心跳可能一切正常（心跳失败会另有一条自愈路径），
//! 于是只有状态调用会安静地一直失败——**没有这条通知线，插件会「活着但状态全废」**。

use std::fmt;
use std::future::Future;
use std::sync::RwLock;
use std::time::Duration;

use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};

use crate::config::DEFAULT_STATE_CALL_TIMEOUT;
use crate::proto::hub_state_client::HubStateClient;
use crate::proto::{
    Envelope, KvDeleteRequest, KvDeleteResponse, KvGetRequest, KvGetResponse, KvKey,
    KvPutRequest, KvScanRequest, KvScanResponse, PublishRequest, PublishResponse,
};
use crate::rules::valid_state_segment;

/// 状态凭证的 metadata 键，必须与中台侧一致（中台的 `STATE_TOKEN_METADATA`，
/// 现在住在 `crates/hub-core/src/state.rs`，由 `crates/hub-grpc/src/state.rs` 再导出）。
///
/// 导出它而不是让各处各写一份字面量：写错的话中台会判成「无凭证」，
/// 而插件侧只会看到一个 401，很难查。契约测试拿
/// `sdk/go/hubkit/testdata/hub-rules.json` 的 `stateTokenMetadata` 钉住它。
pub const STATE_TOKEN_METADATA: &str = "x-hub-state-token";

/// 单个值的上限：1 MiB。
///
/// 取自中台实现（`crates/hub-core/src/state.rs` 的 `MAX_VALUE_BYTES`，
/// 中台侧由 `hub-grpc` 再导出）——**不要改这里**，两边不一致时的表现是
/// 「本地放行、中台拒绝」，比两边都拒更难看。HubState 不是对象存储，
/// 大块内容请走 blob。
///
/// 客户端本地先拦一道，给的是能直接照着改的错，而不是服务端那句
/// `INVALID_ARGUMENT: value 超过上限 1048576 字节，收到 …`。
/// `tests/rules.rs` 有一条测试在中台源码里扫这个定义，防止两份悄悄漂移。
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;

/// 单次 Scan 的硬上限：1000 条。
///
/// 同 [`MAX_VALUE_BYTES`]，取自中台的 `MAX_SCAN_LIMIT`。
/// 它的用途是「别让插件一次拉走整个命名空间」，因此客户端**不提供**「不填就是全扫」的
/// 默认值：limit 必须显式给，且必须在 `1..=MAX_SCAN_LIMIT` 之内。
pub const MAX_SCAN_LIMIT: u32 = 1000;

/// 扫描返回的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateEntry {
    /// 键名，**不带**中台加的前缀，就是当初写进去的那个 key。
    pub key: String,
    /// 值。键存在但值是空字节时这里是空 `Vec`，与 `KvGet` 的 `found` 一样分得清。
    pub value: Vec<u8>,
}

/// 状态调用的错误。
///
/// 分成三类是为了让调用方能**据错误决定动作**：本地拦下的改参数就好，
/// 401 说明凭证轮换了（骨架已在后台重注册，稍后重试多半就好了），
/// 其余的按普通的远端故障处理。
///
/// 它实现了 [`std::error::Error`]，所以在插件里可以 `?` 成
/// [`PluginError`](crate::PluginError)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    /// 参数在本地就被拦下。
    ///
    /// 中台同样会拒（`INVALID_ARGUMENT`），但本地先拦省一次网络往返，
    /// 而且错误信息能说清是哪一条规则、怎么改。
    Invalid(String),

    /// 中台拒绝：凭证缺失或已失效。
    ///
    /// **收到它时客户端已经叫醒了注册循环**去换新凭证，调用方通常只需照常
    /// 处理这次失败（fail-open 还是 fail-closed 由业务定），不必自己重试注册。
    Unauthenticated(String),

    /// `publish` 被中台受理了却**没有投出去**：防环、链深或配额拦下的业务结果。
    ///
    /// 刻意做成错误而不是「`accepted=false` 的成功值」：拿 reason 当普通字段返回，
    /// 调用方漏看一个布尔位，消息就**安静地**没发出去——而 `?` 与错误路径是
    /// Rust 调用方不会漏看的通道。中台侧同款语义见 `crates/hub-grpc/src/state.rs`
    /// 的 `rejected`。
    Rejected(String),

    /// 其它 gRPC 错误：连不上、超时、中台内部故障等。
    Rpc {
        /// gRPC 状态码。超时固定是 [`Code::DeadlineExceeded`]。
        code: Code,
        /// 中台给的信息，或本地生成的超时说明。
        message: String,
    },
}

impl StateError {
    /// 是不是「凭证被拒」。
    pub fn is_unauthenticated(&self) -> bool {
        matches!(self, StateError::Unauthenticated(_))
    }

    /// 是不是远端（含超时）错误——即「这次调用没走通」，而不是「参数写错了」。
    pub fn is_rpc(&self) -> bool {
        matches!(self, StateError::Rpc { .. })
    }
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::Invalid(what) => write!(f, "hubkit: 状态调用参数不合法：{what}"),
            StateError::Unauthenticated(msg) => write!(
                f,
                "hubkit: 状态凭证被中台拒绝（{msg}）——骨架已在后台重新注册换新凭证，\
                 这次调用请按失败处理"
            ),
            StateError::Rejected(reason) => write!(
                f,
                "hubkit: 发布未投出：{reason}（防环或配额拦下的业务结果，\
                 重试同一条多半还是被拒）"
            ),
            StateError::Rpc { code, message } => {
                write!(f, "hubkit: 状态调用失败（{}）：{message}", code_name(*code))
            }
        }
    }
}

impl std::error::Error for StateError {}

/// 把 gRPC 状态码翻成人读得懂的名字。
///
/// `Code` 的 `Debug` 出来是 `DeadlineExceeded` 这种驼峰，日志里与别处的大写下划线风格
/// 不一致；而 `Code::as_str()` 给的是 `deadline-exceeded`。这里统一成 `UNAUTHENTICATED`
/// 那一套——与中台侧错误码的写法一致，排障时两种日志能对着看。
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

/// 中台外置状态（HubState）的客户端。
///
/// 由骨架在注册成功后注入给插件（见 [`Plugin::set_state`](crate::Plugin::set_state)），
/// **插件作者不该自己构造它**——凭证只有中台知道。
///
/// 所有方法都可以并发调用，内部不需要外层再加锁：gRPC 客户端每次调用克隆一份
/// （tonic 的客户端方法要 `&mut self`，但克隆只复制一个句柄，底层连接是共享的），
/// 凭证在 [`RwLock`] 后面读写。
pub struct StateClient {
    /// 与注册共用的那条连接。每次调用克隆一份——tonic 的客户端要 `&mut self`，
    /// 克隆是它给的并发手段，代价只是复制一个句柄。
    client: HubStateClient<Channel>,

    /// 当前凭证。会被重新注册轮换，而注册循环跑在另一个任务上，所以读写都要过锁。
    ///
    /// 用 `std::sync::RwLock` 而不是 `tokio` 的：临界区里只有一次 `clone`，
    /// 不跨 `.await`，没有 await 的锁不需要异步版本。
    token: RwLock<String>,

    /// 撞上 401 时给注册循环的信号，见 [`StateClient::note_denied`]。
    denied: tokio::sync::mpsc::Sender<()>,

    /// 单次调用的时间上限。
    call_timeout: Duration,
}

impl StateClient {
    /// 建一个客户端。**给骨架用**：`pub(crate)` 是有意的，插件作者拿不到凭证。
    pub(crate) fn new(
        client: HubStateClient<Channel>,
        denied: tokio::sync::mpsc::Sender<()>,
        call_timeout: Duration,
    ) -> Self {
        Self {
            client,
            token: RwLock::new(String::new()),
            denied,
            call_timeout: if call_timeout.is_zero() {
                DEFAULT_STATE_CALL_TIMEOUT
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
    ///
    /// **注销也从这里取**（见 [`crate::run`] 里那条优雅退出路径），因此它是 `pub(crate)`
    /// 而不是私有的：中台只在 `RegisterResponse.state_token` 里下发**一份**凭证，状态调用
    /// 拿它做 metadata、注销拿它证明「我是这一行的主人」，两者本就是同一个东西。
    /// 所以不另存一份——两份存法除了「写了一份忘另一份」之外没有别的可能，而那种错只在
    /// 进程退出那一刻才暴露，测试也很难碰上。
    pub(crate) fn current_token(&self) -> String {
        self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 凭证被中台拒绝时叫醒注册循环去换一张新的。
    ///
    /// 只认 `UNAUTHENTICATED`：其它错误（网络抖动、参数非法）与凭证无关，重注册解决不了，
    /// 反而会让注册循环空转。
    ///
    /// 信号是**缓冲 1 + 非阻塞发送**：并发调用一起撞上 401 时只留一个信号，
    /// 不会把注册循环叫成风暴；调用方不需要等注册循环读完才返回。
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
    /// 所以 [`StateClient::note_denied`] 不会被触发，注册循环不会被中台的一次卡顿叫醒。
    ///
    /// 上限取 2s（见 [`crate::config::DEFAULT_STATE_CALL_TIMEOUT`]）：客户端应**短于**中台侧的
    /// Redis 响应超时（5s），由客户端先放弃，插件才有机会走 fail-open。
    async fn call<T, F>(&self, what: &str, fut: F) -> Result<T, StateError>
    where
        F: Future<Output = Result<Response<T>, Status>>,
    {
        match tokio::time::timeout(self.call_timeout, fut).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => {
                let code = status.code();
                let message = status.message().to_string();
                self.note_denied(&status);
                Err(match code {
                    Code::Unauthenticated => StateError::Unauthenticated(message),
                    other => StateError::Rpc {
                        code: other,
                        message,
                    },
                })
            }
            Err(_) => Err(StateError::Rpc {
                code: Code::DeadlineExceeded,
                message: format!("{what}超过 {:?} 的时间上限", self.call_timeout),
            }),
        }
    }

    /// 把凭证挂到请求的 metadata 上。
    fn attach_token<T>(&self, request: &mut Request<T>) -> Result<(), StateError> {
        let token = self.current_token();
        let value = MetadataValue::try_from(token.as_str()).map_err(|_| {
            // 凭证由中台下发，走到这里说明它含非 ASCII 字符——不是插件能修的问题，
            // 但要说清是**凭证**的问题，而不是「键写错了」。
            StateError::Invalid(
                "中台下发的状态凭证含非 ASCII 字符，无法作为 gRPC metadata 发送".to_string(),
            )
        })?;
        request.metadata_mut().insert(STATE_TOKEN_METADATA, value);
        Ok(())
    }

    /// 读一个键。
    ///
    /// `Ok(None)` 是「键不存在」，`Ok(Some(vec![]))` 是「值是空字节」——这两回事
    /// 中台分得清（`KvGetResponse.found`），这里也分得清。
    pub async fn get(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>, StateError> {
        check_segment("namespace", namespace)?;
        check_segment("key", key)?;

        let mut request = Request::new(KvGetRequest {
            key: Some(KvKey {
                namespace: namespace.to_string(),
                key: key.to_string(),
            }),
        });
        self.attach_token(&mut request)?;

        let mut client = self.client.clone();
        let response: KvGetResponse = self.call("读状态", client.kv_get(request)).await?;
        Ok(if response.found {
            Some(response.value)
        } else {
            None
        })
    }

    /// 写一个键。`ttl` 为 [`Duration::ZERO`] 表示不过期。
    ///
    /// 注意：TTL 的粒度是秒（线协议是 `ttl_seconds`），不足 1 秒的 ttl 会被截断为 0，
    /// 也就是**永不过期**。想让键很快消失，请传 >= 1s 的值。
    pub async fn put(
        &self,
        namespace: &str,
        key: &str,
        value: &[u8],
        ttl: Duration,
    ) -> Result<(), StateError> {
        check_segment("namespace", namespace)?;
        check_segment("key", key)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(StateError::Invalid(format!(
                "value 是 {} 字节，超过上限 {MAX_VALUE_BYTES} 字节（1 MiB）——\
                 HubState 不是对象存储，大块内容请走 blob",
                value.len()
            )));
        }

        let mut request = Request::new(KvPutRequest {
            key: Some(KvKey {
                namespace: namespace.to_string(),
                key: key.to_string(),
            }),
            value: value.to_vec(),
            // 秒级截断与 Go 侧一致（`ttl / time.Second`）。`as_secs` 已是饱和转换，
            // 再超出 i64 就用 i64::MAX 兜底，不 panic。
            ttl_seconds: i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX),
        });
        self.attach_token(&mut request)?;

        let mut client = self.client.clone();
        self.call("写状态", client.kv_put(request)).await?;
        Ok(())
    }

    /// 删一个键。删不存在的键返回 `Ok(false)`，不是错误。
    pub async fn delete(&self, namespace: &str, key: &str) -> Result<bool, StateError> {
        check_segment("namespace", namespace)?;
        check_segment("key", key)?;

        let mut request = Request::new(KvDeleteRequest {
            key: Some(KvKey {
                namespace: namespace.to_string(),
                key: key.to_string(),
            }),
        });
        self.attach_token(&mut request)?;

        let mut client = self.client.clone();
        let response: KvDeleteResponse = self.call("删状态", client.kv_delete(request)).await?;
        Ok(response.deleted)
    }

    /// 扫描一个命名空间下前缀匹配的键，最多 `limit` 条。
    ///
    /// `prefix` 允许为空串（扫整个命名空间）；非空时字符规则与键名一致。
    ///
    /// `limit` 必须显式给且在 `1..=MAX_SCAN_LIMIT` 之内——中台对 0 和超限都回
    /// `INVALID_ARGUMENT`，本地先拦能把「要改哪个参数」直接说清楚。
    pub async fn scan(
        &self,
        namespace: &str,
        prefix: &str,
        limit: u32,
    ) -> Result<Vec<StateEntry>, StateError> {
        check_segment("namespace", namespace)?;
        // 空前缀合法：扫整个命名空间（中台侧同一条判断）
        if !prefix.is_empty() {
            check_segment("prefix", prefix)?;
        }
        if limit == 0 || limit > MAX_SCAN_LIMIT {
            return Err(StateError::Invalid(format!(
                "limit 必须在 1..={MAX_SCAN_LIMIT} 之间，收到 {limit}"
            )));
        }

        let mut request = Request::new(KvScanRequest {
            namespace: namespace.to_string(),
            prefix: prefix.to_string(),
            limit,
        });
        self.attach_token(&mut request)?;

        let mut client = self.client.clone();
        let response: KvScanResponse = self.call("扫描状态", client.kv_scan(request)).await?;
        Ok(response
            .entries
            .into_iter()
            .map(|e| StateEntry {
                key: e.key,
                value: e.value,
            })
            .collect())
    }

    /// 把一条信封投给目标 flow（异步触发下游，**拿不到处理结果**）。
    ///
    /// 受理了返回执行 id（`run_id`），拿它去链路里查「这条消息后来怎么样了」。
    /// 需要下游**处理结果**的场景别用这里——那要等、等不到、还不该等，去用
    /// [`GatewayClient`](crate::GatewayClient) 的同步互调。
    ///
    /// 「被防环、链深或配额拦下」**是**错误（[`StateError::Rejected`]，reason 随
    /// 错误携带）：这是业务裁决，重试同一条多半还是被拒；与「中台内部故障该退避」
    /// 的 [`StateError::Rpc`] 是两回事，调用方分得清才能选对动作。
    ///
    /// 信封的 `subject` 会被中台无条件覆盖为**本插件**的身份——填了也没用，
    /// 那是防冒用的设计，不是疏忽。
    pub async fn publish(&self, target: &str, envelope: Envelope) -> Result<String, StateError> {
        // target 是 flow 名（不是 KV 键段），中台只要求非空——这里同一条判断，
        // 不套用 namespace/key 的字符集规则
        let target = target.trim();
        if target.is_empty() {
            return Err(StateError::Invalid("target 不能为空".to_string()));
        }
        // message_id 是总线的幂等键（at-least-once 投递靠它去重）；缺失不会立刻报错，
        // 而是让「同一条消息投两次」在下游变成两次处理——值得本地拦一道。
        if envelope.message_id.trim().is_empty() {
            return Err(StateError::Invalid(
                "envelope.message_id 不能为空——它是总线去重的幂等键".to_string(),
            ));
        }

        let mut request = Request::new(PublishRequest {
            target: target.to_string(),
            envelope: Some(envelope),
        });
        self.attach_token(&mut request)?;

        let mut client = self.client.clone();
        let response: PublishResponse = self.call("发布消息", client.publish(request)).await?;
        if response.accepted {
            Ok(response.run_id)
        } else {
            // 受理了却没投出去：reason 是中台给的业务裁决（成环 / 超限），
            // 必须原样带给调用方——吞掉它，插件只会看到「发了但没反应」。
            Err(StateError::Rejected(response.reason))
        }
    }
}

impl fmt::Debug for StateClient {
    /// 手写：凭证绝不能进日志——`Debug` 是最容易漏的一条泄露路径
    /// （`tracing::debug!(?client)`、断言失败时的 `{:?}` 都会带上它）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateClient")
            .field("token", &"<已隐藏>")
            .field("call_timeout", &self.call_timeout)
            .finish_non_exhaustive()
    }
}

/// 本地拦一道键名规则，规则本身与中台共用（[`crate::rules::valid_state_segment`]）。
///
/// 消息里**带上长度上限**：中台那句话说全了规则，本地可以说得更直白，
/// 而且这一条不需要一次网络往返就能给出。
fn check_segment(what: &str, value: &str) -> Result<(), StateError> {
    if valid_state_segment(value) {
        return Ok(());
    }
    Err(StateError::Invalid(format!(
        "{what} 只允许 [A-Za-z0-9_.-]、非空、最长 200 字节，收到 {value:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只用来验本地校验与信号逻辑的客户端：地址指向一个必然连不上的端口，
    /// 因此**任何真的走到网络的用例都会失败**——这正好保证这些用例不会偷偷依赖网络。
    ///
    /// 这些用例都是 `#[tokio::test]`：建 `Channel` 本身就要在一个 runtime 上下文里
    /// （hyper-util 的连接器在构造时取当前 reactor），用例里那一次 `.await` 就是为此。
    async fn offline_client(
        call_timeout: Duration,
    ) -> (StateClient, tokio::sync::mpsc::Receiver<()>) {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        (
            StateClient::new(HubStateClient::new(channel), tx, call_timeout),
            rx,
        )
    }

    #[tokio::test]
    async fn 键名非法时本地就拦住_且信息里带上长度上限() {
        let (client, _rx) = offline_client(Duration::from_secs(1)).await;

        // 这几条都必须在**发起调用之前**被拦下，否则会变成一次网络往返
        for bad in ["", "a:b", "a*", "中文", "a b"] {
            let err = client.get(bad, "k").await.unwrap_err();
            assert!(matches!(err, StateError::Invalid(_)), "{bad:?} -> {err:?}");
            assert!(err.to_string().contains("200 字节"), "{err}");
        }
        assert!(matches!(
            client.get("ns", "*").await.unwrap_err(),
            StateError::Invalid(_)
        ));
        assert!(matches!(
            client.delete("ns", "").await.unwrap_err(),
            StateError::Invalid(_)
        ));
        // 201 字节也要拦——规则里的长度上限不能只在文档里
        let too_long = "a".repeat(201);
        assert!(matches!(
            client.get("ns", &too_long).await.unwrap_err(),
            StateError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn 超限的值与_limit_在本地就被拦下() {
        let (client, _rx) = offline_client(Duration::from_secs(1)).await;

        // 比上限多一个字节必须本地就拒
        let err = client
            .put("ns", "k", &vec![0u8; MAX_VALUE_BYTES + 1], Duration::ZERO)
            .await
            .unwrap_err();
        assert!(matches!(err, StateError::Invalid(_)), "{err:?}");
        assert!(err.to_string().contains("1048576"), "{err}");

        for limit in [0, MAX_SCAN_LIMIT + 1, u32::MAX] {
            let err = client.scan("ns", "", limit).await.unwrap_err();
            assert!(matches!(err, StateError::Invalid(_)), "limit={limit}");
            assert!(err.to_string().contains("1..=1000"), "{err}");
        }
        // 非空前缀同样过规则判定
        assert!(matches!(
            client.scan("ns", "a:b", 10).await.unwrap_err(),
            StateError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn 只有_401_会叫醒注册循环_且只留一个信号() {
        let (client, mut rx) = offline_client(Duration::from_secs(1)).await;

        // 与凭证无关的错误不该有信号：重注册解决不了网络抖动与参数问题，
        // 发信号只会让注册循环空转
        client.note_denied(&Status::internal("读状态失败"));
        client.note_denied(&Status::deadline_exceeded("超时"));
        client.note_denied(&Status::invalid_argument("namespace 不合法"));
        assert!(rx.try_recv().is_err(), "非 401 不该产生信号");

        client.note_denied(&Status::unauthenticated("状态凭证无效或已失效"));
        assert!(rx.try_recv().is_ok(), "401 必须产生信号");

        // 缓冲 1 + 非阻塞：并发的一堆 401 只留一个信号，不会把注册循环叫成风暴
        for _ in 0..8 {
            client.note_denied(&Status::unauthenticated("状态凭证无效或已失效"));
        }
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "重复的 401 应当被丢掉");
    }

    #[tokio::test]
    async fn 凭证可被轮换且并发读不panic() {
        let (client, _rx) = offline_client(Duration::from_secs(1)).await;
        assert_eq!(client.current_token(), "", "初始没有凭证");

        client.set_token("t1");
        assert_eq!(client.current_token(), "t1");
        // 重新注册会换新的（中台重启、实例被摘除后自愈都走这条）
        client.set_token("t2");
        assert_eq!(client.current_token(), "t2");
        // 空凭证也要能覆盖掉旧的——它会让中台回 401，那正是要的信号
        client.set_token("");
        assert_eq!(client.current_token(), "");

        // 注册循环在别的任务/线程上换凭证，插件自己的调用同时在读——不该 panic、不该撕裂
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..200 {
                        client.set_token("rotating");
                        let _ = client.current_token();
                    }
                });
            }
        });
    }

    #[tokio::test]
    async fn debug_不打印凭证() {
        let (client, _rx) = offline_client(Duration::from_secs(1)).await;
        client.set_token("super-secret-token");
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("已隐藏"), "{rendered}");
    }
}
