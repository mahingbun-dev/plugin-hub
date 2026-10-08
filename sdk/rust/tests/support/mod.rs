//! 集成测试共用的**伪造中台**。
//!
//! 它实现插件侧真正会调的注册面方法（Register / Heartbeat / Unregister）、
//! **状态面**（HubState：KvGet / KvPut / KvDelete / KvScan / Publish）、
//! 以及**网关面**（PluginGateway：发现三件套 + Invoke），
//! 但把「中台到底收到了什么」全部记下来供断言——这正是这类测试要验的东西：
//! 不是「插件跑起来了」，而是「它有没有按契约做那几件事」。
//!
//! 除此之外它还维护一张最小的**实例表**（`instance_id -> 状态凭证`，见
//! [`MockHub::instances`]）：注销要凭注册时下发的凭证认属主（`UnregisterRequest.state_token`），
//! 而只记请求的话，「中台到底删了谁的行」这件事验不到——插件不带凭证照样「注销成功」。
//!
//! 与 Go 侧的 `sdk/go/mockhub` 是同一个角色，但**不追求功能等价**：
//! 那边是给插件团队离线开发用的产品件，这边只是这几条测试的脚手架。
//! 有真中台可用时（仓库根 `cargo build -p hub-mock`），验收用那个。
//!
//! **状态面与注册面共用同一个端口**，与真中台一样（插件面就一个端口）。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use hubkit::proto::hub_state_server::{HubState, HubStateServer};
use hubkit::proto::plugin_gateway_server::{PluginGateway, PluginGatewayServer};
use hubkit::proto::plugin_registry_server::{PluginRegistry, PluginRegistryServer};
use hubkit::proto::{
    DescribeMessageRequest, DescribeMessageResponse, GetContractRequest, GetContractResponse,
    HeartbeatRequest, HeartbeatResponse, InvokeOutcome, InvokeRequest, InvokeResponse,
    KvDeleteRequest, KvDeleteResponse, KvGetRequest, KvGetResponse, KvPutRequest, KvPutResponse,
    KvScanRequest, KvScanResponse, ListPluginsRequest, ListPluginsResponse, PluginSummary,
    PublishRequest, PublishResponse, RegisterRequest, RegisterResponse, Rejection,
    UnregisterRequest, UnregisterResponse, ValidationIssue,
};

/// 伪造中台的行为开关。
#[derive(Default, Clone)]
pub struct MockConfig {
    /// 回执里给出的心跳周期（秒）。
    pub heartbeat_interval_seconds: i32,
    /// 收到这么多拍心跳后开始要求插件重新注册。0 = 永不。
    pub reregister_after_beats: usize,
    /// 非空时用它拒绝注册。用来验「被拒后会不会一直重试」。
    pub rejections: Vec<Rejection>,
    /// 注册回执里**不下发**状态凭证——模拟「中台还没配状态面」。
    pub omit_state_token: bool,
}

impl MockConfig {
    pub fn with_interval(seconds: i32) -> Self {
        Self {
            heartbeat_interval_seconds: seconds,
            ..Default::default()
        }
    }
}

/// 记录下来的一切。
#[derive(Default)]
struct Recorded {
    registrations: Vec<RegisterRequest>,
    heartbeats: usize,
    unregisters: Vec<UnregisterRequest>,
    /// 注册表里**当下**的实例行：`instance_id -> 状态凭证`。
    ///
    /// 真中台 `plugin_instances` 表的最小模型。没有它就只能验「插件发了什么请求」，
    /// 验不了「中台删掉的是谁的行」——而注销那道修复要防的恰恰是删错行：
    /// `instance_id` 由插件自报、可以跨插件相撞，先退出的一方原本会删掉**对方**那一行。
    instances: BTreeMap<String, String>,
}

