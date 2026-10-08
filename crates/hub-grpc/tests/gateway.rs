//! `PluginGateway`（插件互调与发现面）的集成测试：真 PostgreSQL + 真 Redis + 真插件。
//!
//! 九个场景与设计文档 §7 一一对应，外加一条凭证负向用例。这一层要验的同样是
//! 「真的连上去会怎样」：互调走的是注册表解析出的**真实实例地址**，配额记在共享的
//! Redis 上，审计 span 落在真表里——任何一环用假实现替掉，测的都是另一个系统。
//!
//! 夹具没有复用 `tests/common`：那份装配的是「注册面 + 状态面」，而这里被测的是
//! 网关面——同一个 Registry 上要挂带 Invoker 的 GatewayService，还可能换策略与
//! 配额，装配逻辑不同；硬塞进公共夹具会让两边都多出一堆对方用不到的旋钮。
//! Redis 键隔离约定与 common 一致：默认 db 15，`TEST_REDIS_URL` 只用来在 CI 里
//! 换端口。
//!
//! 每个用例的插件名都带场景前缀（`disc-` / `call-` / `ring-`…）：`#[sqlx::test]`
//! 只隔离数据库，Redis 是整个 suite 共享的，而互调配额**按插件名记账**——重名会
//! 让一个用例的调用次数记到另一个用例头上。

use std::sync::Arc;

use chrono::Utc;
use hub_engine::Invoker;
use hub_grpc::gateway::{CALL_CHAIN_META, INVOKE_QUOTA_KEY_PREFIX};
use hub_grpc::state::STATE_TOKEN_METADATA;
use hub_grpc::{CallPolicy, GatewayService};
use hub_plugin_client::PluginClient;
use hub_proto::v1::plugin_gateway_client::PluginGatewayClient;
use hub_proto::v1::plugin_gateway_server::PluginGatewayServer;
use hub_proto::v1::plugin_registry_client::PluginRegistryClient;
use hub_proto::v1::plugin_registry_server::PluginRegistryServer;
use hub_proto::v1::{
    DescribeMessageRequest, Envelope, GetContractRequest, InvokeOutcome, InvokeRequest,
    InvokeResponse, ListPluginsRequest, PayloadType, RegisterRequest, Subject, SubjectKind,
};
use hub_registry::probe::AlwaysHealthy;
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::json;
use sqlx::PgPool;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};
use hub_testkit::{Behavior, Fixture, MESSAGE_FQ};

/// 夹具句柄。
///
/// `pool` / `redis` 是留给用例的**直连句柄**：审计 span 要查表、配额键要先清，
/// 这些都得绕过 gRPC 摸到底层事实。
struct Ctx {
    pool: PgPool,
    redis: ConnectionManager,
    addr: String,
}

/// 起一个带 Invoker 的默认网关（策略放行、配额默认、下游代调已装配）。
///
/// 生产里不该出现的配置不做成测试默认值——所以「未装配 Invoker」那条用例
/// 单独走 [`setup_gateway`] 显式关掉。
async fn setup(pool: PgPool) -> Ctx {
    setup_gateway(pool, None, CallPolicy::Allow, true).await
}

/// 与 [`setup`] 相同，但配额、策略、是否装配 Invoker 都能调。
///
/// 三个口子各对应一条用例要验的「部署形态」：配额调小（验打满）、Declared
/// 策略（验权限）、不装 Invoker（验 mock 中台的替身语义）。
async fn setup_gateway(
    pool: PgPool,
    quota: Option<i64>,
    policy: CallPolicy,
    with_invoker: bool,
) -> Ctx {
    let store = Store::from_pool(pool.clone());

    let redis_url =
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/15".to_string());
    // 与生产装配同一入口建连：响应超时（redis-rs 默认 500ms 会误杀正常请求）
    // 只能在建连时给，见 `state::connect_redis` 的说明。
    let redis = hub_grpc::state::connect_redis(&redis_url)
        .await
        .expect("连 Redis 失败——本机 6379 上应有 Redis");

    // 探针恒通过：注册能不能成不该卡在可达性探测上（与 tests/common 同一取舍）；
    // 互调的「真的能调到」由真夹具进程承担，不靠探针装样子。
    let registry = Registry::new(
        store.clone(),
        Arc::new(AlwaysHealthy) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );

    // 监听随机端口：每个用例一个独立服务，互不抢地址
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("监听失败");
    let addr = listener.local_addr().expect("取地址失败");

    let mut gateway = GatewayService::new(store.pool().clone(), redis.clone()).with_call_policy(policy);
    if with_invoker {
        // 与 flow / Publish 共用同一条 Invoker 链路——网关代调不该另起炉灶，
        // 否则「Invoke 里发生了什么」和「flow 节点里发生了什么」会是两套行为
        let client = PluginClient::new(Default::default());
        gateway = gateway.with_invoker(Invoker::new(registry.clone(), client));
    }
    if let Some(quota) = quota {
        gateway = gateway.with_invoke_quota(quota);
    }

    let reg_svc = hub_grpc::RegistryService::new(registry);
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(PluginRegistryServer::new(reg_svc))
            .add_service(PluginGatewayServer::new(gateway))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
    });

    Ctx {
        pool,
        redis,
        addr: format!("http://{addr}"),
    }
}

