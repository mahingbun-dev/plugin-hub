//! 端到端：真实 gRPC 上跑通「注册 → 探测 → 调用」全链路。
//!
//! 插件由 `hub-testkit` 提供（真实 gRPC 服务，行为可配置），中台侧用真的 gRPC 客户端
//! 去探测与调用——只有真实链路才能证明自报地址、可达性探测、跨进程调用这几段是通的。

use std::sync::Arc;

use hub_engine::{InvokeError, InvokeOutcome, Invoker};
use hub_grpc::RegistryService;
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_proto::v1::plugin_registry_server::PluginRegistry as _;
use hub_proto::v1::{Envelope, HeartbeatRequest, RejectCode};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_testkit as kit;
use sqlx::PgPool;
use tonic::Request;

fn build(store: Store) -> (Registry, Invoker, Arc<PluginClient>) {
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store,
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    (registry, invoker, client)
}

fn envelope(message_id: &str) -> Envelope {
    Envelope {
        message_id: message_id.to_string(),
        trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_string(),
        ..Default::default()
    }
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 注册探测调用全链路走通(pool: PgPool) {
    let fixture = kit::start_default().await;
    let (registry, invoker, _client) = build(Store::from_pool(pool));

    let response = registry
        .register(&fixture.register_request(), Some("10.1.2.3"))
        .await;
    assert!(response.accepted, "注册应通过：{:?}", response.rejections);

    let outcome = invoker
        .invoke("echo-plugin", None, envelope("msg-1"))
        .await
        .expect("调用失败");

    match outcome {
        InvokeOutcome::Handled {
            envelope, target, ..
        } => {
            assert_eq!(target.advertise_addr, fixture.base_url());
            assert_eq!(
                envelope.meta.get("handled_by").map(String::as_str),
                Some("echo-plugin"),
                "结果应来自插件的 Handle"
            );
        }
        other => panic!("应处理成功，实际 {other:?}"),
    }

    assert_eq!(fixture.handled_count(), 1, "插件体应被调用一次");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 校验失败时短路且不执行插件体(pool: PgPool) {
    let fixture = kit::start_default().await;
    let (registry, invoker, _client) = build(Store::from_pool(pool));
    assert!(
        registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let outcome = invoker
        .invoke("echo-plugin", None, envelope("bad-1"))
        .await
        .expect("调用本身应成功返回");

    match outcome {
        InvokeOutcome::Rejected { issues, .. } => {
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].path, "message_id");
            assert!(issues[0].message.contains("bad-"));
        }
        other => panic!("应被校验器拒绝，实际 {other:?}"),
    }

    assert_eq!(
        fixture.handled_count(),
        0,
        "校验不通过时插件体绝不能被执行——这是「校验器随插件发布」的意义所在"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 调用未注册的插件报错(pool: PgPool) {
    let (_registry, invoker, _client) = build(Store::from_pool(pool));

    let err = invoker
        .invoke("从未注册过", None, envelope("msg-1"))
        .await
        .expect_err("应报错");
    assert!(
        err.to_string().contains("从未注册过"),
        "错误信息应指明是哪个插件，实际 {err}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件停掉后调用报连接失败(pool: PgPool) {
    let fixture = kit::start_default().await;
    let (registry, invoker, client) = build(Store::from_pool(pool));
    assert!(
        registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let base_url = fixture.base_url();
    fixture.stop().await;
    client.forget(&base_url).await;

    let err = invoker
        .invoke("echo-plugin", None, envelope("msg-1"))
        .await
        .expect_err("插件已不在，调用必须失败");
    assert!(
        matches!(err, InvokeError::Client { .. }),
        "应归类为客户端调用失败（插件不可达），而不是挂住或误报校验失败，实际 {err:?}"
    );
    assert!(
        err.to_string().contains("echo-plugin"),
        "错误信息要指明是哪个插件，实际 {err}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 地址不可达的插件注册被拒(pool: PgPool) {
    let (registry, _invoker, _client) = build(Store::from_pool(pool));

    let mut request = kit::start_default().await.register_request();
    // 127.0.0.1:1 是保留端口，不会有服务监听
    request.advertise_addr = "http://127.0.0.1:1".to_string();

    let response = registry.register(&request, None).await;
    assert!(!response.accepted);
    assert_eq!(
        response
            .rejections
            .iter()
            .map(|r| r.code)
            .collect::<Vec<_>>(),
        vec![RejectCode::Unreachable as i32],
        "可达性探测走的就是真实的 gRPC 客户端"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 信封的_deadline_会透传到插件(pool: PgPool) {
    let fixture = kit::start_default().await;
    let (registry, invoker, _client) = build(Store::from_pool(pool));
    assert!(
        registry
            .register(&fixture.register_request(), None)
            .await
            .accepted
    );

    let deadline = chrono::Utc::now().timestamp_millis() + 5_000;
    let envelope = Envelope {
        deadline_ms: deadline,
        ..envelope("msg-1")
    };
    assert!(matches!(
        invoker.invoke("echo-plugin", None, envelope).await,
        Ok(InvokeOutcome::Handled { .. })
    ));

    let seen = fixture.last_envelope().expect("插件应收到信封");
    assert_eq!(
        seen.deadline_ms, deadline,
        "插件要能读到绝对 deadline 才能提前放弃，而不是把时间耗光"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 注册服务走_grpc_编解码(pool: PgPool) {
    let fixture = kit::start_default().await;
    let (registry, _invoker, _client) = build(Store::from_pool(pool));
    let service = RegistryService::new(registry);

    let response = service
        .register(Request::new(fixture.register_request()))
        .await
        .expect("gRPC 调用失败")
        .into_inner();

    assert!(response.accepted);
    assert_eq!(response.instance_id, "instance-echo-plugin");
    assert_eq!(response.heartbeat_interval_seconds, 10);

    let heartbeat = service
        .heartbeat(Request::new(HeartbeatRequest {
            instance_id: "instance-echo-plugin".to_string(),
        }))
        .await
        .expect("心跳失败")
        .into_inner();
    assert!(heartbeat.accepted);
    assert!(!heartbeat.reregister_required);
}