/// 运行中的伪造中台。
pub struct MockHub {
    pub addr: SocketAddr,
    recorded: Arc<Mutex<Recorded>>,
    state: Arc<Mutex<StateInner>>,
    gateway: Arc<Mutex<GatewayInner>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MockHub {
    /// 中台地址，形如 `http://127.0.0.1:12345`——直接喂给 `HUB_ADDR`。
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 收到的注册请求数。
    pub fn registrations(&self) -> Vec<RegisterRequest> {
        self.recorded.lock().unwrap().registrations.clone()
    }

    pub fn registration_count(&self) -> usize {
        self.recorded.lock().unwrap().registrations.len()
    }

    /// 最后一次注册请求。
    pub fn last_registration(&self) -> Option<RegisterRequest> {
        self.recorded.lock().unwrap().registrations.last().cloned()
    }

    pub fn heartbeats(&self) -> usize {
        self.recorded.lock().unwrap().heartbeats
    }

    pub fn unregisters(&self) -> Vec<UnregisterRequest> {
        self.recorded.lock().unwrap().unregisters.clone()
    }

    /// 注册表里**当下**的实例行：`instance_id -> 状态凭证`。
    ///
    /// 注销认属主的效果只能从这里看出来：「插件确实发了注销请求」与「中台确实摘掉了那一行」
    /// 是两回事，而后者才是要验的（真中台对凭证不符的请求照样回一个成功的响应）。
    pub fn instances(&self) -> BTreeMap<String, String> {
        self.recorded.lock().unwrap().instances.clone()
    }

    /// 轮询等待「至少收到 n 次注册」。
    ///
    /// 轮询而不是通知：注册是**异步**发生的（插件在一个独立任务里跑注册循环），
    /// 这里要等的是「它什么时候做完」，而通知机制会把中台与插件的实现细节绑在一起。
    pub async fn wait_for_registrations(&self, n: usize, timeout: Duration) -> bool {
        wait_until(timeout, || self.registration_count() >= n).await
    }

    pub async fn wait_for_heartbeats(&self, n: usize, timeout: Duration) -> bool {
        wait_until(timeout, || self.heartbeats() >= n).await
    }

    /// 停掉伪造中台。
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }

    // ---------------------------------------------------------------- 状态面

    /// 中台当前下发的状态凭证（每次注册换一张）。
    pub fn state_token(&self) -> String {
        self.state.lock().unwrap().token.clone()
    }

    /// 状态请求带上来的凭证，按到达顺序。
    pub fn state_seen_tokens(&self) -> Vec<String> {
        self.state.lock().unwrap().seen_tokens.clone()
    }

    /// 状态调用（四个 Kv 方法）的总次数。
    pub fn state_calls(&self) -> usize {
        self.state.lock().unwrap().calls
    }

    /// **吊销当前凭证**：之后的状态调用一律回 401，直到下一次注册成功。
    ///
    /// 这是「中台重启 / 实例被摘除后旧凭证失效」的最小模型：注册面发一张新的即可恢复。
    pub fn revoke_state(&self) {
        self.state.lock().unwrap().revoked = true;
    }

    /// 直接读状态存储，用来断言「中台那边到底存下了什么」。
    pub fn state_value(&self, namespace: &str, key: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .unwrap()
            .store
            .get(&(namespace.to_string(), key.to_string()))
            .cloned()
    }

    pub fn state_keys(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().store.keys().cloned().collect()
    }

    // ---------------------------------------------------------------- 网关面

    /// 网关面（四个 RPC）的总调用次数。
    pub fn gateway_calls(&self) -> usize {
        self.gateway.lock().unwrap().calls
    }

    /// 网关请求带上来的凭证，按到达顺序。
    pub fn gateway_seen_tokens(&self) -> Vec<String> {
        self.gateway.lock().unwrap().seen_tokens.clone()
    }

    /// ListPlugins 收到的 `include_offline`，按到达顺序。
    pub fn list_requests(&self) -> Vec<bool> {
        self.gateway.lock().unwrap().list_calls.clone()
    }

    /// DescribeMessage 收到的 `fq_name`，按到达顺序。
    pub fn describe_requests(&self) -> Vec<String> {
        self.gateway.lock().unwrap().describe_calls.clone()
    }

    /// GetContract 收到的请求，按到达顺序。
    pub fn contract_requests(&self) -> Vec<GetContractRequest> {
        self.gateway.lock().unwrap().contract_calls.clone()
    }

    /// Invoke 收到的请求，按到达顺序。
    pub fn invoke_requests(&self) -> Vec<InvokeRequest> {
        self.gateway.lock().unwrap().invokes.clone()
    }

    /// 预设 ListPlugins 的应答（缺省空清单）。
    pub fn set_plugins(&self, plugins: Vec<PluginSummary>) {
        self.gateway.lock().unwrap().plugins = plugins;
    }

    /// 预设 DescribeMessage 的应答（缺省两端都空）。
    pub fn set_message_endpoints(&self, endpoints: DescribeMessageResponse) {
        self.gateway.lock().unwrap().endpoints = endpoints;
    }

    /// 预设 GetContract 的应答（缺省 `not_found`）。
    pub fn set_contract(&self, contract: GetContractResponse) {
        self.gateway.lock().unwrap().contract = Some(contract);
    }

    /// 改 Invoke 的应答方式（缺省原信封回显）。
    pub fn set_invoke_behavior(&self, behavior: InvokeBehavior) {
        self.gateway.lock().unwrap().behavior = behavior;
    }

    // ---------------------------------------------------------------- Publish

    /// Publish 请求，按到达顺序。
    pub fn publishes(&self) -> Vec<PublishRequest> {
        self.state.lock().unwrap().publishes.clone()
    }