impl Ctx {
    async fn gateway_client(&self) -> PluginGatewayClient<Channel> {
        PluginGatewayClient::connect(self.addr.clone())
            .await
            .expect("连网关面失败")
    }

    /// 走完整注册流程登记一个插件，返回中台下发的状态凭证。
    ///
    /// `advertise` 填夹具的真实地址（下游要被真调）；纯调用方不起进程，填一个
    /// 死地址即可——凭证来自注册落库，不依赖地址可达。`invokes` 非空时写进
    /// manifest 的调用声明（Declared 策略读的就是它）；要**改**声明必须换版号，
    /// 同版本号改 manifest 会被注册面按版本冲突拒掉。
    async fn register_plugin(
        &self,
        name: &str,
        version: &str,
        advertise: &str,
        invokes: &[&str],
    ) -> String {
        let mut client = PluginRegistryClient::connect(self.addr.clone())
            .await
            .expect("连注册面失败");

        let behavior = Behavior::named(name, version);
        let mut manifest = behavior.manifest();
        manifest.invokes = invokes.iter().map(|s| s.to_string()).collect();

        let resp = client
            .register(RegisterRequest {
                plugin_name: name.to_string(),
                version: version.to_string(),
                instance_id: format!("{name}-i1"),
                advertise_addr: advertise.to_string(),
                manifest: Some(manifest),
                descriptor_set: behavior.descriptor(),
            })
            .await
            .expect("注册调用失败")
            .into_inner();
        assert!(
            resp.accepted,
            "注册应被接受，拒绝原因: {:?}",
            resp.rejections
        );
        resp.state_token
    }
}

/// 起一个回显夹具（真进程、随机端口）。
async fn echo_fixture(name: &str) -> Fixture {
    hub_testkit::start(Behavior::named(name, "1.0.0")).await
}

/// 给请求挂上状态凭证。
///
/// 与 `common::with_token` 同款而不是共享它：本文件的夹具装的是网关面，
/// 借 common 会把两个测试目标的死代码告警搅在一起。凭证挂在哪个 metadata
/// 键上是这一层的契约，键名从 `hub_grpc::state` 取常量，不手写字面量。
fn with_token<T>(mut req: Request<T>, token: &str) -> Request<T> {
    req.metadata_mut().insert(
        STATE_TOKEN_METADATA,
        MetadataValue::try_from(token).expect("凭证应是合法 header 值"),
    );
    req
}

/// 发一次互调并拆出业务应答。
///
/// 拆平「挂凭证 → 调用 → 取响应体」的样板；gRPC 层的错误原样上抛，让用例
/// 自己决定它是预期（场景 9 的 unavailable）还是意外。
async fn invoke(
    client: &mut PluginGatewayClient<Channel>,
    token: &str,
    request: InvokeRequest,
) -> Result<InvokeResponse, Status> {
    client
        .invoke(with_token(Request::new(request), token))
        .await
        .map(Response::into_inner)
}

