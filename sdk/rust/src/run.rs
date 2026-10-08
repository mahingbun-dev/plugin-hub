//! 插件侧的服务端骨架：起 gRPC、自注册、心跳、被摘除后自愈、优雅退出。
//!
//! 插件作者只需要实现 [`Plugin`]，然后调用 [`run`]。最小插件长这样：
//!
//! ```no_run
//! use hubkit::{Config, Envelope, PluginError, PluginManifest, ValidateResponse};
//! use hubkit::proto::{Envelope as Env, ValidateResponse as VR};
//! # struct MyPlugin;
//! # #[async_trait::async_trait]
//! # impl hubkit::Plugin for MyPlugin {
//! #   fn manifest(&self) -> PluginManifest { unimplemented!() }
//! #   async fn validate(&self, _e: &Env) -> Result<VR, PluginError> { unimplemented!() }
//! #   async fn handle(&self, e: Env) -> Result<Env, PluginError> { Ok(e) }
//! # }
//! # async fn demo() -> Result<(), hubkit::HubkitError> {
//! hubkit::run(MyPlugin, Config::from_env()).await
//! # }
//! ```
//!
//! 三个刻意的行为，写在这里是因为它们会在排障时被问到：
//!
//!   - **注册会一直重试**：中台可能比插件晚起来，插件先启动是常态。
//!   - **被摘除后自动重新注册**：心跳回执里带 `reregister_required` 时重走注册流程。
//!     这是实例掉线后能自愈的关键，也是本模块被单测覆盖得最狠的一条分支。
//!     状态调用撞上 401 也走这条（见 [`Plugin::set_state`]）。
//!   - **停止信号会打断所有等待**：重试与心跳的等待都是可被打断的，
//!     否则一次 SIGTERM 要等满一个心跳周期才生效。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::Stream;
use tonic::transport::{Channel, Server};
use tonic::{Request, Response, Status};

use crate::config::{Config, UNREGISTER_TIMEOUT};
use crate::envelope::is_well_known_fq_name;
use crate::error::{reject_code_name, HubkitError, PluginError};
use crate::gateway::GatewayClient;
use crate::log::{Logger, Value};
use crate::proto::hub_state_client::HubStateClient;
use crate::proto::plugin_gateway_client::PluginGatewayClient;
use crate::proto::plugin_registry_client::PluginRegistryClient;
use crate::proto::plugin_runtime_server::{PluginRuntime, PluginRuntimeServer};
use crate::proto::{
    DescribeRequest, Envelope, HandleRequest, HandleResponse, HealthRequest, HealthResponse,
    HeartbeatRequest, PluginManifest, RegisterRequest, UnregisterRequest, ValidateRequest,
    ValidateResponse,
};
use crate::state::StateClient;