    /// 之后 Publish 一律回 `accepted=false` 并带这个原因（模拟防环 / 配额拦下）。
    pub fn reject_publishes(&self, reason: &str) {
        self.state.lock().unwrap().publish_reject = Some(reason.to_string());
    }

    /// 恢复 Publish 受理。
    pub fn accept_publishes(&self) {
        self.state.lock().unwrap().publish_reject = None;
    }
}

impl Drop for MockHub {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 伪造状态面的内部状态。
#[derive(Default)]
struct StateInner {
    /// 当前有效的凭证。注册时换一张，与真中台「凭证随每次注册轮换」一致。
    token: String,
    /// 状态请求带上来的凭证，按到达顺序记下来。
    ///
    /// 记的是「请求真的带了什么」，而不是「我们期望它带什么」——
    /// 元数据键名写错时这张表里会是空串，测试据此失败。
    seen_tokens: Vec<String>,
    /// true 时一律回 401（模拟凭证被吊销）。
    revoked: bool,
    /// 状态存储。键是 `(namespace, key)`——真中台会再拼上
    /// `hub:state:{插件名}:` 前缀，这里只验插件可见的那一段。
    store: BTreeMap<(String, String), Vec<u8>>,
    calls: usize,
    /// Publish 请求，按到达顺序记下来。
    publishes: Vec<PublishRequest>,
    /// 非空时 Publish 一律回 `accepted=false` 并带这个原因（模拟防环 / 配额拦下）。
    publish_reject: Option<String>,
}

/// 起一个伪造中台，监听随机端口。
pub async fn start_mock(config: MockConfig) -> MockHub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("伪造中台应能绑上随机端口");
    let addr = listener.local_addr().expect("应能取到本地地址");

    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let state = Arc::new(Mutex::new(StateInner::default()));
    let gateway = Arc::new(Mutex::new(GatewayInner::default()));
    let service = MockRegistryService {
        config,
        recorded: recorded.clone(),
        state: state.clone(),
    };
    let state_service = MockStateService {
        state: state.clone(),
    };
    let gateway_service = MockGatewayService {
        state: state.clone(),
        gateway: gateway.clone(),
    };

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = Server::builder()
            .add_service(PluginRegistryServer::new(service))
            // 与真中台一样，注册面、状态面与网关面在**同一个端口**上
            .add_service(HubStateServer::new(state_service))
            .add_service(PluginGatewayServer::new(gateway_service))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async move {
                    let _ = rx.await;
                },
            )
            .await;
    });

    MockHub {
        addr,
        recorded,
        state,
        gateway,
        shutdown: Some(tx),
    }
}

struct MockRegistryService {
    config: MockConfig,
    recorded: Arc<Mutex<Recorded>>,
    state: Arc<Mutex<StateInner>>,
}

#[async_trait]
impl PluginRegistry for MockRegistryService {
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let request = request.into_inner();
        let mut recorded = self.recorded.lock().unwrap();
        recorded.registrations.push(request.clone());
        let attempt = recorded.registrations.len();

        if !self.config.rejections.is_empty() {
            return Ok(Response::new(RegisterResponse {
                accepted: false,
                rejections: self.config.rejections.clone(),
                ..Default::default()
            }));
        }

        // 凭证随每次注册轮换，且新注册会让被吊销的凭证重新有效——
        // 真中台的行为就是这个（重新注册 = 实例又回来了），自愈测试依赖它。
        let token = if self.config.omit_state_token {
            String::new()
        } else {
            format!("mock-state-token-{attempt}")
        };
        {
            let mut state = self.state.lock().unwrap();
            state.token = token.clone();
            state.revoked = false;
        }

        // 注册即占一行，同一个 `instance_id` 的重复注册覆盖它（中台侧是 upsert）。
        // `omit_state_token` 时这一行的凭证是空串——与真中台一致：迁移前登记的旧行、
        // 或没配状态面的实例，就是「有行但没凭证」。
        recorded
            .instances
            .insert(request.instance_id.clone(), token.clone());