/// 带载荷与 60 秒预算的请求信封。subject 故意填一个**假身份**：
///
/// 「中台必须把 subject 覆盖成真实调用方、自报不算数」正是要验的契约之一——
/// 得先给它一个可以证伪的值，断言才有牙齿。
fn invoke_envelope(message_id: &str, payload: &serde_json::Value) -> Envelope {
    let mut envelope = Envelope {
        message_id: message_id.to_string(),
        deadline_ms: Utc::now().timestamp_millis() + 60_000,
        r#type: PayloadType::Request as i32,
        subject: Some(Subject {
            kind: SubjectKind::Plugin as i32,
            id: "自报的假身份".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };
    envelope.payload = hub_proto::encode_payload(payload).ok();
    envelope
}

/// 场景 1：发现三件套 happy path。
///
/// 「能调谁、谁生产这个消息、它吃什么吐什么」全部从中台取——事实源就是注册表，
/// 发现 RPC 只是把既有查询翻成插件够得着的 gRPC，不另立数据。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 发现三件套_清单_消息端点与字段级_schema(pool: PgPool) {
    let ctx = setup(pool).await;
    let a = echo_fixture("disc-a").await;
    let b = echo_fixture("disc-b").await;
    // a 带调用声明、b 不带：GetContract 的 invokes 字段两种形态都验到
    let token = ctx
        .register_plugin("disc-a", "1.0.0", &a.base_url(), &["disc-b"])
        .await;
    ctx.register_plugin("disc-b", "1.0.0", &b.base_url(), &[])
        .await;

    let mut client = ctx.gateway_client().await;

    // ---- ListPlugins：库是本用例独占的，清单里就该只有这两个 ----
    let resp = client
        .list_plugins(with_token(
            Request::new(ListPluginsRequest {
                include_offline: false,
            }),
            &token,
        ))
        .await
        .expect("list_plugins 应可调用")
        .into_inner();
    assert_eq!(resp.plugins.len(), 2, "独占库里只有本用例注册的两个插件");
    let summary = resp
        .plugins
        .iter()
        .find(|p| p.name == "disc-a")
        .expect("清单里应有 disc-a");
    assert!(summary.online, "有健康实例的插件算在线");
    assert_eq!(summary.latest_version, "1.0.0");
    assert_eq!(summary.instance_count, 1, "一个夹具就是一个健康实例");

    // ---- DescribeMessage：两个夹具都声明生产并消费同一消息 ----
    let resp = client
        .describe_message(with_token(
            Request::new(DescribeMessageRequest {
                fq_name: MESSAGE_FQ.to_string(),
            }),
            &token,
        ))
        .await
        .expect("describe_message 应可调用")
        .into_inner();
    for direction in [&resp.producers, &resp.consumers] {
        for name in ["disc-a", "disc-b"] {
            let endpoint = direction
                .iter()
                .find(|e| e.plugin == name)
                .unwrap_or_else(|| panic!("{name} 应在消息端点里"));
            assert_eq!(endpoint.version, "1.0.0", "端点要带版本，多版本共存时才查得清");
        }
    }

    // ---- GetContract：整份契约（不带 fq_name，schema 留空）----
    let resp = client
        .get_contract(with_token(
            Request::new(GetContractRequest {
                plugin: "disc-b".to_string(),
                version: String::new(),
                fq_name: String::new(),
            }),
            &token,
        ))
        .await
        .expect("get_contract 应可调用")
        .into_inner();
    assert_eq!(resp.name, "disc-b");
    assert_eq!(resp.version, "1.0.0", "版本留空取最新登记版");
    assert!(resp.produces.iter().any(|c| c.fq_name == MESSAGE_FQ));
    assert!(resp.consumes.iter().any(|c| c.fq_name == MESSAGE_FQ));
    assert!(resp.invokes.is_empty(), "b 没声明互调，名单应为空");
    assert!(resp.schema_json.is_empty(), "没指定 fq_name 就不摊平 schema");

    // ---- GetContract：带 fq_name，摊平字段级 schema，并带出 invokes 声明 ----
    let resp = client
        .get_contract(with_token(
            Request::new(GetContractRequest {
                plugin: "disc-a".to_string(),
                version: String::new(),
                fq_name: MESSAGE_FQ.to_string(),
            }),
            &token,
        ))
        .await
        .expect("get_contract 应可调用")
        .into_inner();
    assert_eq!(
        resp.invokes,
        vec!["disc-b".to_string()],
        "manifest 声明了谁，契约就报谁——人工审计依赖边靠它"
    );
    let schema: serde_json::Value =
        serde_json::from_str(&resp.schema_json).expect("schema_json 应是 JSON");
    assert_eq!(schema["fq_name"], MESSAGE_FQ);
    assert_eq!(
        schema["fields"][0]["name"], "text",
        "夹具的消息只有一个 text 字段，摊平后要看得见"
    );

    // ---- 查无此插件：not_found，而不是一份空契约 ----
    let err = client
        .get_contract(with_token(
            Request::new(GetContractRequest {
                plugin: "disc-ghost".to_string(),
                ..Default::default()
            }),
            &token,
        ))
        .await
        .expect_err("查无此插件该报 not_found");
    assert_eq!(err.code(), Code::NotFound, "实际：{err}");
}

/// 场景 2：A→B 互调成功。
///
/// B 收到的信封要满足中台代调的全部契约：subject 是**真实调用方**（信封自报的
/// 假身份被覆盖）、载荷原样、链里追加的是 caller 而**不是** target——链的语义是
/// 「已处理过该消息的插件序列」，target 要等它自己被调到时才入链。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 互调成功_下游收到调用方身份_原样载荷与调用链(pool: PgPool) {
    let ctx = setup(pool).await;
    let a = echo_fixture("call-a").await;
    let b = echo_fixture("call-b").await;
    let token = ctx
        .register_plugin("call-a", "1.0.0", &a.base_url(), &[])
        .await;
    ctx.register_plugin("call-b", "1.0.0", &b.base_url(), &[])
        .await;

    let payload = json!({ "text": "ping" });
    let request = InvokeRequest {
        plugin: "call-b".to_string(),
        version: String::new(),
        envelope: Some(invoke_envelope("call-e2e-1", &payload)),
        timeout_ms: 5_000,
    };

    let mut client = ctx.gateway_client().await;
    let resp = invoke(&mut client, &token, request)
        .await
        .expect("invoke 应可调用");
    assert_eq!(
        resp.outcome,
        InvokeOutcome::Handled as i32,
        "应答：{}",
        resp.reason
    );

    // 回来的信封：meta 里同时有链（中台拼的）与 handled_by（B 拼的）——
    // 两段痕迹都在，才说明这趟真经过了双方
    let echoed = resp.envelope.expect("HANDLED 必须带下游返回的信封");
    assert_eq!(echoed.meta["hub.call_chain"], "call-a");
    assert_eq!(echoed.meta["handled_by"], "call-b");
    assert_eq!(
        echoed.message_id, "call-e2e-1",
        "message_id 是数据幂等键，中台不动它"
    );

    // B 实际收到的信封：身份、载荷、链三条对着验
    let received = b.last_envelope().expect("B 应收到信封");
    let subject = received.subject.expect("subject 必须有值");
    assert_eq!(
        subject.id, "call-a",
        "身份必须是真实调用方，不是信封自报的那个"
    );
    assert_eq!(subject.kind, SubjectKind::Plugin as i32);
    assert_eq!(
        hub_proto::decode_payload(received.payload.as_ref().expect("应带载荷"))
            .expect("载荷应可解"),
        payload,
        "载荷要原样到达下游"
    );
    assert_eq!(
        received.meta["hub.call_chain"], "call-a",
        "链里是 caller，不是 target"
    );
}