/// 插件必须实现的接口。
///
/// `manifest` 与 `descriptor` 是「我给中台什么」，`validate` 与 `handle` 是「我干什么」
/// ——四者缺一不可：契约校验、编排连线、MCP 工具聚合都建立在它们之上。
///
/// 用 `#[async_trait]` 而不是裸 `async fn`：这个 trait 会被 [`PluginRuntimeServer`]
/// 在 `Send` 边界后面调用，而裸 `async fn` 返回的 future 不带 `Send` 保证。
/// tonic 生成的代码自己也是同一套做法。
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// 声明插件的身份、契约（消费/生产哪些消息类型）与暴露给 agent 的工具。
    ///
    /// **同一版本号的 manifest 不可变更**：中台会拒绝「同号不同契约」的注册，
    /// 改了东西请升版本号。
    fn manifest(&self) -> PluginManifest;

    /// 本插件的 `google.protobuf.FileDescriptorSet` 序列化字节。
    ///
    /// 中台据此建立契约基线、做字段级兼容检查。只用 `google.protobuf.Struct`
    /// 承载 JSON 的插件没有自己的 proto，**返回空即可**。
    ///
    /// 要提交自有 proto 时，在 `build.rs` 里给 `tonic_prost_build` 配上
    /// `.file_descriptor_set_path(...)`，再把这个文件的内容嵌进来：
    ///
    /// ```ignore
    /// fn descriptor(&self) -> Vec<u8> {
    ///     include_bytes!(concat!(env!("OUT_DIR"), "/plugin_descriptor.bin")).to_vec()
    /// }
    /// ```
    ///
    /// 注意**只发本插件自己的 proto**，不要带上它 import 的依赖——
    /// 中台侧只关心本插件定义的消息，且它不要求 descriptor 自包含。
    fn descriptor(&self) -> Vec<u8> {
        Vec::new()
    }

    /// 数据校验规则。
    ///
    /// 中台在把数据交给 [`Plugin::handle`] 之前**一定**先调用它；返回
    /// `valid = false` 时链路短路，`Handle` 不会被调用。校验规则与插件体同版本发布，
    /// 因此规则不可能与实现漂移。
    async fn validate(&self, envelope: &Envelope) -> Result<ValidateResponse, PluginError>;

    /// 插件体：自主实现的数据输入输出。
    ///
    /// 输入载荷用 [`crate::envelope::payload_json`] 取（直接调用场景），
    /// 输出用 [`crate::envelope::with_payload_json`] 写。
    async fn handle(&self, envelope: Envelope) -> Result<Envelope, PluginError>;

    /// 逐条结果的流式版本。
    ///
    /// 默认实现 = 「把 `handle` 的单个结果当成只有一条的流」，这对绝大多数插件是对的。
    /// 要一次吐多条输出（大报文分片、批量结果）的插件覆盖它即可。
    ///
    /// 刻意返回 `Vec<Envelope>` 而不是一个 `Stream`：让插件作者去实现 `Stream`
    /// 是把 Rust 的类型体操强加给业务代码，而这里要的只是「几条结果」。
    async fn handle_stream(&self, envelope: Envelope) -> Result<Vec<Envelope>, PluginError> {
        Ok(vec![self.handle(envelope).await?])
    }

    /// 接收外置状态（HubState）客户端。**需要状态的插件覆盖它**；默认什么都不做。
    ///
    /// 由骨架在**每次注册成功后**调用一次——包括心跳要求重注册、以及状态调用撞上 401
    /// 之后的那次。每次都给同一个 [`Arc`]（客户端是复用实例），所以插件**不必**
    /// 每次都换掉自己那份引用；但每一次都要调的理由是：调用的时机就是
    /// 「此刻凭证已经换成新的了」这个事实，插件完全可以借它清理自己那份缓存。
    ///
    /// **不要自己管凭证**：它是中台在注册回执里下发的，插件构造不出也猜不到。
    /// 拿到客户端直接 [`StateClient::get`] / [`StateClient::put`] 即可，
    /// 401 的自愈由骨架负责（见 [`crate::state`]）。
    ///
    /// 与 Go 的形态差异：Go 侧是一个可选接口 `StateAware`（`SetState(*StateClient)`）。
    /// Rust 没有稳定的特化，「泛型 P 是否实现了某个 trait」在语言层面问不出来，
    /// 可选接口只能靠不稳定的特化或 downcast 技巧实现——那对一个要发给插件团队的
    /// SDK 来说是过头的。带默认实现的钩子达成的是同一件事：**不用状态的老插件一行都不用改**，
    /// 而且比可选接口少一个「实现了却没接上」的坑。
    ///
    /// # 同步是实现方的责任
    ///
    /// 骨架从**注册循环那个任务**调用它，而 [`Plugin::handle`] 通常跑在别的任务上。
    /// 把参数裸赋值给一个会被其它任务读的字段就是数据竞争——用
    /// `Mutex<Option<Arc<StateClient>>>`、`RwLock` 或 `OnceLock` 护住它。
    fn set_state(&self, _state: Arc<StateClient>) {}

    /// 接收插件网关（发现与互调）客户端。**要调别的插件的插件覆盖它**；默认什么都不做。
    ///
    /// 形态与 [`Plugin::set_state`] 完全同款，理由也相同：四个网关 RPC 全部凭
    /// **同一个**注册凭证鉴权，凭证只有骨架知道；注入时机也一样——每次注册成功后，
    /// 正是「此刻凭证已经换成新的了」这件事发生的那一刻。同步的责任、护住字段的手法，
    /// 都照 [`Plugin::set_state`] 的文档做，那里说得更全。
    fn set_gateway(&self, _gateway: Arc<GatewayClient>) {}
}

// ------------------------------------------------------------------ gRPC 服务

/// 把 [`Plugin`] 接到生成的 `PluginRuntime` 服务上。
struct RuntimeService<P> {
    plugin: Arc<P>,
}

#[async_trait]
impl<P: Plugin> PluginRuntime for RuntimeService<P> {
    async fn describe(
        &self,
        _request: Request<DescribeRequest>,
    ) -> Result<Response<PluginManifest>, Status> {
        Ok(Response::new(self.plugin.manifest()))
    }

    async fn validate(
        &self,
        request: Request<ValidateRequest>,
    ) -> Result<Response<ValidateResponse>, Status> {
        // 信封缺失是**调用方**的问题，回 InvalidArgument 而不是 Internal：
        // 中台据此区分「插件坏了」与「中台组了个空请求」。
        let envelope = request
            .into_inner()
            .envelope
            .ok_or_else(|| Status::invalid_argument("缺少信封"))?;

        match self.plugin.validate(&envelope).await {
            Ok(response) => Ok(Response::new(response)),
            Err(e) => Err(Status::internal(format!("校验器执行失败: {e}"))),
        }
    }

    async fn handle(
        &self,
        request: Request<HandleRequest>,
    ) -> Result<Response<HandleResponse>, Status> {
        let envelope = request
            .into_inner()
            .envelope
            .ok_or_else(|| Status::invalid_argument("缺少信封"))?;

        match self.plugin.handle(envelope).await {
            // 插件自己的错误原样上报：中台会把它归到「插件调用失败」，
            // 调用方据此重试，而不是把它当成一份合法的空结果。
            Err(e) => Err(Status::internal(format!("插件处理失败: {e}"))),
            Ok(out) => Ok(Response::new(HandleResponse {
                envelope: Some(out),
            })),
        }
    }

