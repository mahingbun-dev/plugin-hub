//! 本地 mock 中台：给插件开发者的「L3 替身」。
//!
//! 插件接入有四道关，第三关是「中台接受注册」。这一关**必须有中台**才能验，
//! 而开发者本地通常没有（起一个真中台要 PostgreSQL + Redis + 迁移）。这个 mock 就是
//! 那一关的替身：一个进程、无外部依赖，插件照常往 `:8093` 注册。
//!
//! # 它与真中台的关系
//!
//! **判据同源，不是照抄**：注册校验的每一步都调 `hub-registry` 里那个真中台也在调的
//! 函数——[`hub_registry::reject::preflight`]（manifest 在不在、descriptor 能不能解析、
//! manifest 自不自洽）、[`hub_registry::reject::probe_rejection`]（可达性）、
//! [`hub_registry::reject::instance_id_format_rejection`] /
//! [`hub_registry::reject::instance_id_owner_rejection`]（实例标识）、
//! [`hub_contract::check_compatibility`]（契约兼容）。
//!
//! 这些函数本来是 `Registry::try_register` 里的一段段内联代码，为了这个 mock 才抽出来
//! 成为公开函数。**不照抄的理由是本仓库已经吃过一次亏**：`sdk/go/conformance/conformance.go`
//! 里有一份中台措辞的手抄副本，副本与原件漂移之后没人发现。
//!
//! # 它与真中台**不**同的地方
//!
//! - **不落库**：插件、版本、实例全在内存里，进程退出即清空。所以它只适合本地验证，
//!   不能当测试环境用。
//! - **状态存在内存里**：HubState 的四件（`KvGet` / `KvPut` / `KvDelete` / `KvScan`）
//!   是实现了的——键名规则、值上限、鉴权语义都调 `hub-core` 里与真中台**同一份**判据，
//!   差的是存储介质（内存 vs Redis + PG），所以重启即清空。
//!   但 **`Publish` 没实现**：它依赖中台的 flow 引擎与防环限流，替身里没有对应物，
//!   回 `Unimplemented` 而不是假装受理。
//! - **不做心跳超时摘除**：实例只有在主动 `Unregister` 时才消失。真中台有巡检任务，
//!   这里没有——本地开发时「插件被摘掉」不是要验的东西，而多一个后台任务就多一处
//!   与真中台的差异需要维护。

/// mock 的名字。`/health` 里报的就是它，也是区分「我打的是谁」的那一个字段。
///
/// **为什么要有这个东西**：mock 与真中台刻意绑同一组端口，于是「插件注册成功了」
/// 这件事在日志里长得一模一样——而这两者能证明的东西完全不同（mock 不落库、
/// 不做心跳摘除、没有 flow 引擎）。真实发生过一次：另一个会话的验证结论里，
/// 有一部分其实是我这个 mock 给的，双方都是事后才发现。
///
/// 所以定一条：**动插件之前先确认对面是谁**——
/// `curl http://127.0.0.1:8092/health`，看到 `"name":"hub-mock"` 就是在对 mock 验，
/// 看到 `"name":"plugin-hub"` 才是真中台。
pub const MOCK_NAME: &str = "hub-mock";

/// mock 下发的状态凭证的前缀。
///
/// 真中台的凭证是一串随机值、只有数据库能反查；mock 的**故意做成可解析的**——
/// 它要能一眼看出「这是 mock 发的」，也要能反查出是哪个实例。
/// 见 [`MockRegistry::plugin_of_token`]。
///
/// 写成这个形状而不是随便一个串：真发生过「拿 mock 的凭证去连真中台」这类困惑，
/// 那时错误信息里能看出凭证来自哪儿，比一个随机串强。
const STATE_TOKEN_PREFIX: &str = "mock-state-token-";

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_core::state::{MAX_SCAN_LIMIT, MAX_VALUE_BYTES, STATE_TOKEN_METADATA, valid_segment};

use hub_contract::{ContractIndex, Severity, check_compatibility, has_breaking, summarize};
use hub_proto::v1::hub_state_server::HubState;
use hub_proto::v1::plugin_registry_server::PluginRegistry;
use hub_proto::v1::{
    HeartbeatRequest, HeartbeatResponse, KvDeleteRequest, KvDeleteResponse, KvEntry, KvGetRequest,
    KvGetResponse, KvKey, KvPutRequest, KvPutResponse, KvScanRequest, KvScanResponse, PublishRequest,
    PublishResponse, RegisterRequest, RegisterResponse, RejectCode, Rejection, UnregisterRequest,
    UnregisterResponse,
};
use hub_registry::{PluginProbe, RegistryConfig, reject};
use prost::Message as _;
use serde::Serialize;
use tonic::{Request, Response, Status};

/// 内存里的注册表。
pub struct MockRegistry {
    probe: Arc<dyn PluginProbe>,
    cfg: RegistryConfig,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    plugins: BTreeMap<String, PluginState>,
    /// 全局的 instance_id → 实例。真中台里这张表是数据库的唯一键，跨插件唯一
    instances: BTreeMap<String, InstanceState>,
    /// 被拒绝的历史。给 `/plugins` 用——CI 与开发者都要能机器可读地看到「为什么没进来」
    rejections: Vec<RejectionRecord>,
    /// 外置状态（HubState）。键是 **(插件名, namespace, key)** 三元组。
    ///
    /// **用三元组而不是拼一个字符串**：真中台拼的是 `hub:state:{插件}:{ns}:{key}`，
    /// 那样拼的**前提**是键名里不可能出现分隔符——由 `valid_segment` 的白名单保证。
    /// mock 用元组就不必依赖那个前提：即便规则将来放宽到允许 `:`，隔离也不会破。
    state: BTreeMap<(String, String, String), StateValue>,
}

/// 一个状态条目。
struct StateValue {
    value: Vec<u8>,
    /// `None` = 不过期。到期判断在**读取时**做（惰性，见 [`MockRegistry::state_get`]）——
    /// 为此起一个清理任务不划算，而真中台那边是 Redis 自己管的。
    expires_at: Option<Instant>,
}