/// 场景 3：`policy=Declared`。
///
/// 未声明 → ERROR（**业务结果**，不是 gRPC 错误——该做的是去补声明，不是重试）；
/// 换版号带上声明后放行。声明只能靠升版号补：同版本号改 manifest 会被注册面
/// 按版本冲突拒掉，这正是要走「升级」这条真实的路而不是测试后门的原因。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn declared_策略_未声明拒绝_声明后放行(pool: PgPool) {
    let ctx = setup_gateway(pool, None, CallPolicy::Declared, true).await;
    let a = echo_fixture("decl-a").await;
    let b = echo_fixture("decl-b").await;
    let token = ctx
        .register_plugin("decl-a", "1.0.0", &a.base_url(), &[])
        .await;
    ctx.register_plugin("decl-b", "1.0.0", &b.base_url(), &[])
        .await;

    let mut client = ctx.gateway_client().await;
    let call = |message_id: &str| InvokeRequest {
        plugin: "decl-b".to_string(),
        version: String::new(),
        envelope: Some(invoke_envelope(message_id, &json!({ "text": "hi" }))),
        timeout_ms: 5_000,
    };

    // 未声明：拒，且插件体一次都不该被执行
    let resp = invoke(&mut client, &token, call("decl-1"))
        .await
        .expect("invoke 应可调用");
    assert_eq!(resp.outcome, InvokeOutcome::Error as i32);
    assert!(
        resp.reason.contains("未声明"),
        "原因要说清是授权问题：{}",
        resp.reason
    );
    assert_eq!(b.handled_count(), 0, "被策略拦下就不该惊动下游");

    // 升版号带上声明：同一个插件名、新版本、新凭证——实例注册哪一版，
    // 策略就查哪一版的 manifest（「谁在调用就查谁的自述」）
    let declared_token = ctx
        .register_plugin("decl-a", "1.1.0", &a.base_url(), &["decl-b"])
        .await;
    let resp = invoke(&mut client, &declared_token, call("decl-2"))
        .await
        .expect("invoke 应可调用");
    assert_eq!(
        resp.outcome,
        InvokeOutcome::Handled as i32,
        "声明过的目标应当放行：{}",
        resp.reason
    );
    assert_eq!(b.handled_count(), 1, "放行的这次要真的到达下游");
}