        Ok(Response::new(RegisterResponse {
            accepted: true,
            // 与真中台一致：回执里的 instance_id 就是请求里那个
            instance_id: request.instance_id.clone(),
            heartbeat_interval_seconds: self.config.heartbeat_interval_seconds,
            state_token: token,
            ..Default::default()
        }))
    }

    async fn heartbeat(
        &self,
        _request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let mut recorded = self.recorded.lock().unwrap();
        recorded.heartbeats += 1;
        let beats = recorded.heartbeats;

        let reregister =
            self.config.reregister_after_beats > 0 && beats > self.config.reregister_after_beats;

        Ok(Response::new(HeartbeatResponse {
            accepted: !reregister,
            heartbeat_interval_seconds: self.config.heartbeat_interval_seconds,
            reregister_required: reregister,
        }))
    }

    async fn unregister(
        &self,
        request: Request<UnregisterRequest>,
    ) -> Result<Response<UnregisterResponse>, Status> {
        let request = request.into_inner();
        let mut recorded = self.recorded.lock().unwrap();
        recorded.unregisters.push(request.clone());

        // **认属主**：只有凭证与这一行当前的状态凭证一致才摘掉它，空凭证或不符一律不删。
        // 这是真中台 `delete_instance_with_token` 的判定（判定与删除在同一条 SQL 里，
        // 没有先查后删的窗口），也是本次修复要钉住的那条契约。
        //
        // 只把请求记下来是不够的：那样「注销到底删了谁的行」压根没被验到，
        // 插件不带凭证、或带着别人的凭证来注销，测试照样全绿。
        let owner_matches = recorded
            .instances
            .get(&request.instance_id)
            .is_some_and(|current| {
                !request.state_token.is_empty() && *current == request.state_token
            });
        if owner_matches {
            recorded.instances.remove(&request.instance_id);
        }

        // 与真中台一致：拒绝也是回一个成功的空响应——`UnregisterResponse` 里没有任何字段，
        // 插件本来就无从分辨自己摘掉的是不是自己那一行。判定只发生在中台侧。
        Ok(Response::new(UnregisterResponse {}))
    }
}

/// 状态凭证的 metadata 键名，**故意写成字面量**而不是引用 SDK 的
/// `hubkit::STATE_TOKEN_METADATA`：引用常量的话，常量被改成别的字符串时这条
/// 伪造中台会跟着改口，测试永远绿。写成字面量，改错了就是一片 401。
///
/// 状态面与网关面共用这个键——真中台就是同一个凭证、同一个键名（`x-hub-state-token`）。
const STATE_TOKEN_HEADER: &str = "x-hub-state-token";

/// 网关 Invoke 替身的应答方式。
///
/// 默认回显：HANDLED + 原信封原样返回，测试从 `invoke_requests()` 断言
/// 「中台收到了什么」，从返回值断言「插件看到了什么」，互不干扰。
#[derive(Clone, Default)]
pub enum InvokeBehavior {
    /// HANDLED：原信封回显
    #[default]
    Echo,
    /// REJECTED：带原因与结构化 issues（验「issues 随异常携带」用）
    Reject {
        /// REJECTED 的 reason
        reason: String,
        /// REJECTED 的 issues
        issues: Vec<ValidationIssue>,
    },
    /// ERROR：带原因（未授权、超限、成环这类中台裁决）
    Fail {
        /// ERROR 的 reason
        reason: String,
    },
}

/// 网关面记下来的请求与可注入的应答。
#[derive(Default)]
struct GatewayInner {
    /// 四个 RPC 的总次数。
    calls: usize,
    /// 网关请求带上来的凭证，按到达顺序记下来（记「请求真的带了什么」，理由同状态面）。
    seen_tokens: Vec<String>,
    list_calls: Vec<bool>,
    describe_calls: Vec<String>,
    contract_calls: Vec<GetContractRequest>,
    invokes: Vec<InvokeRequest>,
    /// ListPlugins 的罐装应答（缺省空清单）。
    plugins: Vec<PluginSummary>,
    /// DescribeMessage 的罐装应答（缺省两端都空）。
    endpoints: DescribeMessageResponse,
    /// GetContract 的罐装应答（缺省 `not_found`，与真中台「查无此插件」同语义）。
    contract: Option<GetContractResponse>,
    behavior: InvokeBehavior,
}

/// 伪造状态面：真中台 `crates/hub-grpc/src/state.rs` 的最小模型。
///
/// 它只做两件事：**认凭证**与**存取**。键前缀、上限、租户这些真中台会做的事
/// 不在这里复刻——那是中台的职责，客户端不该依赖它，也不该在这里被验。
struct MockStateService {
    state: Arc<Mutex<StateInner>>,
}

impl MockStateService {
    /// 认凭证并记一笔。`Err` 就是中台侧的那个 401。
    fn authenticate<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let token = request
            .metadata()
            .get(STATE_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();

        let mut state = self.state.lock().unwrap();
        state.calls += 1;
        state.seen_tokens.push(token.clone());

        // 吊销后一律 401，不看请求带的是什么
        if state.revoked || token.is_empty() || token != state.token {
            return Err(Status::unauthenticated("状态凭证无效或已失效"));
        }
        Ok(())
    }
}

#[async_trait]
impl HubState for MockStateService {
    async fn kv_get(
        &self,
        request: Request<KvGetRequest>,
    ) -> Result<Response<KvGetResponse>, Status> {
        self.authenticate(&request)?;
        let key = request.into_inner().key.unwrap_or_default();
        let state = self.state.lock().unwrap();
        match state.store.get(&(key.namespace, key.key)) {
            // found 与「值是空字节」是两回事，与真中台一致
            Some(value) => Ok(Response::new(KvGetResponse {
                found: true,
                value: value.clone(),
            })),
            None => Ok(Response::new(KvGetResponse {
                found: false,
                value: Vec::new(),
            })),
        }
    }