/// 这个条目过期了吗。`None` 表示永不过期。
fn expired(v: &StateValue) -> bool {
    v.expires_at.is_some_and(|at| Instant::now() >= at)
}

#[derive(Default)]
struct PluginState {
    versions: BTreeMap<String, VersionState>,
    /// 最近一次登记的版本号。
    ///
    /// 真中台的 `latest_version` 是 `ORDER BY created_at DESC, id DESC LIMIT 1`，
    /// 也就是「最近插入的那个」而不是语义化版本最大的那个。这里跟着它，
    /// 否则「新版本与哪个基线比」会与真中台不一致，mock 放行的变更可能在真中台被拒。
    latest: Option<String>,
}

struct VersionState {
    /// manifest 的编码结果。同版本号的 manifest 必须逐字节一致
    manifest: Vec<u8>,
    index: ContractIndex,
}

struct InstanceState {
    plugin: String,
    version: String,
    advertise_addr: String,
    last_heartbeat: Instant,
}

#[derive(Clone, Serialize)]
pub struct RejectionRecord {
    pub plugin: String,
    pub version: String,
    pub instance_id: String,
    pub advertise_addr: String,
    pub code: String,
    pub message: String,
    pub detail: String,
}

/// `/plugins` 的一行。
#[derive(Clone, Serialize)]
pub struct PluginView {
    pub name: String,
    pub versions: Vec<String>,
    pub latest: Option<String>,
    pub instances: Vec<InstanceView>,
}

#[derive(Clone, Serialize)]
pub struct InstanceView {
    pub instance_id: String,
    pub plugin: String,
    pub version: String,
    pub advertise_addr: String,
    pub last_heartbeat_secs_ago: u64,
}