/// 场景 4：成环拒绝。
///
/// 链里已经有 caller 了，说明它正在处理这条消息又想调回来——A→B→A 深度才 2
/// 就已经是环，只数深度会放过它，所以环检测看的是链上的名字。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 成环拒绝_调用方已在链上(pool: PgPool) {
    let ctx = setup(pool).await;
    let a = echo_fixture("ring-a").await;
    let b = echo_fixture("ring-b").await;
    let token = ctx
        .register_plugin("ring-a", "1.0.0", &a.base_url(), &[])
        .await;
    ctx.register_plugin("ring-b", "1.0.0", &b.base_url(), &[])
        .await;

    // A→B→A 的后半跳：A 把自己收到的信封（链上已有 A）原样传给下一次调用
    let mut envelope = invoke_envelope("ring-1", &json!({ "text": "ping" }));
    envelope
        .meta
        .insert(CALL_CHAIN_META.to_string(), "ring-a".to_string());

    let mut client = ctx.gateway_client().await;
    let resp = invoke(
        &mut client,
        &token,
        InvokeRequest {
            plugin: "ring-b".to_string(),
            version: String::new(),
            envelope: Some(envelope),
            timeout_ms: 5_000,
        },
    )
    .await
    .expect("invoke 应可调用");
    assert_eq!(resp.outcome, InvokeOutcome::Error as i32);
    assert!(
        resp.reason.contains("互调环"),
        "原因要点明是环：{}",
        resp.reason
    );
    assert_eq!(b.handled_count(), 0, "成环调用不该惊动下游");
}

/// 场景 5：链深上限。
///
/// 预填 8 段（恰好等于 `MAX_INVOKE_DEPTH`），本次 caller 入链后是第 9 跳——
/// 这一闸挡的是「没有重复节点却无限接力」的链，环检测管不住它。顺手钉住边界：
/// 7 段 + 本次 caller = 8，正好顶到上限，应当放行。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 链深到上限后拒绝_边界内放行(pool: PgPool) {
    let ctx = setup(pool).await;
    let a = echo_fixture("deep-a").await;
    let b = echo_fixture("deep-b").await;
    let token = ctx
        .register_plugin("deep-a", "1.0.0", &a.base_url(), &[])
        .await;
    ctx.register_plugin("deep-b", "1.0.0", &b.base_url(), &[])
        .await;

    let mut client = ctx.gateway_client().await;
    let call = |message_id: &str, chain: &str| {
        let mut envelope = invoke_envelope(message_id, &json!({ "text": "ping" }));
        envelope
            .meta
            .insert(CALL_CHAIN_META.to_string(), chain.to_string());
        InvokeRequest {
            plugin: "deep-b".to_string(),
            version: String::new(),
            envelope: Some(envelope),
            timeout_ms: 5_000,
        }
    };

    // 8 段互不相同的名字：+ 本次 caller 是第 9 跳，拒
    let resp = invoke(
        &mut client,
        &token,
        call("deep-1", "n1,n2,n3,n4,n5,n6,n7,n8"),
    )
    .await
    .expect("invoke 应可调用");
    assert_eq!(resp.outcome, InvokeOutcome::Error as i32);
    assert!(
        resp.reason.contains("上限"),
        "原因要说清是撞了深度：{}",
        resp.reason
    );
    assert_eq!(b.handled_count(), 0, "超限调用不该惊动下游");

    // 边界内：7 段 + 本次 caller = 8，放行
    let resp = invoke(&mut client, &token, call("deep-2", "n1,n2,n3,n4,n5,n6,n7"))
        .await
        .expect("invoke 应可调用");
    assert_eq!(
        resp.outcome,
        InvokeOutcome::Handled as i32,
        "顶到上限（含本次 caller 恰好 8 跳）不该误伤：{}",
        resp.reason
    );
}