    type HandleStreamStream =
        Pin<Box<dyn Stream<Item = Result<HandleResponse, Status>> + Send + 'static>>;

    async fn handle_stream(
        &self,
        request: Request<HandleRequest>,
    ) -> Result<
        Response<Pin<Box<dyn Stream<Item = Result<HandleResponse, Status>> + Send + 'static>>>,
        Status,
    > {
        let envelope = request
            .into_inner()
            .envelope
            .ok_or_else(|| Status::invalid_argument("缺少信封"))?;

        match self.plugin.handle_stream(envelope).await {
            Err(e) => Err(Status::internal(format!("插件处理失败: {e}"))),
            Ok(envelopes) => {
                let items: Vec<Result<HandleResponse, Status>> = envelopes
                    .into_iter()
                    .map(|envelope| {
                        Ok(HandleResponse {
                            envelope: Some(envelope),
                        })
                    })
                    .collect();
                Ok(Response::new(Box::pin(tokio_stream::iter(items))))
            }
        }
    }

    async fn health(
        &self,
        _request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        // 这里刻意**不**去问插件「你还好吗」：插件的健康状况由它的业务调用体现，
        // 而健康探测的用途是「中台能不能拨通你」——那是网络层的事，进程活着就是 healthy。
        // 让插件自定义健康检查会在「中台探测超时」与「插件自报不健康」之间制造歧义。
        Ok(Response::new(HealthResponse {
            healthy: true,
            message: "ok".to_string(),
        }))
    }
}

// ------------------------------------------------------------ 启动 / 优雅退出

/// 启动插件，直到收到 SIGINT / SIGTERM。
///
/// 它做四件事：起 gRPC 服务、向中台自注册、维持心跳、优雅退出时注销。
/// 注销要凭注册时下发的凭证——**没注册成功过就跳过**，理由见模块内 `unregister` 的说明。
pub async fn run<P: Plugin>(plugin: P, config: Config) -> Result<(), HubkitError> {
    run_with_shutdown(plugin, config, shutdown_signal()).await
}

/// 与 [`run`] 相同，但由调用方决定何时结束。
///
/// 测试与嵌入式场景用它：给一个会 resolve 的 future 就能把插件干净地停掉，
/// 不必真的发信号——发信号是全局副作用，并行跑的测试会互相打断。
pub async fn run_with_shutdown<P, F>(
    plugin: P,
    config: Config,
    shutdown: F,
) -> Result<(), HubkitError>
where
    P: Plugin,
    F: Future<Output = ()> + Send,
{
    let config = config.with_defaults();
    config.validate()?;
    let log = config.logger.clone();

    // manifest 的明显问题在本地就挡住。中台也会拒，但那是网络往返之后的事——
    // 本地先炸省一轮排查，也省得对着「注册一直重试」猜。
    check_manifest(&plugin)?;

    let manifest = plugin.manifest();
    let descriptor = plugin.descriptor();

    let bind_addr = config.listen_socket_addr()?;
    let listener = TcpListener::bind(bind_addr)
        .await
        .map_err(|source| HubkitError::Bind {
            addr: bind_addr.to_string(),
            source,
        })?;
    let local_addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| bind_addr.to_string());

    // 惰性连接：中台可能比插件晚起来，此刻连不上不该让插件起不来。
    // Channel 自带重连，注册循环的失败重试也就顺带覆盖了「中台后来才起来」。
    let channel = config.hub_endpoint()?.connect_lazy();

    // 状态客户端与网关客户端都和注册**共用这条连接**（与 Go 侧一致）：注册面、
    // 状态面与网关面在同一个端口，多开连接只会多出要维护的 TLS/重连状态。
    //
    // 中台可能在注册回执里不给凭证（例如还没配状态面），那时两个客户端手里的凭证
    // 都是空的，状态/网关调用会拿到 401——这一步不因此失败，插件照常起来、照常处理
    // 业务。
    //
    // 两个客户端共用**同一条** 401 信号通道：撞上 401 的处置是「重新注册换新凭证」，
    // 与它是哪个面的调用无关。通道缓冲 1 + 非阻塞发送，谁先撞上谁留信号，不叠加。
    let (denied_tx, denied_rx) = mpsc::channel::<()>(1);
    let state = Arc::new(StateClient::new(
        HubStateClient::new(channel.clone()),
        denied_tx.clone(),
        config.state_call_timeout,
    ));
    let gateway = Arc::new(GatewayClient::new(
        PluginGatewayClient::new(channel.clone()),
        denied_tx,
        config.gateway_call_timeout,
    ));

    let plugin = Arc::new(plugin);

    let (server_stop_tx, server_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = Server::builder()
        .add_service(PluginRuntimeServer::new(RuntimeService {
            plugin: plugin.clone(),
        }))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
            // 发送端被 drop 时 recv 会立刻返回 Err，这里当成「该停了」——
            // 主流程 panic 或提前返回时，服务不该继续挂着。
            let _ = server_stop_rx.await;
        });
    let mut server_task = tokio::spawn(server);

    log.info(
        "插件 gRPC 已监听",
        &[
            ("listen", Value::Str(&local_addr)),
            ("advertise", Value::Str(&config.advertise_addr)),
        ],
    );
    log.info(
        "插件已启动",
        &[
            ("hub", Value::Str(&config.hub_addr)),
            ("instance", Value::Str(&config.instance_id)),
        ],
    );

    let (stop_tx, stop_rx) = watch::channel(false);
    let mut registrar = Registrar {
        registry: PluginRegistryClient::new(channel.clone()),
        log: log.clone(),
        config: config.clone(),
        manifest,
        descriptor,
        interval: config.heartbeat_fallback_interval,
        last_register: None,
        instance_id: config.instance_id.clone(),
        plugin,
        state: state.clone(),
        gateway: gateway.clone(),
        denied: denied_rx,
    };
    let mut registrar_task = tokio::spawn(async move {
        registrar.run(Stop::new(stop_rx)).await;
    });

    // 三条路，谁先到谁说了算：停止信号、服务异常退出、注册循环意外结束。
    // 注册循环正常情况**永不返回**，所以它先返回一定是出了事（不该发生）。
    let mut shutdown = Box::pin(shutdown);
    tokio::select! {
        _ = &mut shutdown => {
            log.info("收到退出信号，开始优雅退出", &[]);
        }
        result = &mut server_task => {
            return Err(match result {
                Ok(Err(e)) => HubkitError::Serve(e.to_string()),
                Ok(Ok(())) => HubkitError::Serve(
                    // 服务在没收到停止信号时结束了，这是异常
                    "gRPC 服务在收到停止信号前就结束了".to_string(),
                ),
                Err(join) => HubkitError::Serve(format!("gRPC 服务任务异常结束: {join}")),
            });
        }
        _ = &mut registrar_task => {
            return Err(HubkitError::Serve("注册循环意外结束".to_string()));
        }
    }

    // 先让注册循环停下，再注销：反过来的话，一次正好在进行中的注册会把它加回去。
    let _ = stop_tx.send(true);

    // 主动注销：中台据此**立刻**摘掉实例，不必等心跳超时。
    // 注销失败不是致命错误——中台的心跳超时兜底还在。
    unregister(&channel, &config.instance_id, &state, &log).await;

    if tokio::time::timeout(Duration::from_secs(2), &mut registrar_task)
        .await
        .is_err()
    {
        // 注册循环卡在一次 RPC 上。再等下去只会拖长退出时间，直接掐掉——
        // 注销那一步已经走完了（有凭证就是发了、没凭证就是跳过了），
        // 它手里没有需要善后的状态。
        registrar_task.abort();
    }

    let _ = server_stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), server_task).await;

    log.info("插件已退出", &[]);
    Ok(())
}