impl MockRegistry {
    pub fn new(probe: Arc<dyn PluginProbe>, cfg: RegistryConfig) -> Self {
        Self {
            probe,
            cfg,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn config(&self) -> &RegistryConfig {
        &self.cfg
    }

    /// 处理一次注册。**永不返回错误**：所有失败都以结构化 rejections 回给插件方，
    /// 与真中台一致——插件侧只需要一套处理逻辑。
    pub async fn register(&self, req: &RegisterRequest) -> RegisterResponse {
        let response = self.try_register(req).await;
        if !response.accepted {
            self.record(req, &response.rejections);
        }
        response
    }

    async fn try_register(&self, req: &RegisterRequest) -> RegisterResponse {
        // 1~3) manifest / descriptor / manifest 自洽。与真中台同一段代码
        let (manifest, index) = match reject::preflight(req) {
            reject::Preflight::Reject(rejections) => return rejected(rejections),
            reject::Preflight::Ok { manifest, index } => (manifest, index),
        };

        // 4) 可达性探测。**在锁外做**：它要真的去拨插件，可能几百毫秒，
        // 持锁await会把整个注册面串行化
        let outcome = self.probe.health(&req.advertise_addr).await;
        if let Some(rejection) = reject::probe_rejection(&req.advertise_addr, &outcome) {
            return rejected(vec![rejection]);
        }

        // 5) 实例标识的格式（空、超长）。不需要查表，放在取锁前
        if let Some(rejection) = reject::instance_id_format_rejection(&req.instance_id) {
            return rejected(vec![rejection]);
        }

        let mut inner = self.inner.lock().expect("mock 的锁不该被毒化");

        // 6) 契约兼容：同版本必须逐字节一致；新版本与最近一次登记的基线比破坏性变更
        let mut warnings = Vec::new();
        if let Some(plugin) = inner.plugins.get(&manifest.name) {
            if let Some(existing) = plugin.versions.get(&manifest.version) {
                if existing.index != index || existing.manifest != manifest.encode_to_vec() {
                    return rejected(vec![Rejection {
                        code: RejectCode::VersionConflict as i32,
                        message: format!(
                            "版本 {} 已存在且契约或 manifest 不一致",
                            manifest.version
                        ),
                        detail: "同一版本号不可改变契约；请升版本号后重新注册".to_string(),
                    }]);
                }
            } else if let Some(baseline) =
                plugin.latest.as_ref().and_then(|v| plugin.versions.get(v))
            {
                let changes = check_compatibility(&baseline.index, &index);
                if has_breaking(&changes) {
                    return rejected(vec![Rejection {
                        code: RejectCode::BreakingChange as i32,
                        message: format!(
                            "相对版本 {} 存在破坏性契约变更",
                            plugin.latest.as_deref().unwrap_or("（未知）")
                        ),
                        detail: format!(
                            "{}；基线版本里已有的字段不能删、改类型或改编号，只能新增——\
                             请把这些字段按原编号原类型改回再注册；若确实要重新设计契约，\
                             需由中台侧先清掉旧版本基线（hubctl remove-version）",
                            summarize(&changes).unwrap_or_default()
                        ),
                    }]);
                }
                warnings.extend(
                    changes
                        .iter()
                        .filter(|c| c.severity == Severity::Warning)
                        .map(|c| c.detail.clone()),
                );
            }
        }

        // 7) 实例标识的属主。跨插件复用会让两边每次重注册都互相顶掉，详见那支函数的说明
        let owner = inner
            .instances
            .get(&req.instance_id)
            .map(|inst| inst.plugin.clone());
        if let Some(rejection) =
            reject::instance_id_owner_rejection(&req.instance_id, &manifest.name, owner.as_deref())
        {
            return rejected(vec![rejection]);
        }

        // 8) 记下
        let entry = inner.plugins.entry(manifest.name.clone()).or_default();
        let is_new_version = !entry.versions.contains_key(&manifest.version);
        entry.versions.insert(
            manifest.version.clone(),
            VersionState {
                manifest: manifest.encode_to_vec(),
                index,
            },
        );
        if is_new_version {
            entry.latest = Some(manifest.version.clone());
        }

        // 凭证的格式是 mock 自己定的（真中台的是随机串），关键是它**能反查出插件名**：
        // 状态面的每个请求都要带它，mock 靠它知道「这是谁在读写」，正如真中台靠数据库
        // 里那份凭证反查一样。
        //
        // **不额外存一份**：instance_id 已经唯一确定了一个实例，而实例上记着插件名。
        // 存一份平行的 token 表只会在两个地方之间制造同步问题，而它们本可以只有一个来源。
        let state_token = format!("{STATE_TOKEN_PREFIX}{}", req.instance_id);
        inner.instances.insert(
            req.instance_id.clone(),
            InstanceState {
                plugin: manifest.name.clone(),
                version: manifest.version.clone(),
                advertise_addr: req.advertise_addr.clone(),
                last_heartbeat: Instant::now(),
            },
        );

        RegisterResponse {
            accepted: true,
            instance_id: req.instance_id.clone(),
            heartbeat_interval_seconds: self.cfg.heartbeat_interval_seconds,
            rejections: Vec::new(),
            warnings,
            state_token,
        }
    }

    /// 心跳。与真中台一致：认不出来就要求重新注册，而不是回一个 gRPC 错误。
    pub fn heartbeat(&self, instance_id: &str) -> HeartbeatResponse {
        let mut inner = self.inner.lock().expect("mock 的锁不该被毒化");

        match inner.instances.get_mut(instance_id) {
            Some(inst) => {
                inst.last_heartbeat = Instant::now();
                HeartbeatResponse {
                    accepted: true,
                    heartbeat_interval_seconds: self.cfg.heartbeat_interval_seconds,
                    reregister_required: false,
                }
            }
            None => HeartbeatResponse {
                accepted: false,
                heartbeat_interval_seconds: self.cfg.heartbeat_interval_seconds,
                reregister_required: true,
            },
        }
    }

    /// 插件主动注销（优雅退出）。
    pub fn unregister(&self, instance_id: &str) -> UnregisterResponse {
        let mut inner = self.inner.lock().expect("mock 的锁不该被毒化");
        inner.instances.remove(instance_id);
        UnregisterResponse {}
    }

    /// 当前注册在册的插件。
    pub fn plugins(&self) -> Vec<PluginView> {
        let inner = self.inner.lock().expect("mock 的锁不该被毒化");

        inner
            .plugins
            .iter()
            .map(|(name, state)| PluginView {
                name: name.clone(),
                versions: state.versions.keys().cloned().collect(),
                latest: state.latest.clone(),
                instances: inner
                    .instances
                    .iter()
                    .filter(|(_, inst)| &inst.plugin == name)
                    .map(|(id, inst)| InstanceView {
                        instance_id: id.clone(),
                        plugin: inst.plugin.clone(),
                        version: inst.version.clone(),
                        advertise_addr: inst.advertise_addr.clone(),
                        last_heartbeat_secs_ago: inst.last_heartbeat.elapsed().as_secs(),
                    })
                    .collect(),
            })
            .collect()
    }

    /// 被拒绝过的注册，最近的在前。
    pub fn rejections(&self) -> Vec<RejectionRecord> {
        let inner = self.inner.lock().expect("mock 的锁不该被毒化");
        inner.rejections.iter().rev().cloned().collect()
    }

    // ------------------------------------------------------------ 外置状态

    /// 按凭证反查插件名。查不到返回 `None`——调用方应回 `UNAUTHENTICATED`。
    ///
    /// 真中台是拿凭证查数据库，mock 是从凭证里剥出 instance_id 再查实例表。**语义一致**：
    /// 身份来自**凭证**，不是请求里插件自报的任何字段——插件面本就不鉴权，自报的插件名
    /// 不构成身份。这一条是状态面的隔离基础，mock 必须跟着成立。
    pub fn plugin_of_token(&self, token: &str) -> Option<String> {
        let instance_id = token.strip_prefix(STATE_TOKEN_PREFIX)?;
        let inner = self.inner.lock().expect("mock 的锁不该被毒化");
        inner
            .instances
            .get(instance_id)
            .map(|inst| inst.plugin.clone())
    }

    /// 读一个键。返回 `(found, value)`；`found=false` 表示不存在**或已过期**——
    /// 那两者对调用方是同一件事。
    pub fn state_get(&self, plugin: &str, ns: &str, key: &str) -> (bool, Vec<u8>) {
        let mut inner = self.inner.lock().expect("mock 的锁不该被毒化");
        let k = (plugin.to_string(), ns.to_string(), key.to_string());
        match inner.state.get(&k) {
            Some(v) if !expired(v) => (true, v.value.clone()),
            Some(_) => {
                // 惰性清理：读到过期的就顺手删掉。为此起后台任务不划算，
                // 而真中台那边过期是 Redis 自己管的。
                inner.state.remove(&k);
                (false, Vec::new())
            }
            None => (false, Vec::new()),
        }
    }

    /// 写一个键。`ttl_seconds <= 0` 表示不过期（与契约里「0 表示不过期」一致）。
    ///
    /// 返回 `Err` 时是**调用方参数不对**（值超限），不是内部故障——调用方应转成
    /// `INVALID_ARGUMENT`。上限取自 `hub-core`，与真中台是同一个常量。
    pub fn state_put(
        &self,
        plugin: &str,
        ns: &str,
        key: &str,
        value: Vec<u8>,
        ttl_seconds: i64,
    ) -> Result<(), String> {
        if value.len() > MAX_VALUE_BYTES {
            return Err(format!(
                "值 {} 字节，超过上限 {MAX_VALUE_BYTES} 字节",
                value.len()
            ));
        }
        let mut inner = self.inner.lock().expect("mock 的锁不该被毒化");
        let expires_at =
            (ttl_seconds > 0).then(|| Instant::now() + Duration::from_secs(ttl_seconds as u64));
        inner.state.insert(
            (plugin.to_string(), ns.to_string(), key.to_string()),
            StateValue { value, expires_at },
        );
        Ok(())
    }

    /// 删一个键。返回它**之前是否存在**（契约里是 `deleted` 布尔，语义相同）。
    ///
    /// 已过期的算不存在：删一个本来就等于不存在的东西，报 `true` 会误导调用方
    /// 以为「确实清理掉了什么」。
    pub fn state_delete(&self, plugin: &str, ns: &str, key: &str) -> bool {
        let mut inner = self.inner.lock().expect("mock 的锁不该被毒化");
        inner
            .state
            .remove(&(plugin.to_string(), ns.to_string(), key.to_string()))
            .is_some_and(|v| !expired(&v))
    }

    /// 扫一个命名空间。`prefix` 为空表示扫整个命名空间。
    ///
    /// **超限由调用方先拒绝**，不是在这里截断：真中台对 `limit > MAX_SCAN_LIMIT` 的
    /// 请求是**拒绝**（见 `crates/hub-grpc/src/state.rs`），mock 跟着拒绝才不会出现
    /// 「本地放行、线上被拒」——那正是这类替身最该避免的失败。
    pub fn state_scan(
        &self,
        plugin: &str,
        ns: &str,
        prefix: &str,
        limit: u32,
    ) -> Vec<(String, Vec<u8>)> {
        let mut inner = self.inner.lock().expect("mock 的锁不该被毒化");
        let (plugin, ns) = (plugin.to_string(), ns.to_string());

        // 先清掉这个命名空间里过期的：它们不该占 limit 的名额，否则「扫出 50 条里有
        // 30 条早过期了」会让调用方以为数据还在
        let stale: Vec<_> = inner
            .state
            .iter()
            .filter(|((p, n, _), v)| p == &plugin && n == &ns && expired(v))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            inner.state.remove(&k);
        }

        inner
            .state
            .iter()
            .filter(|((p, n, k), _)| {
                p == &plugin && n == &ns && (prefix.is_empty() || k.starts_with(prefix))
            })
            .take(limit as usize)
            .map(|((_, _, k), v)| (k.clone(), v.value.clone()))
            .collect()
    }

    fn record(&self, req: &RegisterRequest, rejections: &[Rejection]) {
        let mut inner = self.inner.lock().expect("mock 的锁不该被毒化");
        for r in rejections {
            inner.rejections.push(RejectionRecord {
                plugin: req.plugin_name.clone(),
                version: req.version.clone(),
                instance_id: req.instance_id.clone(),
                advertise_addr: req.advertise_addr.clone(),
                code: RejectCode::try_from(r.code)
                    .map(|c| c.as_str_name().to_string())
                    .unwrap_or_else(|_| format!("UNKNOWN({})", r.code)),
                message: r.message.clone(),
                detail: r.detail.clone(),
            });
        }
    }
}

fn rejected(rejections: Vec<Rejection>) -> RegisterResponse {
    RegisterResponse {
        accepted: false,
        instance_id: String::new(),
        heartbeat_interval_seconds: 0,
        rejections,
        warnings: Vec::new(),
        state_token: String::new(),
    }
}

// ---------------------------------------------------------------- gRPC 服务层

/// mock 的 gRPC 服务：注册面 + 状态面。
///
/// **两个面共用同一个 [`MockRegistry`]**——注册留下的实例表就是状态面反查插件名用的
/// 那张表，拆成两个结构反而要把它们同步起来。
///
/// 它住在 lib 而不是 `main.rs`：这一层有三个容易错、而开发者一定会撞上的地方——
/// metadata 取凭证、`UNAUTHENTICATED` 错误码、参数校验。放这里才测得着。
#[derive(Clone)]
pub struct MockService {
    registry: Arc<MockRegistry>,
}

impl MockService {
    pub fn new(registry: Arc<MockRegistry>) -> Self {
        Self { registry }
    }