/// 场景 6：配额打满。
///
/// 夹具把配额调到 2，三发即满——不必真发 61 条，也就不用把「配额是多少」这个
/// 实现细节焊进测试。超额是**业务结果**（outcome=ERROR），且按插件维度记账：
/// 一个插件刷不出别人的额度。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 配额打满后拒绝且只影响自己(pool: PgPool) {
    let ctx = setup_gateway(pool, Some(2), CallPolicy::Allow, true).await;
    let b = echo_fixture("quota-b").await;
    ctx.register_plugin("quota-b", "1.0.0", &b.base_url(), &[])
        .await;
    // 调用方不需要真进程：它的凭证来自注册，互调里中台只对它记账、不回调它
    let token = ctx
        .register_plugin("quota-a", "1.0.0", "http://127.0.0.1:9", &[])
        .await;

    // db 15 全 suite 共享：上一轮运行在同一分钟窗口留下的计数会顶掉
    // 「前两次在额度内」。先把本插件名当前与下一个窗口的键删掉，用例才可重复。
    let window = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时钟不该早于 1970")
        .as_secs()
        / 60;
    let mut conn = ctx.redis.clone();
    for w in [window, window + 1] {
        let key = format!("{INVOKE_QUOTA_KEY_PREFIX}:quota-a:{w}");
        let _: redis::RedisResult<i64> = conn.del(&key).await; // 键不存在也是正常形态
    }

    let mut client = ctx.gateway_client().await;
    let call = |message_id: &str| InvokeRequest {
        plugin: "quota-b".to_string(),
        version: String::new(),
        envelope: Some(invoke_envelope(message_id, &json!({ "text": "ping" }))),
        timeout_ms: 5_000,
    };

    // 前两次在额度内
    for i in 1..=2 {
        let resp = invoke(&mut client, &token, call(&format!("quota-{i}")))
            .await
            .expect("invoke 应可调用");
        assert_eq!(
            resp.outcome,
            InvokeOutcome::Handled as i32,
            "第 {i} 次应在额度内：{}",
            resp.reason
        );
    }

    // 第三次超了
    let resp = invoke(&mut client, &token, call("quota-3"))
        .await
        .expect("invoke 应可调用");
    assert_eq!(resp.outcome, InvokeOutcome::Error as i32, "超过配额必须被拒");
    assert!(
        resp.reason.contains("上限"),
        "原因要说清是撞了配额：{}",
        resp.reason
    );

    // **另一个插件不受影响**：配额按插件隔离，一个插件刷不出别人的额度
    let quiet = ctx
        .register_plugin("quota-c", "1.0.0", "http://127.0.0.1:9", &[])
        .await;
    let resp = invoke(&mut client, &quiet, call("quota-quiet-1"))
        .await
        .expect("invoke 应可调用");
    assert_eq!(
        resp.outcome,
        InvokeOutcome::Handled as i32,
        "别的插件不该被连累：{}",
        resp.reason
    );
}

/// 场景 7：deadline 夹紧。
///
/// 调用方给的整体 deadline 还很远，但本次只给了 5 秒预算——中台把下游看到的
/// deadline 夹到 `now + timeout`：本次调用的预算是调用方的显式意愿，不允许被
/// 更晚的整体 deadline 顶掉。
///
/// 断言里的 now 是**中台夹紧时刻**（产品在服务端取 `Utc::now()`，见 gateway.rs
/// 的 `clamp_deadline` 调用点），测试侧拿不到精确值：RPC 往返把它推后几十毫秒。
/// 所以上界在**收到应答之后**才取 now——那时刻必晚于服务端夹紧（同一台机的钟），
/// 服务端的 `now + timeout` 不可能越过它，不必猜一个延迟余量出来。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn deadline_被夹紧到_now_加_timeout(pool: PgPool) {
    let ctx = setup(pool).await;
    let a = echo_fixture("dl-a").await;
    let b = echo_fixture("dl-b").await;
    let token = ctx
        .register_plugin("dl-a", "1.0.0", &a.base_url(), &[])
        .await;
    ctx.register_plugin("dl-b", "1.0.0", &b.base_url(), &[])
        .await;

    let timeout_ms: u32 = 5_000;
    let before = Utc::now().timestamp_millis();
    let mut envelope = invoke_envelope("dl-1", &json!({ "text": "ping" }));
    envelope.deadline_ms = before + 3_600_000; // 远期整体 deadline，必须被夹下来

    let mut client = ctx.gateway_client().await;
    let resp = invoke(
        &mut client,
        &token,
        InvokeRequest {
            plugin: "dl-b".to_string(),
            version: String::new(),
            envelope: Some(envelope),
            timeout_ms,
        },
    )
    .await
    .expect("invoke 应可调用");
    assert_eq!(
        resp.outcome,
        InvokeOutcome::Handled as i32,
        "应答：{}",
        resp.reason
    );

    // 应答已回来：此刻必晚于服务端夹紧所用的 now
    let after = Utc::now().timestamp_millis();

    let received = b.last_envelope().expect("B 应收到信封");
    assert!(
        received.deadline_ms > before && received.deadline_ms <= after + i64::from(timeout_ms),
        "deadline 应被夹到 (before, after + timeout] 内（远期 deadline 不该被沿用），\
         实际 {}（before {before} / after {after} / timeout {timeout_ms}）",
        received.deadline_ms
    );
}

