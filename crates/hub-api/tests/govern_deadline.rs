//! 治理与「预算」交互的端到端：**调用方等不起，会不会把实例对所有人打成熔断**。
//!
//! 独立验证 agent 编写。走真实 gRPC 插件 + 真实注册表。
//! 断言写的是**正确行为**。这两条起初都是失败的（缺陷真实存在），修复后转为回归防守——
//! 断言不改，它们从此守着这两个缺陷不再出现。

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_api::{ApiState, SystemState, router};
use hub_engine::{Governor, GovernorConfig, InvokeError, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_proto::v1::Envelope;
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_testkit as kit;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

/// 跳闸阈值 2、冷却 60 秒：跳闸后长时间拒绝，便于断言。
fn eager(threshold: u32) -> GovernorConfig {
    GovernorConfig {
        max_concurrency: 8,
        queue_timeout: Duration::ZERO,
        failure_threshold: threshold,
        open_cooldown: Duration::from_secs(60),
    }
}

/// 建 Invoker / Registry / app（与 governance_api.rs 同一套装配）。
fn harness(store: Store, threshold: u32) -> (Router, Registry, Invoker) {
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone())
        .with_governor(Governor::new(eager(threshold)));
    let flows = hub_engine::FlowService::new(
        store.clone(),
        hub_engine::FlowExecutor::new(registry.clone(), invoker.clone()),
    );
    let app = router(
        SystemState::without_metrics(),
        ApiState::new(store, registry.clone(), invoker.clone(), flows),
    );
    (app, registry, invoker)
}

async fn register(fixture: &kit::Fixture, registry: &Registry) {
    let response = registry.register(&fixture.register_request(), None).await;
    assert!(response.accepted, "注册应通过：{:?}", response.rejections);
}

fn envelope() -> Envelope {
    Envelope {
        message_id: "ok-1".to_string(),
        payload: Some(hub_proto::encode_payload(&json!({"a": 1})).expect("编码载荷")),
        ..Default::default()
    }
}

fn brief<T: std::fmt::Debug>(r: &Result<T, InvokeError>) -> String {
    match r {
        Ok(_) => "Ok".to_string(),
        Err(e) => format!("{e}"),
    }
}

async fn post(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).expect("序列化失败")))
        .expect("构造请求失败");
    let response = app.clone().oneshot(request).await.expect("请求处理失败");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体失败")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// 【已复现】`instance_healthy(&InvokeError::Timeout{..}) == false`：
/// 「预算不够、调用方自己等不起」被记成**实例故障**。攒够阈值后，实例对**所有人**
/// 熔断——包括本来能成功的大预算调用。这里是确定性的版本：插件处理要 200ms，
/// 两次 50ms 预算的调用把它打成熔断，随后一个 5 秒预算的调用被 503 拦下（其实它会成功）。
///
/// 代码注释把这条写成刻意的取舍（「超出了预算指向这个实例不行」），
/// 但后果是「一个调用方的耐心」能变成「所有人的 10 秒不可用」。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 预算不够的调用不该把实例对所有人打成熔断(pool: PgPool) {
    // 健康但慢的插件：200ms 才处理完
    let fixture = kit::start(kit::Behavior {
        delay: Some(Duration::from_millis(200)),
        ..kit::Behavior::named("slow-plugin", "1.0.0")
    })
    .await;
    let (_app, registry, invoker) = harness(Store::from_pool(pool), 2);
    register(&fixture, &registry).await;

    let target = registry
        .resolve("slow-plugin", None)
        .await
        .expect("应能解析到实例");

    // 两次「只给 50ms」的调用：实例没病，是调用方自己等不起
    for n in 1..=2 {
        let r = invoker
            .invoke_target(&target, envelope(), Some(Duration::from_millis(50)))
            .await;
        println!("【第 {n} 次 50ms 预算调用】{}", brief(&r));
        assert!(
            matches!(r, Err(InvokeError::Timeout { .. })),
            "50ms 预算对 200ms 的插件必然超时；实际 {}",
            brief(&r)
        );
    }

    // 预算充足的调用（5 秒）：插件 200ms 就能处理完，理应成功
    let after = invoker
        .invoke_target(&target, envelope(), Some(Duration::from_secs(5)))
        .await;
    println!("【5 秒预算的下一次调用】{}", brief(&after));
    assert!(
        after.is_ok(),
        "本断言是**正确行为**：实例健康，只是有调用方等不起；\
         实际它被熔断拦下了（预算不足被当成实例故障）——{}",
        brief(&after)
    );
}

/// 【未能稳定复现】同一个机制在 HTTP 面的入口：`timeout_ms` 只要 > 0 就放行
/// （`resolve_timeout` 只拒绝 <= 0），1ms 的 deadline 让调用方没有任何余量。
/// 实测这一步是**概率性**的：约 1/3 的请求会 502（超时），能不能攒够阈值看运气。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 极小_timeout_ms_不该把健康实例打成熔断(pool: PgPool) {
    let fixture = kit::start_default().await;
    let (app, registry, _invoker) = harness(Store::from_pool(pool), 3);
    register(&fixture, &registry).await;

    let mut timed_out = 0;
    for _ in 0..30 {
        let (status, body) = post(
            &app,
            "/ingress/echo-plugin",
            &json!({"payload": {"a": 1}, "timeout_ms": 1}),
        )
        .await;
        if status == StatusCode::BAD_GATEWAY {
            timed_out += 1;
        }
        if status == StatusCode::SERVICE_UNAVAILABLE {
            println!("【第 {timed_out} 次超时后即被熔断】{body}");
            break;
        }
    }
    println!("【30 次 timeout_ms=1 中，超时 {} 次】", timed_out);
    println!("【插件真实处理次数】{}", fixture.handled_count());

    let (status, body) = post(&app, "/ingress/echo-plugin", &json!({"payload": {"a": 1}})).await;
    println!("【正常请求】{status}");
    assert_eq!(
        status,
        StatusCode::OK,
        "本断言是**正确行为**：健康实例不该被极小 timeout_ms 的调用打成熔断；\
         实际正常请求吃了 {status} {body}"
    );
}