    /// 从 metadata 里取凭证、反查出插件名。
    ///
    /// **回 `UNAUTHENTICATED` 而不是别的码**：插件侧对它的处理是「重新注册取新凭证」
    /// （Go 的 `noteDenied` 就是这条），mock 换个码会让那段逻辑在本地压根测不到。
    fn authenticate<T>(&self, request: &Request<T>) -> Result<String, Status> {
        let token = request
            .metadata()
            .get(STATE_TOKEN_METADATA)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();

        self.registry.plugin_of_token(token).ok_or_else(|| {
            Status::unauthenticated(format!(
                "状态凭证无效或实例已注销（metadata `{STATE_TOKEN_METADATA}`）。\
                 插件应在收到本错误后重新注册以取得新凭证"
            ))
        })
    }
}

/// 取出 `KvKey` 并校验两个分段。
fn key_of(key: Option<KvKey>) -> Result<KvKey, Status> {
    let key = key.ok_or_else(|| Status::invalid_argument("缺少 key"))?;
    check_segments(&key.namespace, &key.key)?;
    Ok(key)
}

/// 键名白名单校验。**判据来自 `hub-core`，与真中台同一个函数**——这是 mock 存在的
/// 意义所在：本地放行而线上被拒，是最坏的一类不一致，两边还都不会报错。
fn check_segments(ns: &str, key: &str) -> Result<(), Status> {
    if !valid_segment(ns) {
        return Err(Status::invalid_argument(format!(
            "namespace 只允许 [A-Za-z0-9_.-]、非空、最长 200 字节，收到 {ns:?}"
        )));
    }
    if !valid_segment(key) {
        return Err(Status::invalid_argument(format!(
            "key 只允许 [A-Za-z0-9_.-]、非空、最长 200 字节，收到 {key:?}"
        )));
    }
    Ok(())
}

#[tonic::async_trait]
impl PluginRegistry for MockService {
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let req = request.into_inner();
        let response = self.registry.register(&req).await;

        // 与真中台一致：拒绝走结构化 rejections，**不用 gRPC 错误码**。
        // 插件侧只需要一套处理逻辑，且拒绝原因要能原样展示给人看。
        if response.accepted {
            tracing::info!(
                plugin = %req.plugin_name,
                version = %req.version,
                instance = %req.instance_id,
                addr = %req.advertise_addr,
                "已接受注册"
            );
        } else {
            for rejection in &response.rejections {
                tracing::warn!(
                    plugin = %req.plugin_name,
                    code = ?rejection.code,
                    message = %rejection.message,
                    detail = %rejection.detail,
                    "拒绝了注册"
                );
            }
        }

        Ok(Response::new(response))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        Ok(Response::new(
            self.registry.heartbeat(&request.into_inner().instance_id),
        ))
    }