/// 场景 8：审计 span 落库。
///
/// 鉴权通过后的**每个出口**都要落一条：成功（ok）、被下游校验器拒（rejected）、
/// 下游出错（error）。调用链排查靠的是「每一跳都有痕」，缺哪种形态，那种调用
/// 就查不了——所以三种都要有，且都能对上 caller / target。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 互调审计_span_落库_每个出口一条(pool: PgPool) {
    let ctx = setup(pool).await;
    let a = echo_fixture("audit-a").await;
    let b = echo_fixture("audit-b").await;
    let token = ctx
        .register_plugin("audit-a", "1.0.0", &a.base_url(), &[])
        .await;
    ctx.register_plugin("audit-b", "1.0.0", &b.base_url(), &[])
        .await;

    const TRACE: &str = "audit-trace-1";

    let mut client = ctx.gateway_client().await;
    let call = |message_id: &str, plugin: &str| {
        let mut envelope = invoke_envelope(message_id, &json!({ "text": "ping" }));
        // 显式给 trace：查表要用它，也顺带验「调用方带了 trace 就沿用」
        envelope.trace_id = TRACE.to_string();
        InvokeRequest {
            plugin: plugin.to_string(),
            version: String::new(),
            envelope: Some(envelope),
            timeout_ms: 5_000,
        }
    };

    // 1) 成功；2) 被下游校验器拒（夹具按 message_id 前缀 bad- 拒）；3) 下游插件根本不存在
    let handled = invoke(&mut client, &token, call("audit-ok", "audit-b"))
        .await
        .expect("invoke 应可调用");
    assert_eq!(handled.outcome, InvokeOutcome::Handled as i32);
    let rejected = invoke(&mut client, &token, call("bad-audit-rejected", "audit-b"))
        .await
        .expect("invoke 应可调用");
    assert_eq!(rejected.outcome, InvokeOutcome::Rejected as i32);
    let errored = invoke(&mut client, &token, call("audit-error", "audit-ghost"))
        .await
        .expect("invoke 应可调用");
    assert_eq!(errored.outcome, InvokeOutcome::Error as i32);

    let spans = hub_store::spans::list_spans_by_trace(&ctx.pool, TRACE)
        .await
        .expect("查 span 失败");
    assert_eq!(spans.len(), 3, "三个出口各落一条，实际：{spans:?}");

    for span in &spans {
        assert!(
            span.name.starts_with("gateway.invoke."),
            "span 要挂在 gateway.invoke.* 名下：{}",
            span.name
        );
        let attrs = span.attributes.as_ref().expect("span 应带属性");
        assert_eq!(attrs["caller"], "audit-a", "审计要能回答「谁发起的」");
        assert_eq!(
            attrs["target"],
            span.name.trim_start_matches("gateway.invoke."),
            "属性里的 target 要与 span 名对上"
        );
        assert!(span.run_id.is_none(), "顶层直调没有 run 上下文");
    }

    // 成功那条的细节：outcome、幂等键与本跳 caller 入链
    let ok = spans.iter().find(|s| s.status == "ok").expect("成功那次应落 ok");
    assert_eq!(ok.name, "gateway.invoke.audit-b");
    let attrs = ok.attributes.as_ref().expect("ok span 应带属性");
    assert_eq!(attrs["outcome"], "HANDLED");
    assert_eq!(attrs["message_id"], "audit-ok");
    assert_eq!(
        attrs["call_chain"],
        json!(["audit-a"]),
        "链上应有本跳 caller"
    );

    // 另两个出口不能只落成 ok 一种形态
    assert!(
        spans
            .iter()
            .any(|s| s.status == "rejected" && s.name == "gateway.invoke.audit-b"),
        "被下游校验器拒的那次要有痕"
    );
    assert!(
        spans
            .iter()
            .any(|s| s.status == "error" && s.name == "gateway.invoke.audit-ghost"),
        "下游不存在的那次要有痕"
    );
}