    async fn kv_put(
        &self,
        request: Request<KvPutRequest>,
    ) -> Result<Response<KvPutResponse>, Status> {
        self.authenticate(&request)?;
        let inner = request.into_inner();
        let key = inner.key.unwrap_or_default();
        self.state
            .lock()
            .unwrap()
            .store
            .insert((key.namespace, key.key), inner.value);
        Ok(Response::new(KvPutResponse {}))
    }

    async fn kv_delete(
        &self,
        request: Request<KvDeleteRequest>,
    ) -> Result<Response<KvDeleteResponse>, Status> {
        self.authenticate(&request)?;
        let key = request.into_inner().key.unwrap_or_default();
        let removed = self
            .state
            .lock()
            .unwrap()
            .store
            .remove(&(key.namespace, key.key))
            .is_some();
        // 删不存在的键返回 false，不是错误
        Ok(Response::new(KvDeleteResponse { deleted: removed }))
    }

    async fn kv_scan(
        &self,
        request: Request<KvScanRequest>,
    ) -> Result<Response<KvScanResponse>, Status> {
        self.authenticate(&request)?;
        let inner = request.into_inner();
        let state = self.state.lock().unwrap();

        let entries = state
            .store
            .iter()
            .filter(|((ns, key), _)| ns == &inner.namespace && key.starts_with(&inner.prefix))
            .take(inner.limit as usize)
            .map(|((_, key), value)| hubkit::proto::KvEntry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();

        Ok(Response::new(KvScanResponse { entries }))
    }

    async fn publish(
        &self,
        request: Request<PublishRequest>,
    ) -> Result<Response<PublishResponse>, Status> {
        self.authenticate(&request)?;
        let inner = request.into_inner();

        let mut state = self.state.lock().unwrap();
        state.publishes.push(inner);

        // 防环 / 配额拦下的业务结果：accepted=false + reason，与真中台同语义
        if let Some(reason) = state.publish_reject.clone() {
            return Ok(Response::new(PublishResponse {
                accepted: false,
                run_id: String::new(),
                reason,
            }));
        }

        // 受理：给一个能认出来的 run_id（真中台回的是 flow 执行 id，这里只是序号）
        let n = state.publishes.len();
        Ok(Response::new(PublishResponse {
            accepted: true,
            run_id: format!("mock-run-{n}"),
            reason: String::new(),
        }))
    }
}

/// 伪造网关面：真中台 `crates/hub-grpc/src/gateway.rs` 的最小模型。
///
/// 只做三件事：**认凭证**（与状态面同一份凭证表）、**记录请求**、按
/// [`InvokeBehavior`] 给罐装应答。真中台的防环、链深、配额、subject 覆盖、
/// deadline 夹紧都不在这里复刻——那些是中台的职责，SDK 单测要验的是
/// 「SDK 发出的请求长什么样」，验「中台怎么处理」是 `crates/hub-grpc/tests/gateway.rs` 的事。
struct MockGatewayService {
    state: Arc<Mutex<StateInner>>,
    gateway: Arc<Mutex<GatewayInner>>,
}

impl MockGatewayService {
    /// 认凭证并记一笔。`Err` 就是中台侧的那个 401。
    ///
    /// 与状态面**同一个凭证表**：注册轮换凭证发生在注册面，网关面只是读它——
    /// 吊销状态（`revoke_state`）对网关调用同样生效，401 自愈测试靠这个。
    fn authenticate<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let token = request
            .metadata()
            .get(STATE_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();

        {
            let mut gateway = self.gateway.lock().unwrap();
            gateway.calls += 1;
            gateway.seen_tokens.push(token.clone());
        }

        let state = self.state.lock().unwrap();
        if state.revoked || token.is_empty() || token != state.token {
            return Err(Status::unauthenticated("状态凭证无效或已失效"));
        }
        Ok(())
    }
}

#[async_trait]
impl PluginGateway for MockGatewayService {
    async fn list_plugins(
        &self,
        request: Request<ListPluginsRequest>,
    ) -> Result<Response<ListPluginsResponse>, Status> {
        self.authenticate(&request)?;
        let include_offline = request.into_inner().include_offline;
        let mut gateway = self.gateway.lock().unwrap();
        gateway.list_calls.push(include_offline);
        Ok(Response::new(ListPluginsResponse {
            plugins: gateway.plugins.clone(),
        }))
    }

    async fn describe_message(
        &self,
        request: Request<DescribeMessageRequest>,
    ) -> Result<Response<DescribeMessageResponse>, Status> {
        self.authenticate(&request)?;
        let fq_name = request.into_inner().fq_name;
        let mut gateway = self.gateway.lock().unwrap();
        gateway.describe_calls.push(fq_name);
        Ok(Response::new(gateway.endpoints.clone()))
    }

