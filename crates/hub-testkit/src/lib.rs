//! 测试夹具：一个可配置的插件，起在真实 gRPC 上。
//!
//! 各层的集成测试都要「有一个插件跑起来」这件事——中台侧的注册、探测、调用、编排
//! 都只能在真实链路（而不是假的 trait 实现）上验。把它做成夹具，测试里就只剩断言。
//!
//! 夹具刻意做成**可配置**：拒绝前缀、空响应这类异常行为是测试的主角，不该被藏在
//! 每个测试各自的插件实现里。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hub_proto::v1::plugin_runtime_server::{PluginRuntime, PluginRuntimeServer};
use hub_proto::v1::{
    DescribeRequest, Envelope, HandleRequest, HandleResponse, HealthRequest, HealthResponse,
    MessageContract, PluginManifest, RegisterRequest, Severity, ToolDecl, ValidateRequest,
    ValidateResponse, ValidationIssue,
};
use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

/// 夹具插件生产/消费的消息类型。
pub const MESSAGE_FQ: &str = "test.v1.Ping";

/// 插件行为配置。
#[derive(Debug, Clone)]
pub struct Behavior {
    pub plugin_name: String,
    pub version: String,
    pub description: String,

    /// `message_id` 以此前缀开头时，校验器拒绝。
    ///
    /// 用来验证「校验不通过时链路短路、插件体零执行」。
    pub reject_prefix: Option<String>,

    /// `Handle` 返回空信封，模拟不合规的插件。
    pub empty_response: bool,

    /// 前 N 次 `Handle` 调用直接失败。
    ///
    /// 用来验证节点重试：让第一次失败、第二次成功。
    pub fail_first: usize,

    /// `Handle` 每次的固定延迟。
    ///
    /// 用来验证节点超时与 deadline 逐跳递减。
    pub delay: Option<Duration>,

    /// 当 auth 插件用：`cookie` 里出现哪个子串，就给出哪一档权限位。
    ///
    /// 真实的 auth 插件是拿 Cookie 去问 SSO；这里按配置直接回答。被测的是**中台
    /// 那一侧**怎么用它——权限位认不认、缺位时拒不拒、插件挂了会不会降级成匿名。
    ///
    /// 第一项匹配上就用它；都没匹配上返回 `authenticated: false`。
    /// `None`（默认）表示这个夹具不当 auth 插件用，走原来的回显。
    pub auth_scopes: Option<Vec<(String, Vec<String>)>>,

    /// 这个夹具在 manifest 里声明的 MCP 工具。
    ///
    /// 默认空：大多数用例不关心工具声明。需要验「中台把插件工具聚合进工具面」的
    /// 用例才填它——那份聚合读的是注册时落库的声明，所以必须真的声明出来。
    pub tools: Vec<ToolDecl>,

    /// 设了就**原样回传这份载荷**（替代回显）——模拟任何「返回固定结果」的插件，
    /// 比如以固定身份应答的 auth 替身。
    pub respond_payload: Option<serde_json::Value>,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            plugin_name: "echo-plugin".to_string(),
            version: "1.0.0".to_string(),
            description: "测试用回显插件".to_string(),
            reject_prefix: Some("bad-".to_string()),
            empty_response: false,
            fail_first: 0,
            delay: None,
            auth_scopes: None,
            tools: Vec::new(),
            respond_payload: None,
        }
    }
}

impl Behavior {
    pub fn named(name: &str, version: &str) -> Self {
        Self {
            plugin_name: name.to_string(),
            version: version.to_string(),
            ..Default::default()
        }
    }

    /// 这份行为对应的 manifest。
    ///
    /// 挂在 `Behavior` 上（而非只在 `TestPlugin` 上）是为了让**不起插件进程**的测试
    /// 也能拿到一份合法的注册材料：中台侧只关心「注册时提交了什么」，不关心提交方
    /// 是不是真的在监听端口。需要真链路时用 [`start`]。
    pub fn manifest(&self) -> PluginManifest {
        PluginManifest {
            name: self.plugin_name.clone(),
            version: self.version.clone(),
            description: self.description.clone(),
            owner: "testkit".to_string(),
            // 既消费又生产同一个类型：链路上的节点本来就是这个形状（收上游的、
            // 吐给下游的）。只声明一半会让「上下游契约能否接上」那条校验永远失败。
            consumes: vec![MessageContract {
                fq_name: MESSAGE_FQ.to_string(),
                description: String::new(),
            }],
            produces: vec![MessageContract {
                fq_name: MESSAGE_FQ.to_string(),
                description: String::new(),
            }],
            tools: self.tools.clone(),
            ..Default::default()
        }
    }