/// 等 SIGINT / SIGTERM。
///
/// 两个都收：容器编排系统（k8s、docker stop）发的是 **SIGTERM**，而开发机上
/// Ctrl-C 是 SIGINT。只接一个就会出现「本地能停、容器里停不掉」这种只在生产暴露的差异。
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            // 装不上 SIGTERM 处理器（极少见）时退化成只等 Ctrl-C，
            // 而不是直接返回——直接返回会让插件起来就退。
            Err(e) => {
                eprintln!("hubkit: 装 SIGTERM 处理器失败，只等 Ctrl-C: {e}");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// 主动注销。中台据此立刻摘掉实例，不必等心跳超时。
///
/// **没拿到过凭证就整个跳过**。凭证只在注册成功时下发，为空说明本实例压根没进过注册表
/// （注册被拒、或还没注册上就退出了），没有实例行可摘除。而 `instance_id` 是插件自报的、
/// 可以跟别的插件撞（缺省「主机名-PID」，同一 host 网络下容器 PID 又都是 1）——此时发一次
/// 不带身份的注销，中台若只按 `instance_id` 删行，删掉的正是**对方**那一行；对方的心跳仍按
/// `instance_id` 命中、返回 accepted，**完全察觉不到自己从注册表里消失了**（实测：auth 重启
/// 一次，sql-executor 的工具从 MCP 工具面上全部消失，而它自己的日志停在「已注册到中台」
/// 之后再无输出）。中台侧现在会拒（见 `UnregisterRequest.state_token`），插件侧这一道是
/// 别发这个注定被拒的请求。
///
/// 这也决定了**插件先于中台升级**时的行为：旧中台的注册回执里没有这个字段，SDK 拿到的是
/// 空串，于是注销整个跳过——对旧中台也就不再有优雅注销，只能等它心跳超时摘除。这是有意的
/// 取舍：窗口是有界的（中台升级完就恢复），而反过来放行的代价是可能删掉别人的实例行。
///
/// 凭证从 [`StateClient`] 取而不是在 registrar 里另存一份：中台只下发**一份**
/// （`RegisterResponse.state_token`），状态调用与注销认属主用的是同一个东西。
async fn unregister(channel: &Channel, instance_id: &str, state: &StateClient, log: &Logger) {
    let state_token = state.current_token();
    if state_token.is_empty() {
        log.info(
            "本实例没有注册凭证，跳过主动注销（没注册成功就没有可摘除的实例行）",
            &[("instance", Value::Str(instance_id))],
        );
        return;
    }

    let mut client = PluginRegistryClient::new(channel.clone());
    let request = UnregisterRequest {
        instance_id: instance_id.to_string(),
        reason: "插件优雅退出".to_string(),
        state_token,
    };

    match tokio::time::timeout(UNREGISTER_TIMEOUT, client.unregister(request)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => log.warn(
            "注销失败（中台会在心跳超时后自行摘除）",
            &[("err", Value::Owned(e.to_string()))],
        ),
        Err(_) => log.warn(
            "注销超时（中台会在心跳超时后自行摘除）",
            &[("timeout", Value::Str("3s"))],
        ),
    }
}

/// 启动时就把 manifest 的明显问题挡住。
fn check_manifest<P: Plugin>(plugin: &P) -> Result<(), HubkitError> {
    let manifest = plugin.manifest();

    if manifest.name.trim().is_empty() {
        return Err(HubkitError::ManifestMissingName);
    }
    if manifest.version.trim().is_empty() {
        return Err(HubkitError::ManifestMissingVersion);
    }

    // 空 descriptor 本身合法：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的
    // proto。但**声明了自有类型的必须有出处**，否则中台会拒（声明的类型找不到定义）。
    if plugin.descriptor().is_empty() {
        for contract in manifest.produces.iter().chain(manifest.consumes.iter()) {
            if !is_well_known_fq_name(&contract.fq_name) {
                return Err(HubkitError::OwnTypeWithoutDescriptor(
                    contract.fq_name.clone(),
                ));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------- 注册与心跳

/// 停止信号。基于 `watch` 而不是 `Notify`：`watch` 是**水平触发**的——
/// 错过通知不会丢掉这个事实，而 `Notify::notify_waiters` 只唤醒**当时**在等的那些。
/// 插件在「重试等待」与「心跳等待」之间来回切换，用水平触发才不会有窗口期。
struct Stop {
    rx: watch::Receiver<bool>,
}

impl Stop {
    fn new(rx: watch::Receiver<bool>) -> Self {
        Self { rx }
    }

    fn stopped(&self) -> bool {
        *self.rx.borrow()
    }

    /// 等 `d`，或等到停止信号。返回 `true` 表示可以继续，`false` 表示该退出了。
    async fn wait(&mut self, d: Duration) -> bool {
        if self.stopped() {
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(d) => !self.stopped(),
            // 发送端被 drop 也返回 Err —— 主流程没了，注册循环没有存在的理由
            _ = self.rx.changed() => false,
        }
    }

    /// 等一次停止信号（发送端消失也算）。
    ///
    /// 给那些**不止等这一件事**的 select 用（[`Registrar::wait_tick`]）：
    /// 它自己先查 [`Stop::stopped`] 兜住「信号先到、等待后开始」，所以这里不必再查一次。
    async fn changed(&mut self) {
        let _ = self.rx.changed().await;
    }
}

/// 注册与心跳循环。
struct Registrar<P> {
    registry: PluginRegistryClient<Channel>,
    log: Logger,
    config: Config,
    manifest: PluginManifest,
    descriptor: Vec<u8>,
    /// 心跳周期。初始是兜底值，注册成功后换成中台指定的那个。
    interval: Duration,
    /// 最近一次注册成功的时刻。日志要用（「距上次注册多久」是排障时第一个要问的），
    /// 也是「状态凭证被拒」触发的重注册的**速率下限**——没有它，持续被拒会把注册打成自旋。
    last_register: Option<Instant>,
    instance_id: String,
    /// 插件本体。注册成功后要把状态客户端注入给它（[`Plugin::set_state`]），
    /// 而注入是**注册循环**做的事——它需要一个能碰到插件的引用。
    plugin: Arc<P>,
    /// 外置状态客户端。注册成功后先换它的凭证，再注入给插件。
    ///
    /// 退出时的注销也从它取凭证——中台只下发一份状态凭证，注销认属主用的就是它，
    /// 所以不另存一份（见 `unregister`）。它同时被注册循环与本函数持有，因此是 `Arc`。
    state: Arc<StateClient>,
    /// 插件网关客户端。与 [`Registrar::state`] 同一份凭证、同一套注入时机——
    /// 四个网关 RPC 的鉴权与状态面是同一个凭证，不另存第二份。
    gateway: Arc<GatewayClient>,
    /// 状态调用撞上 401 的信号，见 [`StateClient`] 与 [`Registrar::heartbeat_loop`]。
    denied: mpsc::Receiver<()>,
}

/// 心跳循环为什么返回。
#[derive(Debug, PartialEq, Eq)]
enum HeartbeatExit {
    /// 中台要求重新注册（实例可能已被摘除）——这是自愈路径。
    Reregister,
    /// 进程要退出了。
    Shutdown,
}

/// 「等下一拍」等到了什么。
#[derive(Debug, PartialEq, Eq)]
enum Tick {
    /// 该发心跳了。
    Beat,
    /// 进程要退出了。
    Shutdown,
    /// 状态凭证被拒，该重走注册换一张。
    StateDenied,
}

impl<P: Plugin> Registrar<P> {
    /// 维持「注册 → 心跳 → 被摘除则重新注册」的循环，直到收到停止信号。
    async fn run(&mut self, mut stop: Stop) {
        loop {
            match self.register_once().await {
                Ok(()) => {
                    if self.heartbeat_loop(&mut stop).await == HeartbeatExit::Shutdown {
                        return;
                    }
                    // 被要求重注册：**立刻**回到循环顶部，不再等一拍。
                    // 加个延迟在这里会让「实例掉线后自愈」慢一个周期，
                    // 而这段延迟没有任何好处——重注册本身就是被要求做的事。
                }
                Err(RegisterFailure::Rejected(rejections)) => {
                    self.log_rejections(&rejections);
                    if !stop.wait(self.config.register_retry_interval).await {
                        return;
                    }
                }
                Err(RegisterFailure::Transport(err)) => {
                    // 连不上中台、网络抖动这类错误本身就是一条，收尾信息打在同一行里正好。
                    // 拆成两行只会多出一行没有信息量的「稍后重试」。
                    self.log.error(
                        "注册未通过，稍后重试",
                        &[
                            ("err", Value::Owned(err)),
                            (
                                "retry_in",
                                Value::Owned(fmt_duration(self.config.register_retry_interval)),
                            ),
                        ],
                    );
                    if !stop.wait(self.config.register_retry_interval).await {
                        return;
                    }
                }
            }
        }
    }

    /// 把一次注册失败讲成「看一眼就懂」的样子。
    ///
    /// 每**条**原因各占一行，而不是把 N 条塞进一个字段：日志走的是 JSON，
    /// 一整段多行文本会被转义成 `\n` 塞进单个字段——一行里糊着 N 条原因，
    /// 得靠人脑反解析才看得出哪条是哪条。摊成 `code` / `message` / `detail` 三列之后，
    /// 每行本身就是完整的一条，读日志不需要 `jq`，也不需要任何工具。
    ///
    /// 重试间隔单独收尾一行，而不是跟在每条原因后面：它是「接下来会怎样」，
    /// 与「错在哪」不是一回事，逐条重复只会把原因行淹掉。`reasons` 是原因条数，
    /// 用来兜底——日志被截断时，一眼能看出还有几条没打出来。
    fn log_rejections(&self, rejections: &[crate::proto::Rejection]) {
        for r in rejections {
            let code = reject_code_name(r.code);
            self.log.error(
                "中台拒绝了注册",
                &[
                    ("code", Value::Str(&code)),
                    ("message", Value::Str(r.message.as_str())),
                    ("detail", Value::Str(r.detail.as_str())),
                ],
            );
        }
        self.log.error(
            "注册未通过，稍后重试",
            &[
                ("reasons", Value::Uint(rejections.len() as u64)),
                (
                    "retry_in",
                    Value::Owned(fmt_duration(self.config.register_retry_interval)),
                ),
            ],
        );
    }

    async fn register_once(&mut self) -> Result<(), RegisterFailure> {
        let request = RegisterRequest {
            plugin_name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            instance_id: self.instance_id.clone(),
            advertise_addr: self.config.advertise_addr.clone(),
            manifest: Some(self.manifest.clone()),
            descriptor_set: self.descriptor.clone(),
        };

        let response = self
            .registry
            .register(request)
            .await
            .map_err(|e| RegisterFailure::Transport(e.to_string()))?
            .into_inner();

        if !response.accepted {
            return Err(RegisterFailure::Rejected(response.rejections));
        }
        self.last_register = Some(Instant::now());

        // 凭证随每次注册轮换，这里**覆盖**旧的。不做「新凭证为空就留着旧的」这种优化：
        // 中台明确回了空凭证，就是「这个实例现在没有状态凭证」的意思，
        // 留着旧的只会让状态调用拿着一个已经被吊销的凭证去撞 401（那正是我们要的信号）。
        self.state.set_token(&response.state_token);
        // 网关客户端认的是**同一个**凭证，随状态客户端一起换——两个客户端各存一份
        // 引用而非共享一个 token，是沿用了它们各自独立轮换的简单性；同一次注册里
        // 连续两次 set，不存在「换了一半」的中间态。
        self.gateway.set_token(&response.state_token);
        if response.state_token.is_empty() {
            self.log
                .warn("中台未下发状态凭证，HubState 与网关调用将不可用", &[]);
        }
        // 每次注册后都注入一次：插件的 handle 可能还持有上一个凭证时期的客户端引用，
        // 而现在正是「凭证已经换成新的了」这件事发生的那一刻。
        self.plugin.set_state(self.state.clone());
        self.plugin.set_gateway(self.gateway.clone());

        // 中台指定的周期优先于兜底值。为 0 表示中台没给，保持原值。
        if response.heartbeat_interval_seconds > 0 {
            self.interval = Duration::from_secs(response.heartbeat_interval_seconds as u64);
        }
        // 心跳周期是排障时第一个要问的东西（「它到底多久发一拍」），
        // 而它来自中台的回执、不在配置里——不记一笔就得靠抓包。
        self.log.debug(
            "采用中台指定的心跳周期",
            &[
                ("interval_s", Value::Uint(self.interval.as_secs())),
                (
                    "from_hub",
                    Value::Bool(response.heartbeat_interval_seconds > 0),
                ),
            ],
        );

        for warning in &response.warnings {
            self.log
                .warn("中台提示", &[("warning", Value::Str(warning.as_str()))]);
        }

        self.log.info(
            "已注册到中台",
            &[
                ("plugin", Value::Str(&self.manifest.name)),
                ("version", Value::Str(&self.manifest.version)),
                // 用中台回执里的 instance_id 而不是我们请求里那个：中台有权改写它，
                // 打出来的应该是**中台认的那个**，否则对不上号
                ("instance", Value::Str(&response.instance_id)),
            ],
        );
        Ok(())
    }

    /// 等下一拍心跳，或等到停止信号，或等到「状态凭证被拒」的信号。
    async fn wait_tick(&mut self, stop: &mut Stop) -> Tick {
        // 水平触发的停止信号：信号先到、等待后开始也不能丢（与 [`Stop::wait`] 同一条理由）
        if stop.stopped() {
            return Tick::Shutdown;
        }
        let interval = self.interval;
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                if stop.stopped() {
                    Tick::Shutdown
                } else {
                    Tick::Beat
                }
            }
            _ = stop.changed() => Tick::Shutdown,
            // 只认 `Some`：发送端全没了时 `recv` 返回 `None`，那条分支该就此沉默，
            // 而不是把循环当成「又被拒了一次」空转起来
            Some(()) = self.denied.recv() => Tick::StateDenied,
        }
    }

    async fn heartbeat_loop(&mut self, stop: &mut Stop) -> HeartbeatExit {
        loop {
            // 把「等一拍」做成 stop 可打断的等待：否则一次 SIGTERM 要等满一个
            // 心跳周期（中台可以指定成几十秒）才生效。
            // 状态凭证被拒的信号也在这同一个等待里收（见 [`StateClient`]）：
            // 它要与心跳一样「哪怕正在等也立刻响应」，否则一次 401 最多要等一个周期才被处理。
            match self.wait_tick(stop).await {
                Tick::Beat => {}
                Tick::Shutdown => return HeartbeatExit::Shutdown,
                Tick::StateDenied => {
                    // 插件侧的状态调用被中台判了 401：凭证多半已被吊销或轮换。
                    // 心跳本身可能一切正常（实例还在库里），不重注册就会一直哑下去。
                    //
                    // 但必须有速率下限：denial 可能连续不断（中台校验滞后、撤销尚未传播，
                    // 或非凭证原因也回 401——中台侧那条查询失败时会回 500 而不是 401，
                    // 这里防的是「同一条路径以 401 的形式反复回来」）。
                    // 没有它，一次成功注册之后紧接着排空 denial 就是零延迟，
                    // 注册速率等于 Register RPC 的延迟——无限自旋，同时还在反复探测插件自己的地址。
                    // 窗口内到达的 denial 直接丢掉。
                    if let Some(since) = self.last_register.map(|t| t.elapsed()) {
                        if since < self.config.register_retry_interval {
                            self.log.warn(
                                "状态凭证被拒，但距上次注册不足冷却窗口，忽略本次",
                                &[
                                    ("since_register", Value::Owned(fmt_duration(since))),
                                    (
                                        "cooldown",
                                        Value::Owned(fmt_duration(
                                            self.config.register_retry_interval,
                                        )),
                                    ),
                                ],
                            );
                            continue;
                        }
                    }
                    self.log.warn("状态凭证被拒，重新注册以换取新凭证", &[]);
                    return HeartbeatExit::Reregister;
                }
            }

            let request = HeartbeatRequest {
                instance_id: self.instance_id.clone(),
            };
            let response = match self.registry.heartbeat(request).await {
                Ok(r) => r.into_inner(),
                Err(e) => {
                    // 网络抖动不该让插件停止心跳，下一拍继续——
                    // 中台侧的心跳超时窗口远大于一个周期，偶发失败不会导致被摘除。
                    self.log
                        .warn("心跳失败", &[("err", Value::Owned(e.to_string()))]);
                    continue;
                }
            };

            // 中台可以在心跳回执里改周期（例如治理时调高），跟上它
            if response.heartbeat_interval_seconds > 0 {
                self.interval = Duration::from_secs(response.heartbeat_interval_seconds as u64);
            }

            self.log.debug(
                "心跳已送达",
                &[
                    ("interval_s", Value::Uint(self.interval.as_secs())),
                    ("accepted", Value::Bool(response.accepted)),
                ],
            );

            if !response.accepted || response.reregister_required {
                let since = self
                    .last_register
                    .map(|t| fmt_duration(t.elapsed()))
                    .unwrap_or_else(|| "未知".to_string());
                self.log.warn(
                    "中台要求重新注册（实例可能已被摘除）",
                    &[
                        ("accepted", Value::Bool(response.accepted)),
                        (
                            "reregister_required",
                            Value::Bool(response.reregister_required),
                        ),
                        ("since_register", Value::Owned(since)),
                    ],
                );
                return HeartbeatExit::Reregister;
            }
        }
    }
}

enum RegisterFailure {
    /// 中台明确拒绝，带结构化原因。
    Rejected(Vec<crate::proto::Rejection>),
    /// 连不上、超时这一类。
    Transport(String),
}

/// 把时长打成人能读的形式。
///
/// 直接 `Debug` 一个 `Duration` 会得到 `5s` 也可能得到纳秒整数——
/// 日志里出现一个裸数字没人认得出那是几秒。
fn fmt_duration(d: Duration) -> String {
    if d.as_secs() > 0 && d.subsec_millis() == 0 {
        format!("{}s", d.as_secs())
    } else if d.as_secs() == 0 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.3}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 时长按人读得懂的形式打印() {
        assert_eq!(fmt_duration(Duration::from_secs(5)), "5s");
        assert_eq!(fmt_duration(Duration::from_millis(50)), "50ms");
        assert_eq!(fmt_duration(Duration::from_millis(1500)), "1.500s");
    }

    #[test]
    fn manifest_缺项时本地就报错() {
        struct MissingName;
        #[async_trait]
        impl Plugin for MissingName {
            fn manifest(&self) -> PluginManifest {
                PluginManifest {
                    name: String::new(),
                    version: "1.0.0".into(),
                    ..Default::default()
                }
            }
            async fn validate(&self, _e: &Envelope) -> Result<ValidateResponse, PluginError> {
                Ok(ValidateResponse::default())
            }
            async fn handle(&self, e: Envelope) -> Result<Envelope, PluginError> {
                Ok(e)
            }
        }
        assert!(matches!(
            check_manifest(&MissingName),
            Err(HubkitError::ManifestMissingName)
        ));
    }

    #[test]
    fn 声明了自有类型却没有_descriptor_会被拦住() {
        struct OwnType;
        #[async_trait]
        impl Plugin for OwnType {
            fn manifest(&self) -> PluginManifest {
                PluginManifest {
                    name: "demo".into(),
                    version: "1.0.0".into(),
                    produces: vec![crate::proto::MessageContract {
                        fq_name: "wms.v1.Order".into(),
                        description: String::new(),
                    }],
                    ..Default::default()
                }
            }
            async fn validate(&self, _e: &Envelope) -> Result<ValidateResponse, PluginError> {
                Ok(ValidateResponse::default())
            }
            async fn handle(&self, e: Envelope) -> Result<Envelope, PluginError> {
                Ok(e)
            }
        }
        match check_manifest(&OwnType) {
            Err(HubkitError::OwnTypeWithoutDescriptor(fq)) => assert_eq!(fq, "wms.v1.Order"),
            other => panic!("应当报 OwnTypeWithoutDescriptor，实际 {other:?}"),
        }
    }

    #[test]
    fn 只用_struct_载荷的插件没有_descriptor_也合法() {
        struct StructOnly;
        #[async_trait]
        impl Plugin for StructOnly {
            fn manifest(&self) -> PluginManifest {
                PluginManifest {
                    name: "demo".into(),
                    version: "1.0.0".into(),
                    consumes: vec![crate::proto::MessageContract {
                        fq_name: crate::envelope::STRUCT_FQ_NAME.into(),
                        description: String::new(),
                    }],
                    ..Default::default()
                }
            }
            async fn validate(&self, _e: &Envelope) -> Result<ValidateResponse, PluginError> {
                Ok(ValidateResponse::default())
            }
            async fn handle(&self, e: Envelope) -> Result<Envelope, PluginError> {
                Ok(e)
            }
        }
        assert!(check_manifest(&StructOnly).is_ok());
    }

    #[tokio::test]
    async fn 停止信号能打断等待() {
        let (tx, rx) = watch::channel(false);
        let mut stop = Stop::new(rx);

        // 没发信号：等满时长后返回「继续」
        assert!(stop.wait(Duration::from_millis(10)).await);

        tx.send(true).unwrap();
        let t0 = Instant::now();
        assert!(!stop.wait(Duration::from_secs(30)).await);
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "停止信号应当立刻打断等待，实际等了 {:?}",
            t0.elapsed()
        );
    }

    #[tokio::test]
    async fn 停止信号在等待开始前发出也不会丢() {
        // 水平触发：信号先到、等待后开始，也必须立刻返回——
        // 用 Notify 的 notify_waiters 时这里就是一个真实的丢信号窗口。
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();

        let mut stop = Stop::new(rx);
        let t0 = Instant::now();
        assert!(!stop.wait(Duration::from_secs(30)).await);
        assert!(t0.elapsed() < Duration::from_millis(100));
    }
}
