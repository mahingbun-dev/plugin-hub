//! 实例级治理的端到端：熔断跳闸与背压，以及它们对外暴露的状态码。
//!
//! 这里刻意走**真实的 gRPC 插件**而不是打桩：治理挂在 `Invoker` 上，判据是「这次调用
//! 本身有没有成功」。只有真插件真失败，才能验证「失败 → 跳闸 → 后续请求被拦下」这条链
//! 是通的，以及「校验器拒绝不算故障」这个容易写反的判据。

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_api::{ApiState, SystemState, router};
use hub_engine::{Governor, GovernorConfig, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_testkit as kit;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

/// 一套参数激进的治理器：阈值低、冷却长。冷却长是刻意的——测试要在「跳闸中」这个状态
/// 上做断言，冷却太长反而成了干扰项。
fn eager(threshold: u32) -> GovernorConfig {
    GovernorConfig {
        max_concurrency: 8,
        queue_timeout: Duration::ZERO,
        failure_threshold: threshold,
        open_cooldown: Duration::from_secs(60),
    }
}

/// 建 app 与 registry。
fn build(store: Store, govern: GovernorConfig) -> (Router, Registry) {
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker =
        Invoker::new(registry.clone(), (*client).clone()).with_governor(Governor::new(govern));
    let flows = hub_engine::FlowService::new(
        store.clone(),
        hub_engine::FlowExecutor::new(registry.clone(), invoker.clone()),
    );
    let app = router(
        SystemState::without_metrics(),
        ApiState::new(store, registry.clone(), invoker, flows),
    );
    (app, registry)
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

async fn register(fixture: &kit::Fixture, registry: &Registry) {
    let response = registry.register(&fixture.register_request(), None).await;
    assert!(response.accepted, "注册应通过：{:?}", response.rejections);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 实例连续失败会跳闸且后续请求被_503_拦下(pool: PgPool) {
    // 一个永远失败的插件
    let fixture = kit::start(kit::Behavior {
        fail_first: usize::MAX,
        ..kit::Behavior::named("broken-plugin", "1.0.0")
    })
    .await;

    let (app, registry) = build(Store::from_pool(pool), eager(3));
    register(&fixture, &registry).await;

    // 前 3 次：调用真的打到了插件，得到的是一次真实的调用失败
    for n in 1..=3 {
        let (status, body) = post(
            &app,
            "/ingress/broken-plugin",
            &json!({"payload": {"a": 1}}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "第 {n} 次应是一次普通的调用失败：{body}"
        );
        assert_eq!(body["error"], "plugin_unavailable", "{body}");
    }

    // 第 4 次：实例已经跳闸，请求**不该再打到插件**
    let before = fixture.handled_count();
    let (status, body) = post(
        &app,
        "/ingress/broken-plugin",
        &json!({"payload": {"a": 1}}),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "响应: {body}");
    assert_eq!(
        body["error"], "circuit_open",
        "应明确说是熔断而不是笼统的插件不可用：{body}"
    );
    assert_eq!(
        body["plugin"], "broken-plugin",
        "要带上是哪个插件，否则调用方无从降级：{body}"
    );
    assert!(
        body["message"].as_str().unwrap_or("").contains('3'),
        "文案里应带上累计失败次数，排障要用：{body}"
    );
    assert_eq!(
        fixture.handled_count(),
        before,
        "跳闸后不该再往已经确诊故障的实例上打流量"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 校验器拒绝不算实例故障(pool: PgPool) {
    // 默认行为：message_id 以 bad- 开头时校验器拒绝
    let fixture = kit::start_default().await;
    let (app, registry) = build(Store::from_pool(pool), eager(3));
    register(&fixture, &registry).await;

    // 连续拒绝，次数远超阈值
    for n in 0..5 {
        let (status, body) = post(
            &app,
            "/ingress/echo-plugin",
            &json!({"payload": {"a": 1}, "message_id": format!("bad-{n}")}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    }

    // 插件一直在正常干活，只是数据不合规——不该被熔断
    let (status, body) = post(
        &app,
        "/ingress/echo-plugin",
        &json!({"payload": {"orderId": "SO-9"}, "message_id": "good-1"}),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "校验器拒绝是业务结果不是故障，不能算进熔断：{body}"
    );
    assert_eq!(body["payload"]["orderId"], "SO-9");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 并发到顶时返回_429(pool: PgPool) {
    // 每次 Handle 慢 300ms，好让两个请求真的叠在一起
    let fixture = kit::start(kit::Behavior {
        delay: Some(Duration::from_millis(300)),
        ..kit::Behavior::named("slow-plugin", "1.0.0")
    })
    .await;

    let (app, registry) = build(
        Store::from_pool(pool),
        GovernorConfig {
            max_concurrency: 1,
            queue_timeout: Duration::ZERO, // 不排队：到顶就立刻失败
            ..eager(99)
        },
    );
    register(&fixture, &registry).await;

    // 第一个请求要先真的跑起来、把唯一的名额拿走，第二个才会被背压拦下。
    // 不能只把两个 future 建出来就 await——那样 poll 顺序反过来，被拦下的会是第一个。
    let holder = {
        let app = app.clone();
        tokio::spawn(async move {
            post(&app, "/ingress/slow-plugin", &json!({"payload": {"a": 1}})).await
        })
    };
    tokio::time::sleep(Duration::from_millis(80)).await;

    let (status, body) = post(&app, "/ingress/slow-plugin", &json!({"payload": {"a": 2}})).await;

    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "背压要和熔断分开表达：前者是「整体太忙」，后者是「这个实例不能用」：{body}"
    );
    assert_eq!(body["error"], "overloaded");
    assert_eq!(body["plugin"], "slow-plugin");

    let (held_status, held_body) = holder.await.expect("占用名额的请求应正常结束");
    assert_eq!(held_status, StatusCode::OK, "响应: {held_body}");
    assert_eq!(fixture.handled_count(), 1, "被背压拦下的那次绝不该打到插件");
}

// ---------------------------------------------------------------- 治理快照接口

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
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

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 治理快照在没人调用过时是空的(pool: PgPool) {
    let (app, _registry) = build(Store::from_pool(pool), eager(3));

    let (status, body) = get(&app, "/admin/governance").await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    // 配置要带出来：看这份面板的第一个问题就是「这些数字从哪来」
    let config = &body["config"];
    assert_eq!(
        config["failure_threshold"], 3,
        "带出来的要是这个实例真正在用的那套参数，不是默认值：{body}"
    );
    assert_eq!(config["max_concurrency"], 8);
    assert!(config["queue_timeout_ms"].as_u64().is_some());
    assert!(config["open_cooldown_secs"].as_u64().is_some());

    assert!(
        body["instances"].as_array().expect("应当是数组").is_empty(),
        "治理表按「被调用过」建项，一次调用都没有时它是空的——不是报错，\
         也不是「没有实例在线」：那要看 /admin/instances"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 治理快照在调用过之后给出该实例的状态(pool: PgPool) {
    let fixture = kit::start(kit::Behavior::named("govern-demo", "1.0.0")).await;
    let (app, registry) = build(Store::from_pool(pool), eager(3));
    register(&fixture, &registry).await;

    // 先真的调一次——治理表是按「被调用过」建项的
    let (status, body) = post(&app, "/ingress/govern-demo", &json!({"payload": {"a": 1}})).await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    let (status, body) = get(&app, "/admin/governance").await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    let instances = body["instances"].as_array().expect("实例应当是数组");
    assert_eq!(instances.len(), 1, "响应: {body}");

    let instance = &instances[0];
    assert!(
        instance["instance_id"]
            .as_str()
            .unwrap_or("")
            .contains("govern-demo"),
        "应当是刚才那个实例，实际：{instance}"
    );

    let max = instance["max_concurrency"].as_u64().expect("并发上限");
    let available = instance["available"].as_u64().expect("剩余名额");
    let in_flight = instance["in_flight"].as_u64().expect("在飞数");
    assert!(available <= max, "剩余名额不该超过上限");
    assert_eq!(
        available + in_flight,
        max,
        "在飞 = 上限 − 剩余名额：两者本该互补，对不上说明计数错了"
    );

    // 调用已经结束，名额应该都放回来了
    assert_eq!(in_flight, 0, "调用结束后不该还有在飞");
    assert_eq!(instance["breaker"], "closed", "只调了一次，不该跳闸");
    assert_eq!(instance["consecutive_failures"], 0);
    assert!(
        instance["admitted"].as_u64().unwrap_or(0) >= 1,
        "至少放行过一次，实际：{instance}"
    );
    assert!(
        instance.get("open_for_ms").is_none(),
        "没跳闸时不该给冷却剩余时间——给了会让前端以为它在跳闸"
    );

    drop(fixture);
}