    async fn unregister(
        &self,
        request: Request<UnregisterRequest>,
    ) -> Result<Response<UnregisterResponse>, Status> {
        let req = request.into_inner();
        tracing::info!(instance = %req.instance_id, reason = %req.reason, "插件主动注销");
        Ok(Response::new(self.registry.unregister(&req.instance_id)))
    }
}

/// 外置状态面。
///
/// **它与真中台的差异只有一处：存储介质**（内存 vs Redis + PG）。键名规则、上限、
/// 鉴权语义、错误码全部同源，插件在本地验过的行为到线上应当一样。
#[tonic::async_trait]
impl HubState for MockService {
    async fn kv_get(
        &self,
        request: Request<KvGetRequest>,
    ) -> Result<Response<KvGetResponse>, Status> {
        let plugin = self.authenticate(&request)?;
        let key = key_of(request.into_inner().key)?;
        let (found, value) = self.registry.state_get(&plugin, &key.namespace, &key.key);
        Ok(Response::new(KvGetResponse { found, value }))
    }

    async fn kv_put(
        &self,
        request: Request<KvPutRequest>,
    ) -> Result<Response<KvPutResponse>, Status> {
        let plugin = self.authenticate(&request)?;
        let inner = request.into_inner();
        let key = key_of(inner.key)?;
        self.registry
            .state_put(&plugin, &key.namespace, &key.key, inner.value, inner.ttl_seconds)
            .map_err(Status::invalid_argument)?;
        Ok(Response::new(KvPutResponse {}))
    }

    async fn kv_delete(
        &self,
        request: Request<KvDeleteRequest>,
    ) -> Result<Response<KvDeleteResponse>, Status> {
        let plugin = self.authenticate(&request)?;
        let key = key_of(request.into_inner().key)?;
        let deleted = self.registry.state_delete(&plugin, &key.namespace, &key.key);
        Ok(Response::new(KvDeleteResponse { deleted }))
    }

    async fn kv_scan(
        &self,
        request: Request<KvScanRequest>,
    ) -> Result<Response<KvScanResponse>, Status> {
        let plugin = self.authenticate(&request)?;
        let inner = request.into_inner();

        if !valid_segment(&inner.namespace) {
            return Err(Status::invalid_argument(format!(
                "namespace 只允许 [A-Za-z0-9_.-]、非空、最长 200 字节，收到 {:?}",
                inner.namespace
            )));
        }
        // prefix 允许为空（表示扫整个命名空间），非空时字符集同 namespace
        if !inner.prefix.is_empty() && !valid_segment(&inner.prefix) {
            return Err(Status::invalid_argument(format!(
                "prefix 只允许 [A-Za-z0-9_.-]、最长 200 字节，收到 {:?}",
                inner.prefix
            )));
        }
        // **超限是拒绝而不是截断**，与真中台一致。截断会让调用方以为「就这么多」，
        // 而真中台会回一个错——那种差异正是 mock 最该避免的
        if inner.limit > MAX_SCAN_LIMIT {
            return Err(Status::invalid_argument(format!(
                "limit {} 超过上限 {MAX_SCAN_LIMIT}",
                inner.limit
            )));
        }

        let entries = self
            .registry
            .state_scan(&plugin, &inner.namespace, &inner.prefix, inner.limit)
            .into_iter()
            .map(|(key, value)| KvEntry { key, value })
            .collect();
        Ok(Response::new(KvScanResponse { entries }))
    }