    async fn get_contract(
        &self,
        request: Request<GetContractRequest>,
    ) -> Result<Response<GetContractResponse>, Status> {
        self.authenticate(&request)?;
        let inner = request.into_inner();
        let mut gateway = self.gateway.lock().unwrap();
        gateway.contract_calls.push(inner);
        match gateway.contract.clone() {
            Some(contract) => Ok(Response::new(contract)),
            // 与真中台一致：查无此插件（或没预设应答）是 not_found，不是空契约
            None => Err(Status::not_found("mock 未预设 GetContract 应答")),
        }
    }

    async fn invoke(
        &self,
        request: Request<InvokeRequest>,
    ) -> Result<Response<InvokeResponse>, Status> {
        self.authenticate(&request)?;
        let inner = request.into_inner();

        let mut gateway = self.gateway.lock().unwrap();
        gateway.invokes.push(inner.clone());

        match &gateway.behavior {
            InvokeBehavior::Echo => {
                // 原信封回显：下游「处理完成、把信封还回来」的最小模型
                Ok(Response::new(InvokeResponse {
                    outcome: InvokeOutcome::Handled as i32,
                    envelope: inner.envelope,
                    ..Default::default()
                }))
            }
            InvokeBehavior::Reject { reason, issues } => Ok(Response::new(InvokeResponse {
                outcome: InvokeOutcome::Rejected as i32,
                reason: reason.clone(),
                issues: issues.clone(),
                ..Default::default()
            })),
            InvokeBehavior::Fail { reason } => Ok(Response::new(InvokeResponse {
                outcome: InvokeOutcome::Error as i32,
                reason: reason.clone(),
                ..Default::default()
            })),
        }
    }
}

/// 轮询等待一个条件成立。
pub async fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cond()
}

/// 探一个空闲端口。**只给真的需要预知端口的测试用**：要直连插件的
/// `plugin_service.rs`、真中台会做可达性探测的 `state_live.rs`。
///
/// mock 从不回拨插件，注册类测试里预挑端口**只剩竞态没有收益**——那些测试
/// 应当用 [`PLUGIN_LISTEN_ANY`] 让内核在 bind 的原子时刻分配端口。
///
/// 刻意**不 bind(:0) 再放掉**（早先的做法，与 Go 侧 `mockhub.FreeAddr` 相同）：
/// 那是把端口还进内核的**临时端口池**。单独跑测试时窗口极小，但并行验证门下
/// （工作区测试、Go/Python 的 mockhub 套件同时高频 bind/close :0）临时池会快速
/// 回卷，刚放掉的端口会在插件 bind 之前被别的进程重新拿走——插件在自己的
/// `TcpListener::bind` 上撞 `AddrInUse` **静默退出**（bind 错误只随返回值走，
/// 不打日志），测试只看到「插件没注册上来」，实测复现过。因此改为在临时端口段
/// **之外**随机挑口（macOS 默认 49152+、Linux 默认 32768+，这里取 15000..30000，
/// 两个平台都摸不到）。
pub fn free_port() -> u16 {
    // **每次调用**的输入 = 进程级种子 ^ 调用序号。种子只初始化一次，序号用原子
    // 计数器递增——**不能用「每次调用取当前时刻」当种子**：macOS 的时钟粒度是
    // 微秒级（实测连续调用返回同一个值），并行测试线程在同一微秒内各自调用
    // free_port 时会拿到相同种子、挑中同一个端口，然后互相在 bind 上踩死对方。
    // 序号保证同进程内每次调用必然不同；`pid` 混进种子保证不同测试进程错开。
    static PORT_SEQ: AtomicU64 = AtomicU64::new(0);
    let mut state = PORT_SEQ.fetch_add(1, Ordering::Relaxed)
        ^ (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            ^ ((std::process::id() as u64) << 32)
            | 0x9E37_79B9_7F4A_7C15);
    for _ in 0..100 {
        // splitmix64 终混：低位直接用 LCG 会分布不均
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let port = 15_000u16 + ((z ^ (z >> 31)) % 15_000) as u16;
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            drop(listener);
            return port;
        }
    }
    // 随机带全被占住（几乎不可能）时退回旧做法：宁可带着理论上的竞态窗口，
    // 也不让测试因为挑不出口而直接失败
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("应能探到空闲端口");
    listener.local_addr().unwrap().port()
}