    /// 一份自洽的 descriptor：包含 `MESSAGE_FQ` 对应的消息。
    pub fn descriptor(&self) -> Vec<u8> {
        let (package, message) = MESSAGE_FQ.rsplit_once('.').unwrap_or(("test", MESSAGE_FQ));

        FileDescriptorSet {
            file: vec![FileDescriptorProto {
                name: Some("testkit.proto".to_string()),
                package: Some(package.to_string()),
                message_type: vec![DescriptorProto {
                    name: Some(message.to_string()),
                    field: vec![FieldDescriptorProto {
                        name: Some("text".to_string()),
                        number: Some(1),
                        label: Some(Label::Optional as i32),
                        r#type: Some(Type::String as i32),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
        .encode_to_vec()
    }
}

/// 夹具插件本体。
///
/// 可 `Clone`，且克隆体共享同一份计数与「最后收到的信封」——tonic 的服务端要拿走所有权，
/// 夹具侧靠克隆保留观察点。
#[derive(Clone)]
pub struct TestPlugin {
    behavior: Behavior,
    handled: Arc<AtomicUsize>,
    last_envelope: Arc<Mutex<Option<Envelope>>>,
}

impl TestPlugin {
    fn new(behavior: Behavior) -> Self {
        Self {
            behavior,
            handled: Arc::new(AtomicUsize::new(0)),
            last_envelope: Arc::new(Mutex::new(None)),
        }
    }

    pub fn manifest(&self) -> PluginManifest {
        self.behavior.manifest()
    }

    /// 一份自洽的 descriptor：包含 `MESSAGE_FQ` 对应的消息。
    pub fn descriptor(&self) -> Vec<u8> {
        self.behavior.descriptor()
    }
}

#[tonic::async_trait]
impl PluginRuntime for TestPlugin {
    async fn describe(
        &self,
        _request: Request<DescribeRequest>,
    ) -> Result<Response<PluginManifest>, Status> {
        Ok(Response::new(self.manifest()))
    }

    async fn validate(
        &self,
        request: Request<ValidateRequest>,
    ) -> Result<Response<ValidateResponse>, Status> {
        let envelope = request.into_inner().envelope.unwrap_or_default();

        if let Some(prefix) = &self.behavior.reject_prefix
            && envelope.message_id.starts_with(prefix)
        {
            return Ok(Response::new(ValidateResponse {
                valid: false,
                issues: vec![ValidationIssue {
                    path: "message_id".to_string(),
                    message: format!("不接受 {prefix} 前缀"),
                    severity: Severity::Error as i32,
                }],
            }));
        }

        Ok(Response::new(ValidateResponse {
            valid: true,
            issues: Vec::new(),
        }))
    }

    async fn handle(
        &self,
        request: Request<HandleRequest>,
    ) -> Result<Response<HandleResponse>, Status> {
        let attempt = self.handled.fetch_add(1, Ordering::SeqCst) + 1;

        let mut envelope = request
            .into_inner()
            .envelope
            .ok_or_else(|| Status::invalid_argument("缺少信封"))?;

        // 失败也要记下来：测试常要断言「失败的那次收到的信封是什么样」
        *self.last_envelope.lock().expect("锁中毒") = Some(envelope.clone());

        if let Some(delay) = self.behavior.delay {
            tokio::time::sleep(delay).await;
        }

        // 模拟「前几次调用失败」——用来验证节点重试
        if attempt <= self.behavior.fail_first {
            return Err(Status::unavailable(format!(
                "{} 第 {attempt} 次调用按配置失败",
                self.behavior.plugin_name
            )));
        }

        // auth 插件模式：按 cookie 回答「这个凭证是谁、有哪几个权限位」。
        // 真实插件是去问 SSO，这里按配置直接答——被测的是中台那一侧怎么用它
        if let Some(rules) = self.behavior.auth_scopes.clone() {
            let cookie = envelope
                .payload
                .as_ref()
                .and_then(hub_proto::decode_payload)
                .and_then(|payload| {
                    payload
                        .get("cookie")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();

            let body = match rules
                .iter()
                .find(|(needle, _)| cookie.contains(needle.as_str()))
            {
                Some((_, scopes)) => serde_json::json!({
                    "authenticated": true,
                    "userCode": "u-1",
                    "userName": "测试用户",
                    "scopes": scopes,
                }),
                None => serde_json::json!({
                    "authenticated": false,
                    "reason": "测试夹具不认识这个凭证",
                }),
            };

            envelope.payload = hub_proto::encode_payload(&body).ok();
            return Ok(Response::new(HandleResponse {
                envelope: Some(envelope),
            }));
        }

        if self.behavior.empty_response {
            return Ok(Response::new(HandleResponse { envelope: None }));
        }

        if let Some(payload) = self.behavior.respond_payload.clone() {
            envelope.payload = hub_proto::encode_payload(&payload).ok();
            return Ok(Response::new(HandleResponse {
                envelope: Some(envelope),
            }));
        }

        envelope
            .meta
            .insert("handled_by".to_string(), self.behavior.plugin_name.clone());
        envelope
            .meta
            .insert("attempt".to_string(), attempt.to_string());

        Ok(Response::new(HandleResponse {
            envelope: Some(envelope),
        }))
    }

    type HandleStreamStream = ReceiverStream<Result<HandleResponse, Status>>;

    async fn handle_stream(
        &self,
        _request: Request<HandleRequest>,
    ) -> Result<Response<Self::HandleStreamStream>, Status> {
        Err(Status::unimplemented("流式处理尚未实现"))
    }

    async fn health(
        &self,
        _request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        Ok(Response::new(HealthResponse {
            healthy: true,
            message: "ok".to_string(),
        }))
    }
}

/// 已启动的夹具。
pub struct Fixture {
    pub addr: SocketAddr,
    plugin: TestPlugin,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Fixture {
    /// 插件的中台可达地址。
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 插件体被执行过几次。用来断言「校验失败时插件体没被调用」。
    pub fn handled_count(&self) -> usize {
        self.plugin.handled.load(Ordering::SeqCst)
    }

    /// 插件最后一次收到的信封。用来断言 deadline、meta 等是否透传到位。
    pub fn last_envelope(&self) -> Option<Envelope> {
        self.plugin.last_envelope.lock().expect("锁中毒").clone()
    }

    pub fn manifest(&self) -> PluginManifest {
        self.plugin.manifest()
    }

    pub fn descriptor(&self) -> Vec<u8> {
        self.plugin.descriptor()
    }

    /// 一份可直接投给中台的注册请求。
    pub fn register_request(&self) -> RegisterRequest {
        let manifest = self.manifest();
        RegisterRequest {
            plugin_name: manifest.name.clone(),
            version: manifest.version.clone(),
            instance_id: format!("instance-{}", manifest.name),
            advertise_addr: self.base_url(),
            manifest: Some(manifest),
            descriptor_set: self.descriptor(),
        }
    }

    /// 换个版本号的注册请求（用于验证多版本共存与升级）。
    pub fn register_request_for_version(&self, version: &str) -> RegisterRequest {
        let mut request = self.register_request();
        if let Some(manifest) = request.manifest.as_mut() {
            manifest.version = version.to_string();
        }
        request.version = version.to_string();
        request.instance_id = format!("instance-{}", version);
        request
    }

    /// 停止插件并等到端口真的不可连为止。
    pub async fn stop(mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(self.addr).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("插件在 2 秒内没有停下来");
    }
}

/// 起一个夹具插件，监听在随机端口上。
pub async fn start(behavior: Behavior) -> Fixture {
    let plugin = TestPlugin::new(behavior);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定插件端口失败");
    let addr = listener.local_addr().expect("取插件地址失败");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let serving = plugin.clone();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(PluginRuntimeServer::new(serving))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async move {
                    let _ = rx.await;
                },
            )
            .await;
    });

    Fixture {
        addr,
        plugin,
        shutdown: Some(tx),
    }
}

/// 默认行为的夹具。
pub async fn start_default() -> Fixture {
    start(Behavior::default()).await
}