    /// mock **不实现**它：`Publish` 依赖中台的 flow 引擎与防环、限流，那些在替身里
    /// 没有对应物。回 `Unimplemented` 而不是假装受理——假受理会让开发者在本地以为
    /// 「下游被触发了」，而线上什么都不会发生。
    async fn publish(
        &self,
        _request: Request<PublishRequest>,
    ) -> Result<Response<PublishResponse>, Status> {
        Err(Status::unimplemented(
            "mock 中台没有 flow 引擎，Publish 只能在真中台上验",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_proto::v1::PluginManifest;
    use hub_registry::probe::{AlwaysHealthy, ProbeOutcome};

    /// 一个总是"不可达"的探针，用来验探测那一关。
    struct AlwaysUnreachable;

    #[async_trait::async_trait]
    impl PluginProbe for AlwaysUnreachable {
        async fn health(&self, _addr: &str) -> ProbeOutcome {
            ProbeOutcome::Unreachable {
                message: "dial tcp: connection refused".to_string(),
            }
        }
    }

    fn registry() -> MockRegistry {
        MockRegistry::new(Arc::new(AlwaysHealthy), RegistryConfig::default())
    }

    fn request(name: &str, version: &str, instance: &str) -> RegisterRequest {
        RegisterRequest {
            plugin_name: name.to_string(),
            version: version.to_string(),
            instance_id: instance.to_string(),
            advertise_addr: "http://127.0.0.1:9000".to_string(),
            manifest: Some(PluginManifest {
                name: name.to_string(),
                version: version.to_string(),
                // 至少声明一条契约，否则本地 L1 会红（中台倒是不管）
                consumes: vec![hub_proto::v1::MessageContract {
                    fq_name: "google.protobuf.Struct".to_string(),
                    description: String::new(),
                }],
                ..Default::default()
            }),
            descriptor_set: Vec::new(),
        }
    }

    async fn register(r: &MockRegistry, req: &RegisterRequest) -> RegisterResponse {
        r.register(req).await
    }

    #[tokio::test]
    async fn 合法插件注册成功() {
        let r = registry();
        let resp = register(&r, &request("order-reader", "0.1.0", "host-1")).await;
        assert!(resp.accepted, "应注册成功：{:?}", resp.rejections);
        assert_eq!(resp.instance_id, "host-1");
        assert!(!resp.state_token.is_empty(), "应下发状态凭证");
        assert_eq!(r.plugins().len(), 1);
    }

    #[tokio::test]
    async fn 探测不通过时拒绝且原因指向地址() {
        let r = MockRegistry::new(Arc::new(AlwaysUnreachable), RegistryConfig::default());
        let resp = register(&r, &request("order-reader", "0.1.0", "host-1")).await;

        assert!(!resp.accepted);
        let rej = &resp.rejections[0];
        assert_eq!(rej.code, RejectCode::Unreachable as i32);
        // 出路要写在消息里——这是那一整套措辞工作的全部意义
        assert!(
            rej.detail.contains("HUB_ADVERTISE_ADDR"),
            "应告诉插件方去查哪个变量，实际：{}",
            rej.detail
        );
    }

    #[tokio::test]
    async fn 缺少manifest被拒() {
        let r = registry();
        let mut req = request("p", "1.0.0", "i");
        req.manifest = None;

        let resp = register(&r, &req).await;
        assert!(!resp.accepted);
        assert_eq!(resp.rejections[0].code, RejectCode::ManifestInvalid as i32);
    }

    #[tokio::test]
    async fn 非法插件名被拒() {
        let r = registry();
        let resp = register(&r, &request("中文名", "1.0.0", "i")).await;
        assert!(!resp.accepted);
        assert_eq!(resp.rejections[0].code, RejectCode::ManifestInvalid as i32);
    }

    #[tokio::test]
    async fn 空实例标识被拒() {
        let r = registry();
        let resp = register(&r, &request("p", "1.0.0", "")).await;
        assert!(!resp.accepted);
        assert_eq!(resp.rejections[0].code, RejectCode::InstanceConflict as i32);
    }

    #[tokio::test]
    async fn 同一插件同一版本重复注册要逐字一致() {
        let r = registry();
        assert!(register(&r, &request("p", "1.0.0", "i1")).await.accepted);

        // 同版本、同样内容：放行（这就是「重启」）
        assert!(
            register(&r, &request("p", "1.0.0", "i2")).await.accepted,
            "同版本同内容的重注册应放行"
        );

        // 同版本、改了 manifest：拒绝
        let mut changed = request("p", "1.0.0", "i3");
        changed.manifest.as_mut().unwrap().description = "改过了".to_string();
        let resp = register(&r, &changed).await;
        assert!(!resp.accepted);
        assert_eq!(resp.rejections[0].code, RejectCode::VersionConflict as i32);
    }

    #[tokio::test]
    async fn 换版本号可以改契约() {
        let r = registry();
        assert!(register(&r, &request("p", "1.0.0", "i1")).await.accepted);

        let mut next = request("p", "1.1.0", "i2");
        next.manifest.as_mut().unwrap().description = "新版本".to_string();

        let resp = register(&r, &next).await;
        assert!(
            resp.accepted,
            "换版本号就是为了能改契约，不该被拦：{:?}",
            resp.rejections
        );
    }

    #[tokio::test]
    async fn 跨插件复用实例标识被拒() {
        let r = registry();
        assert!(
            register(&r, &request("a", "1.0.0", "same-id"))
                .await
                .accepted
        );

        // 另一个插件拿同一个 instance_id：这正是真中台要拦的那件事
        let resp = register(&r, &request("b", "1.0.0", "same-id")).await;
        assert!(!resp.accepted, "跨插件复用 instance_id 必须被拒");
        assert_eq!(resp.rejections[0].code, RejectCode::InstanceConflict as i32);
        assert!(resp.rejections[0].detail.contains("HUB_INSTANCE_ID"));
    }

    #[tokio::test]
    async fn 同一插件换版本可以复用实例标识() {
        let r = registry();
        assert!(
            register(&r, &request("a", "1.0.0", "same-id"))
                .await
                .accepted
        );

        // 同一插件升级复用 instance_id 是正常的（实例行随 upsert 迁移）
        assert!(
            register(&r, &request("a", "2.0.0", "same-id"))
                .await
                .accepted,
            "同一插件换版本复用实例标识应放行"
        );
    }

    #[tokio::test]
    async fn 心跳认得注册过的实例() {
        let r = registry();
        assert!(register(&r, &request("p", "1.0.0", "i")).await.accepted);

        let hb = r.heartbeat("i");
        assert!(hb.accepted);
        assert!(!hb.reregister_required);
    }

    #[tokio::test]
    async fn 心跳认不出实例时要求重新注册() {
        let r = registry();
        let hb = r.heartbeat("从没注册过");
        assert!(!hb.accepted);
        assert!(
            hb.reregister_required,
            "认不出来要回 reregister_required 而不是错误码——与真中台一致"
        );
    }

    #[tokio::test]
    async fn 注销后实例消失且心跳要求重新注册() {
        let r = registry();
        assert!(register(&r, &request("p", "1.0.0", "i")).await.accepted);
        r.unregister("i");

        assert!(r.plugins()[0].instances.is_empty(), "注销后不该还有实例");
        assert!(r.heartbeat("i").reregister_required);
    }

    #[tokio::test]
    async fn 被拒的历史可查且带得走原因() {
        let r = registry();
        let _ = register(&r, &request("中文名", "1.0.0", "i")).await;

        let log = r.rejections();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].code, "REJECT_CODE_MANIFEST_INVALID");
        assert!(!log[0].detail.is_empty(), "detail 要给下一步怎么做");
    }

    #[tokio::test]
    async fn 拒绝历史最近的在前() {
        let r = registry();
        let _ = register(&r, &request("中文名", "1.0.0", "i1")).await;
        let _ = register(&r, &request("p", "1.0.0", "")).await;

        let log = r.rejections();
        assert_eq!(log.len(), 2);
        assert_eq!(
            log[0].code, "REJECT_CODE_INSTANCE_CONFLICT",
            "最近一条应排在最前"
        );
    }

    #[tokio::test]
    async fn 状态凭证一眼能看出是_mock_的() {
        // 拿它去调真中台的状态面会失败，写成能认出来的样子免得人困惑
        let r = registry();
        let resp = register(&r, &request("p", "1.0.0", "i")).await;
        assert!(resp.state_token.starts_with("mock-"));
    }

    #[test]
    fn 名字与真中台不同() {
        // `/health` 的 name 是「我打的是谁」的唯一判据。撞名等于把这条判据废掉，
        // 于是「对 mock 验过了」会被当成「对真中台验过了」——真实发生过一次。
        assert_ne!(
            MOCK_NAME,
            hub_core::SERVICE_NAME,
            "mock 的名字不能与真中台（{}）相同，否则分不出打的是谁",
            hub_core::SERVICE_NAME
        );
    }

    // ------------------------------------------------------------ 外置状态

    /// 注册一个插件并返回它拿到的凭证。
    async fn 注册并拿凭证(r: &MockRegistry, name: &str, instance: &str) -> String {
        let resp = register(r, &request(name, "1.0.0", instance)).await;
        assert!(resp.accepted, "{name} 应当注册成功：{:?}", resp.rejections);
        resp.state_token
    }

    #[tokio::test]
    async fn 凭证能反查出插件名() {
        let r = registry();
        let token = 注册并拿凭证(&r, "order-reader", "inst-1").await;
        assert_eq!(r.plugin_of_token(&token).as_deref(), Some("order-reader"));
    }

    #[tokio::test]
    async fn 凭证认不出时反查是空() {
        // 三种都要挡住：随便一个串、真中台那种随机串、以及**形状对但实例不存在**
        let r = registry();
        for bad in [
            "",
            "garbage",
            "glpat-xxxxxxxxxxxxxxxxxxxx",
            "mock-state-token-不存在的实例",
        ] {
            assert!(
                r.plugin_of_token(bad).is_none(),
                "{bad:?} 不该反查出插件名"
            );
        }
    }

    #[tokio::test]
    async fn 实例注销后凭证失效() {
        // 这条是「身份来自凭证、且凭证随实例存亡」的直接后果：插件注销后，
        // 它手里的旧凭证不该还能读写状态
        let r = registry();
        let token = 注册并拿凭证(&r, "p", "i").await;
        assert!(r.plugin_of_token(&token).is_some());

        r.unregister("i");
        assert!(
            r.plugin_of_token(&token).is_none(),
            "实例注销后旧凭证应失效"
        );
    }

    #[tokio::test]
    async fn 状态读写删的往返() {
        let r = registry();
        let token = 注册并拿凭证(&r, "p", "i").await;
        let plugin = r.plugin_of_token(&token).unwrap();

        // 没写过 → 读不到
        assert_eq!(r.state_get(&plugin, "ns", "k"), (false, Vec::new()));

        r.state_put(&plugin, "ns", "k", b"v1".to_vec(), 0).unwrap();
        assert_eq!(r.state_get(&plugin, "ns", "k"), (true, b"v1".to_vec()));

        // 覆盖写
        r.state_put(&plugin, "ns", "k", b"v2".to_vec(), 0).unwrap();
        assert_eq!(r.state_get(&plugin, "ns", "k"), (true, b"v2".to_vec()));

        assert!(r.state_delete(&plugin, "ns", "k"));
        assert_eq!(r.state_get(&plugin, "ns", "k"), (false, Vec::new()));
        // 删一个本来就不存在的，要回 false——回 true 会让调用方以为清理掉了什么
        assert!(!r.state_delete(&plugin, "ns", "k"));
    }

    #[tokio::test]
    async fn 过期的键读不到也扫不出() {
        let r = registry();
        let token = 注册并拿凭证(&r, "p", "i").await;
        let plugin = r.plugin_of_token(&token).unwrap();

        r.state_put(&plugin, "ns", "k", b"v".to_vec(), -1).unwrap(); // <=0 视为不过期
        assert!(r.state_get(&plugin, "ns", "k").0, "ttl<=0 应当不过期");

        // 1 秒的 ttl，等它过去
        r.state_put(&plugin, "ns", "ttl", b"v".to_vec(), 1).unwrap();
        assert!(r.state_get(&plugin, "ns", "ttl").0);
        std::thread::sleep(Duration::from_millis(1100));
        assert!(!r.state_get(&plugin, "ns", "ttl").0, "过期后应读不到");

        // 扫描也不该再看到它。**不能断言结果是空的**——同一个命名空间里还有那个
        // 不过期的 `k`，它本来就该被扫出来
        let scanned = r.state_scan(&plugin, "ns", "", 100);
        assert!(
            !scanned.iter().any(|(k, _)| k == "ttl"),
            "过期的条目不该出现在扫描结果里，实际 {scanned:?}"
        );
        assert!(
            scanned.iter().any(|(k, _)| k == "k"),
            "不过期的条目应当还在，实际 {scanned:?}"
        );
    }

    #[tokio::test]
    async fn 扫描按前缀过滤且受上限约束() {
        let r = registry();
        let token = 注册并拿凭证(&r, "p", "i").await;
        let plugin = r.plugin_of_token(&token).unwrap();

        for k in ["user:1", "user:2", "sess:1"] {
            r.state_put(&plugin, "ns", k, k.as_bytes().to_vec(), 0)
                .unwrap();
        }
        // 另一个命名空间的不该混进来
        r.state_put(&plugin, "other", "user:9", b"x".to_vec(), 0)
            .unwrap();

        let all = r.state_scan(&plugin, "ns", "", 100);
        assert_eq!(all.len(), 3, "空 prefix 应扫整个命名空间，实际 {all:?}");

        let users = r.state_scan(&plugin, "ns", "user:", 100);
        assert_eq!(users.len(), 2, "前缀过滤应当生效，实际 {users:?}");

        let capped = r.state_scan(&plugin, "ns", "", 2);
        assert_eq!(capped.len(), 2, "limit 应当生效");
    }

    #[tokio::test]
    async fn 值超过上限时被拒() {
        let r = registry();
        let token = 注册并拿凭证(&r, "p", "i").await;
        let plugin = r.plugin_of_token(&token).unwrap();

        // 上限正好放行
        r.state_put(&plugin, "ns", "k", vec![0u8; MAX_VALUE_BYTES], 0)
            .expect("正好等于上限应当放行");

        let err = r
            .state_put(&plugin, "ns", "k", vec![0u8; MAX_VALUE_BYTES + 1], 0)
            .expect_err("超过一个字节就该拒");
        assert!(err.contains(&MAX_VALUE_BYTES.to_string()), "错误里应带上限：{err}");
    }

    #[tokio::test]
    async fn 一个插件看不到另一个插件的状态() {
        // **这是状态面唯一的安全性质**：真中台靠「凭证反查出插件名、强制拼进键前缀」
        // 实现隔离，mock 靠键里的插件名。两者机制不同，**可观测的行为必须一样**。
        let r = registry();
        let ta = 注册并拿凭证(&r, "plugin-a", "ia").await;
        let tb = 注册并拿凭证(&r, "plugin-b", "ib").await;
        let (a, b) = (
            r.plugin_of_token(&ta).unwrap(),
            r.plugin_of_token(&tb).unwrap(),
        );

        r.state_put(&a, "ns", "k", b"a-secret".to_vec(), 0).unwrap();

        // 同一个 namespace、同一个 key，B 也看不到 A 的东西
        assert_eq!(
            r.state_get(&b, "ns", "k"),
            (false, Vec::new()),
            "B 不该读到 A 的状态"
        );
        assert!(r.state_scan(&b, "ns", "", 100).is_empty(), "B 不该扫到 A 的条目");

        // 反过来 B 写自己的，也不该覆盖 A 的
        r.state_put(&b, "ns", "k", b"b-secret".to_vec(), 0).unwrap();
        assert_eq!(r.state_get(&a, "ns", "k"), (true, b"a-secret".to_vec()));
        assert_eq!(r.state_get(&b, "ns", "k"), (true, b"b-secret".to_vec()));
    }

    // ------------------------------------------------- gRPC 服务层（含 metadata）

    /// 造一个带凭证的请求。
    fn 带凭证<T>(body: T, token: &str) -> Request<T> {
        let mut req = Request::new(body);
        req.metadata_mut()
            .insert(STATE_TOKEN_METADATA, token.parse().unwrap());
        req
    }

    fn 键(ns: &str, key: &str) -> Option<KvKey> {
        Some(KvKey {
            namespace: ns.to_string(),
            key: key.to_string(),
        })
    }

    #[tokio::test]
    async fn 没有凭证的状态请求回未认证() {
        // **必须是 UNAUTHENTICATED**：插件侧对它的处理是「重新注册取新凭证」，
        // 换个码那段逻辑在本地就测不到
        let r = registry();
        let svc = MockService::new(Arc::new(r));

        let err = svc
            .kv_get(Request::new(KvGetRequest {
                key: 键("ns", "k"),
            }))
            .await
            .expect_err("没带凭证应当被拒");
        assert_eq!(err.code(), tonic::Code::Unauthenticated, "实际 {}", err.message());
    }

    #[tokio::test]
    async fn 凭证无效时回未认证() {
        let r = registry();
        let svc = MockService::new(Arc::new(r));

        for bad in ["garbage", "glpat-xxxxxxxxxxxxxxxxxxxx"] {
            let err = svc
                .kv_get(带凭证(KvGetRequest { key: 键("ns", "k") }, bad))
                .await
                .expect_err("无效凭证应当被拒");
            assert_eq!(err.code(), tonic::Code::Unauthenticated, "凭证 {bad:?}");
        }
    }

    #[tokio::test]
    async fn 走完整链路能读写状态() {
        // 这条覆盖「metadata 里的凭证真的被读到了」——单元测试直接调 registry 是验不到的
        let r = registry();
        let token = 注册并拿凭证(&r, "p", "i").await;
        let svc = MockService::new(Arc::new(r));

        svc.kv_put(带凭证(
            KvPutRequest {
                key: 键("ns", "k"),
                value: b"hello".to_vec(),
                ttl_seconds: 0,
            },
            &token,
        ))
        .await
        .expect("写入应当成功");

        let got = svc
            .kv_get(带凭证(KvGetRequest { key: 键("ns", "k") }, &token))
            .await
            .expect("读取应当成功")
            .into_inner();
        assert!(got.found);
        assert_eq!(got.value, b"hello");

        let deleted = svc
            .kv_delete(带凭证(KvDeleteRequest { key: 键("ns", "k") }, &token))
            .await
            .expect("删除应当成功")
            .into_inner();
        assert!(deleted.deleted);
    }

    #[tokio::test]
    async fn 非法键名与超限扫描都回参数错误() {
        let r = registry();
        let token = 注册并拿凭证(&r, "p", "i").await;
        let svc = MockService::new(Arc::new(r));

        // 键名带通配符——放行它是漏洞（前缀隔离会被一个星号绕过）
        for (ns, key) in [("*", "k"), ("ns", "a:b"), ("", "k"), ("ns", "a b")] {
            let err = svc
                .kv_put(带凭证(
                    KvPutRequest {
                        key: 键(ns, key),
                        value: b"v".to_vec(),
                        ttl_seconds: 0,
                    },
                    &token,
                ))
                .await
                .expect_err("非法键名应当被拒");
            assert_eq!(
                err.code(),
                tonic::Code::InvalidArgument,
                "namespace={ns:?} key={key:?}"
            );
        }

        // limit 超限：**拒绝而不是截断**，与真中台一致
        let err = svc
            .kv_scan(带凭证(
                KvScanRequest {
                    namespace: "ns".to_string(),
                    prefix: String::new(),
                    limit: MAX_SCAN_LIMIT + 1,
                },
                &token,
            ))
            .await
            .expect_err("超限应当被拒");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // 正好等于上限应当放行
        svc.kv_scan(带凭证(
            KvScanRequest {
                namespace: "ns".to_string(),
                prefix: String::new(),
                limit: MAX_SCAN_LIMIT,
            },
            &token,
        ))
        .await
        .expect("正好等于上限应当放行");
    }

    #[tokio::test]
    async fn publish_回未实现() {
        // mock 没有 flow 引擎。**回 Unimplemented 而不是假装受理**——假受理会让开发者
        // 在本地以为「下游被触发了」，而线上什么都不会发生
        let r = registry();
        let token = 注册并拿凭证(&r, "p", "i").await;
        let svc = MockService::new(Arc::new(r));

        let err = svc
            .publish(带凭证(
                PublishRequest {
                    target: "some-flow".to_string(),
                    envelope: None,
                },
                &token,
            ))
            .await
            .expect_err("Publish 不该假装成功");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }
}