/// 场景 9：没装配 Invoker 的实例（mock 中台的形态）必须明确回 unavailable。
///
/// 走 gRPC Status 而不是业务结果：这是**部署配置**问题，调用方重试也没用；
/// 混进业务结果里反而会被 SDK 当成「下游拒绝」去做降级，把配置错误掩盖成业务失败。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 未装配_invoker_时明确回_unavailable(pool: PgPool) {
    let ctx = setup_gateway(pool, None, CallPolicy::Allow, false).await;
    // 两个都按「不起进程」注册：这个形态拦在装配检查上，谁都到不了下游
    let token = ctx
        .register_plugin("noinv-a", "1.0.0", "http://127.0.0.1:9", &[])
        .await;
    ctx.register_plugin("noinv-b", "1.0.0", "http://127.0.0.1:9", &[])
        .await;

    let mut client = ctx.gateway_client().await;
    let err = invoke(
        &mut client,
        &token,
        InvokeRequest {
            plugin: "noinv-b".to_string(),
            version: String::new(),
            envelope: Some(invoke_envelope("noinv-1", &json!({ "text": "ping" }))),
            timeout_ms: 5_000,
        },
    )
    .await
    .expect_err("没装配调用能力必须以 gRPC 错误拒绝");

    assert_eq!(err.code(), Code::Unavailable, "实际：{err}");
    assert!(
        err.message().contains("未装配"),
        "原因要点明是没装配，实际：{}",
        err.message()
    );
}

/// 凭证挂着哪个 metadata 键、缺了/错了必须被拒，是「与 Publish 同强度」的
/// 安全承诺——此前只有 mock 侧替身验过这条，真实现在这里钉住：四个 RPC
/// 在无凭证与伪造凭证下都统一回 `Unauthenticated`，任何一条漏了都是绕过面。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 无凭证与伪造凭证_四个rpc统一拒绝(pool: PgPool) {
    let ctx = setup(pool).await;
    ctx.register_plugin("authz-a", "1.0.0", "http://127.0.0.1:9", &[])
        .await;

    let client = ctx.gateway_client().await;
    let invoke_req = InvokeRequest {
        plugin: "authz-a".to_string(),
        version: String::new(),
        envelope: Some(invoke_envelope("authz-1", &json!({ "text": "ping" }))),
        timeout_ms: 5_000,
    };

    for (label, token) in [("缺凭证", None), ("伪造凭证", Some("forged-token"))] {
        let mut client = client.clone();
        // 发现面三条 + 互调一条：每个 RPC 都过同一道闸
        let res = client
            .list_plugins(attach(
                Request::new(ListPluginsRequest::default()),
                token,
            ))
            .await;
        expect_unauth(label, "list_plugins", res);

        let res = client
            .describe_message(attach(
                Request::new(DescribeMessageRequest {
                    fq_name: MESSAGE_FQ.to_string(),
                }),
                token,
            ))
            .await;
        expect_unauth(label, "describe_message", res);

        let res = client
            .get_contract(attach(
                Request::new(GetContractRequest {
                    plugin: "authz-a".to_string(),
                    ..Default::default()
                }),
                token,
            ))
            .await;
        expect_unauth(label, "get_contract", res);

        let res = client
            .invoke(attach(Request::new(invoke_req.clone()), token))
            .await;
        expect_unauth(label, "invoke", res);
    }
}

/// 按用例语义挂（或不挂）凭证。
fn attach<T>(mut req: Request<T>, token: Option<&str>) -> Request<T> {
    if let Some(t) = token {
        req = with_token(req, t);
    }
    req
}

/// 断言应答是 `Unauthenticated`。真插件面的拒绝文案可能各有措辞，
/// 但 gRPC 状态码必须是同一个——SDK 判「该走重注册自愈」就看它。
fn expect_unauth<T>(label: &str, rpc: &str, res: Result<Response<T>, Status>)
where
    T: std::fmt::Debug,
{
    let err = res.expect_err(&format!("{label} 时 {rpc} 必须被拒"));
    assert_eq!(
        err.code(),
        Code::Unauthenticated,
        "{label} 时 {rpc} 的实际拒绝：{err}"
    );
}