/// 插件监听地址：`127.0.0.1:0`，端口由内核在插件 bind 的**原子时刻**分配，
/// 结构上不存在「预挑的端口被并行进程抢走」的竞态（见 [`free_port`] 的文档）。
///
/// mock 从不回拨插件，所以「测试不知道插件的真实端口」毫无代价；
/// 需要直连插件（`plugin_service.rs`）或真中台可达性探测（`state_live.rs`）
/// 的测试才需要预挑端口，用 [`free_port`]。
pub const PLUGIN_LISTEN_ANY: &str = "127.0.0.1:0";

/// [`PLUGIN_LISTEN_ANY`] 搭配的 advertise 占位：mock 按契约**原样记录**它
/// （不拨号验证），注册类断言比对的是「原样上报」，不是「可达」。
/// 取 29999 是刻意的：没人监听它，谁要是真去拨它，错误一眼可认。
pub const PLUGIN_ADVERTISE: &str = "http://127.0.0.1:29999";

/// 一句能直接喂给 `Config` 的地址。
pub fn local_addr(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// 断言失败时给出人话的解释。
pub fn msg(what: &str) -> String {
    format!("等待「{what}」超时")
}

/// 连一个插件的 gRPC，用来直连打它（L2 那一层）。
pub async fn connect_runtime(
    addr: &str,
) -> hubkit::proto::plugin_runtime_client::PluginRuntimeClient<tonic::transport::Channel> {
    hubkit::proto::plugin_runtime_client::PluginRuntimeClient::connect(addr.to_string())
        .await
        .expect("插件应当已起来并能连上")
}

/// 一个计数的原子量，测试里用来数「插件被调了几次」。
pub type Counter = Arc<AtomicUsize>;

pub fn counter() -> Counter {
    Arc::new(AtomicUsize::new(0))
}

pub fn count(c: &Counter) -> usize {
    c.load(Ordering::SeqCst)
}

pub fn bump(c: &Counter) {
    c.fetch_add(1, Ordering::SeqCst);
}

// ------------------------------------------------------------ 被测插件

/// 集成测试用的最小插件：Validate 要求 `payload.text` 非空，Handle 回显。
///
/// 刻意做得像脚手架生成出来的那个插件——测试验的就是开发者拿到手的那条路。
pub struct DemoPlugin;

#[async_trait]
impl hubkit::Plugin for DemoPlugin {
    fn manifest(&self) -> hubkit::PluginManifest {
        hubkit::PluginManifest {
            name: "test-plugin".to_string(),
            version: "0.1.0".to_string(),
            description: "集成测试用的插件".to_string(),
            owner: "测试".to_string(),
            consumes: vec![hubkit::proto::MessageContract {
                fq_name: hubkit::envelope::STRUCT_FQ_NAME.to_string(),
                description: String::new(),
            }],
            tools: vec![hubkit::proto::ToolDecl {
                name: "echo".to_string(),
                description: "回显".to_string(),
                input_schema_json: r#"{"type":"object"}"#.to_string(),
                requires_approval: false,
            }],
            ..Default::default()
        }
    }

    async fn validate(
        &self,
        envelope: &hubkit::Envelope,
    ) -> Result<hubkit::ValidateResponse, hubkit::PluginError> {
        let Some(payload) = hubkit::envelope::payload_json(envelope) else {
            return Ok(hubkit::envelope::invalid(vec![hubkit::envelope::issue(
                "payload",
                "需要 JSON 对象载荷",
            )]));
        };
        match payload.get("text").and_then(|v| v.as_str()) {
            None | Some("") => Ok(hubkit::envelope::invalid(vec![hubkit::envelope::issue(
                "payload.text",
                "缺少必填字段 text",
            )])),
            Some(_) => Ok(hubkit::envelope::valid()),
        }
    }

    async fn handle(
        &self,
        envelope: hubkit::Envelope,
    ) -> Result<hubkit::Envelope, hubkit::PluginError> {
        let payload = hubkit::envelope::payload_json(&envelope).unwrap_or_default();
        let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");

        // `?` 能直接用：PluginError 有一条 From<E: Error + Send + Sync> 的转换
        let out = hubkit::envelope::with_payload_json(
            &envelope,
            serde_json::json!({ "echo": format!("test-plugin 收到: {text}") }),
        )?;
        Ok(out)
    }
}

/// 把日志关到只剩 error，免得测试输出被注册日志淹掉。
pub fn quiet() -> hubkit::Logger {
    hubkit::Logger::new(hubkit::Level::Error)
}

// ------------------------------------------------------------ 需要状态的插件

/// 一个**需要用状态**的插件：注册成功后应当收到状态客户端注入。
///
/// 注入发生在注册循环那个任务上，而断言跑在测试任务上——所以内部用 `Mutex` 护住，
/// 顺带就是文档里那句「同步是实现方的责任」的示范（裸赋值给一个被别的任务读的字段
/// 就是数据竞争）。
///
/// 是 `Clone` 的（内部共享同一份 `Arc`）：`run` 要吃走插件本体，而测试还要留着
/// 一个句柄读「注入进来了什么」。
#[derive(Clone)]
pub struct StatePlugin {
    inner: Arc<StatePluginInner>,
}

struct StatePluginInner {
    name: String,
    state: Mutex<Option<Arc<hubkit::StateClient>>>,
    /// 被注入过几次。用来验「每次重新注册都会再注入一次」。
    injections: AtomicUsize,
}

impl StatePlugin {
    pub fn named(name: &str) -> Self {
        Self {
            inner: Arc::new(StatePluginInner {
                name: name.to_string(),
                state: Mutex::new(None),
                injections: AtomicUsize::new(0),
            }),
        }
    }

    /// 当前那份状态客户端；还没被注入时是 `None`。
    pub fn state(&self) -> Option<Arc<hubkit::StateClient>> {
        self.inner.state.lock().unwrap().clone()
    }

    /// 等注入完成，返回客户端；超时则失败。
    ///
    /// 轮询而不是通知：注入是**异步**发生的（注册循环跑在另一个任务里），
    /// 这里要等的是「它什么时候做完」。
    pub async fn wait_for_state(&self, timeout: Duration) -> Arc<hubkit::StateClient> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(state) = self.state() {
                return state;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{}",
                msg("状态客户端注入")
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 已注入过几次。
    pub fn injections(&self) -> usize {
        self.inner.injections.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl hubkit::Plugin for StatePlugin {
    fn manifest(&self) -> hubkit::PluginManifest {
        hubkit::PluginManifest {
            name: self.inner.name.clone(),
            version: "0.1.0".to_string(),
            description: "集成测试用的、需要外置状态的插件".to_string(),
            owner: "测试".to_string(),
            ..Default::default()
        }
    }

    async fn validate(
        &self,
        _envelope: &hubkit::Envelope,
    ) -> Result<hubkit::ValidateResponse, hubkit::PluginError> {
        Ok(hubkit::envelope::valid())
    }

    async fn handle(
        &self,
        envelope: hubkit::Envelope,
    ) -> Result<hubkit::Envelope, hubkit::PluginError> {
        Ok(envelope)
    }

    fn set_state(&self, state: Arc<hubkit::StateClient>) {
        *self.inner.state.lock().unwrap() = Some(state);
        self.inner.injections.fetch_add(1, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------ 需要网关的插件

/// 一个**要调别的插件**的插件：注册成功后应当收到网关客户端注入。
///
/// 形态与 [`StatePlugin`] 同款（`Mutex` 护住跨任务注入、`Clone` 共享内部状态、
/// 轮询等注入完成），理由也在那里——不重复。
#[derive(Clone)]
pub struct GatewayPlugin {
    inner: Arc<GatewayPluginInner>,
}

struct GatewayPluginInner {
    name: String,
    gateway: Mutex<Option<Arc<hubkit::GatewayClient>>>,
    /// 被注入过几次。用来验「每次重新注册都会再注入一次」。
    injections: AtomicUsize,
}

impl GatewayPlugin {
    pub fn named(name: &str) -> Self {
        Self {
            inner: Arc::new(GatewayPluginInner {
                name: name.to_string(),
                gateway: Mutex::new(None),
                injections: AtomicUsize::new(0),
            }),
        }
    }

    /// 当前那份网关客户端；还没被注入时是 `None`。
    pub fn gateway(&self) -> Option<Arc<hubkit::GatewayClient>> {
        self.inner.gateway.lock().unwrap().clone()
    }

    /// 等注入完成，返回客户端；超时则失败。
    pub async fn wait_for_gateway(&self, timeout: Duration) -> Arc<hubkit::GatewayClient> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(gateway) = self.gateway() {
                return gateway;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{}",
                msg("网关客户端注入")
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 已注入过几次。
    pub fn injections(&self) -> usize {
        self.inner.injections.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl hubkit::Plugin for GatewayPlugin {
    fn manifest(&self) -> hubkit::PluginManifest {
        hubkit::PluginManifest {
            name: self.inner.name.clone(),
            version: "0.1.0".to_string(),
            description: "集成测试用的、要调别的插件的插件".to_string(),
            owner: "测试".to_string(),
            ..Default::default()
        }
    }

    async fn validate(
        &self,
        _envelope: &hubkit::Envelope,
    ) -> Result<hubkit::ValidateResponse, hubkit::PluginError> {
        Ok(hubkit::envelope::valid())
    }

    async fn handle(
        &self,
        envelope: hubkit::Envelope,
    ) -> Result<hubkit::Envelope, hubkit::PluginError> {
        Ok(envelope)
    }

    fn set_gateway(&self, gateway: Arc<hubkit::GatewayClient>) {
        *self.inner.gateway.lock().unwrap() = Some(gateway);
        self.inner.injections.fetch_add(1, Ordering::SeqCst);
    }
}
