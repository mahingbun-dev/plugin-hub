//! **插件网关（PluginGateway）**——凭证怎么到手、发现怎么查、互调的信封怎么组装、
//! 业务结果怎么映射成类型化错误。
//!
//! 断言分两层，与 `tests/state.rs` 同一条纪律：
//!   1. **插件侧看到了什么**（返回值 / 错误变体对不对）；
//!   2. **中台侧收到了什么**（`MockHub` 记下来的凭证、信封与链）。
//!
//! 只验第 1 层的话，「其实什么都没发出去、mock 恰好也放行了」这种错会漏过去。
//!
//! 伪造中台只回答「SDK 发出的请求长什么样」；防环、链深、配额、subject 覆盖这些
//! **中台侧**的行为，验收在 `crates/hub-grpc/tests/gateway.rs`，不在这里重复。

mod support;

use std::collections::HashMap;
use std::time::Duration;

use hubkit::proto::{
    DescribeMessageResponse, Envelope, GetContractResponse, MessageEndpoint, PluginSummary,
    PayloadType, ValidationIssue,
};
use hubkit::{Config, GatewayError, InvokeOptions, CALL_CHAIN_META, StateError};
use support::*;

fn config_for(mock: &MockHub, instance: &str) -> Config {
    Config {
        hub_addr: mock.url(),
        advertise_addr: PLUGIN_ADVERTISE.to_string(),
        listen_addr: PLUGIN_LISTEN_ANY.to_string(),
        instance_id: instance.to_string(),
        logger: quiet(),
        register_retry_interval: Duration::from_millis(50),
        heartbeat_fallback_interval: Duration::from_millis(100),
        state_call_timeout: Duration::from_secs(2),
        gateway_call_timeout: Duration::from_secs(2),
    }
}

/// 起一个「需要用网关」的插件，返回插件句柄（测试从它拿注入进来的客户端）。
struct Running {
    plugin: GatewayPlugin,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), hubkit::HubkitError>>,
}

async fn start_gateway_plugin(mock: &MockHub, name: &str, instance: &str) -> Running {
    let plugin = GatewayPlugin::named(name);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let task = tokio::spawn(hubkit::run_with_shutdown(
        plugin.clone(),
        config_for(mock, instance),
        async move {
            let _ = stop_rx.await;
        },
    ));

    Running {
        plugin,
        stop: stop_tx,
        task,
    }
}

/// 调用方「正在处理」的信封：模拟本插件在一条互调链里收到了上游的请求。
fn current_envelope() -> Envelope {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    Envelope {
        message_id: "01J0CURRENT0ENVELOPE".into(),
        trace_id: "01J0TRACE0FROM0UPSTREAM".into(),
        run_id: "01J0RUN".into(),
        node_id: "node-3".into(),
        deadline_ms: now + 60_000,
        meta: HashMap::from([(
            CALL_CHAIN_META.to_string(),
            "upstream-a,upstream-b".to_string(),
        )]),
        ..Default::default()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn 注册后网关客户端被注入_发现调用带凭证且请求如实到达() {
    let mock = start_mock(MockConfig::with_interval(1)).await;
    let running = start_gateway_plugin(&mock, "gw-plugin", "it-gw-1").await;

    // ---- 注入：插件作者不该自己管凭证，这一步由骨架完成 ----
    let gateway = running.plugin.wait_for_gateway(Duration::from_secs(5)).await;
    assert!(
        !mock.state_token().is_empty(),
        "伪造中台应当在注册回执里下发凭证"
    );

    // ---- ListPlugins：罐装应答原样回来，include_offline 如实到达 ----
    mock.set_plugins(vec![PluginSummary {
        name: "order-reader".into(),
        latest_version: "0.2.0".into(),
        online: true,
        instance_count: 2,
        description: "读单插件".into(),
    }]);
    let list = gateway.list_plugins(false).await.expect("列清单应当成功");
    assert_eq!(list.plugins.len(), 1);
    assert_eq!(list.plugins[0].name, "order-reader");
    assert_eq!(list.plugins[0].instance_count, 2);
    assert_eq!(mock.list_requests(), vec![false], "include_offline 必须照传");

    // ---- DescribeMessage：fq_name 到达，生产/消费端点回来 ----
    mock.set_message_endpoints(DescribeMessageResponse {
        producers: vec![MessageEndpoint {
            plugin: "order-reader".into(),
            version: "0.2.0".into(),
        }],
        consumers: vec![MessageEndpoint {
            plugin: "billing".into(),
            version: "1.0.0".into(),
        }],
    });
    let described = gateway
        .describe_message("wms.v1.OrderCreated")
        .await
        .expect("查消息端点应当成功");
    assert_eq!(described.producers[0].plugin, "order-reader");
    assert_eq!(described.consumers[0].plugin, "billing");
    assert_eq!(
        mock.describe_requests(),
        vec!["wms.v1.OrderCreated".to_string()],
        "fq_name 必须照传"
    );

    // ---- GetContract：version / fq_name 显式给与给 None（=空串，最新版/不展开）----
    mock.set_contract(GetContractResponse {
        name: "order-reader".into(),
        version: "0.2.0".into(),
        ..Default::default()
    });
    let contract = gateway
        .get_contract("order-reader", Some("0.2.0"), Some("wms.v1.OrderCreated"))
        .await
        .expect("查契约应当成功");
    assert_eq!(contract.name, "order-reader");

    let sent = &mock.contract_requests()[0];
    assert_eq!(sent.plugin, "order-reader");
    assert_eq!(sent.version, "0.2.0");
    assert_eq!(sent.fq_name, "wms.v1.OrderCreated");

    let _ = gateway
        .get_contract("order-reader", None, None)
        .await
        .expect("不指定版本与 schema 也应当成功");
    let sent = &mock.contract_requests()[1];
    assert_eq!(sent.version, "", "None 就是「最新版」，线协议里是空串");
    assert_eq!(sent.fq_name, "", "None 就是不展开 schema");

    // ---- 中台侧收到的凭证：四个 RPC 都必须带（与状态面同一个键、同一张凭证）----
    let seen = mock.gateway_seen_tokens();
    assert_eq!(seen.len(), mock.gateway_calls(), "每个网关请求都要记到凭证");
    let expected = mock.state_token();
    assert!(
        seen.iter().all(|t| t == &expected),
        "每一次网关请求带的都该是中台刚下发的那张 {expected:?}，实际收到 {seen:?}"
    );

    running.stop.send(()).ok();
    running.task.await.expect("插件应当正常退出").expect("退出");
}

#[tokio::test]
async fn 互调传当前信封时_trace_贯通_链原样复制_不追加自己() {
    let mock = start_mock(MockConfig::with_interval(1)).await;
    let running = start_gateway_plugin(&mock, "gw-plugin", "it-gw-chain").await;
    let gateway = running.plugin.wait_for_gateway(Duration::from_secs(5)).await;

    let current = current_envelope();
    let out = gateway
        .invoke_plugin(
            "order-pricer",
            serde_json::json!({"sku": "A-1"}),
            InvokeOptions {
                current_envelope: Some(&current),
                timeout_ms: Some(4_000),
                ..Default::default()
            },
        )
        .await
        .expect("互调（回显替身）应当成功");

    // 替身是原信封回显：返回的信封就是发出的那个
    assert_eq!(out.trace_id, current.trace_id);

    // ---- 中台侧收到了什么 ----
    let sent = mock.invoke_requests();
    assert_eq!(sent.len(), 1);
    let sent = &sent[0];
    assert_eq!(sent.plugin, "order-pricer");
    assert_eq!(sent.version, "", "没给 version 就是「最新版」");
    assert_eq!(sent.timeout_ms, 4_000, "预算必须照传");

    let env = sent.envelope.as_ref().expect("请求里必须有信封");
    // trace 贯通：同一条链路的下游
    assert_eq!(env.trace_id, current.trace_id);
    assert_eq!(env.run_id, current.run_id);
    assert_eq!(env.node_id, current.node_id);

    // 链**原样**复制：插件名叫 gw-plugin，它**不**该出现在链里——
    // 追加 caller 是中台的职责，SDK 多写一笔防环的判断对象就错了
    assert_eq!(
        env.meta.get(CALL_CHAIN_META).map(String::as_str),
        Some("upstream-a,upstream-b")
    );
    assert!(!env.meta.values().any(|v| v.contains("gw-plugin")));

    // message_id 是新 ULID：复制调用方的会让两次互调在下游被当成同一条消息去重
    assert_ne!(env.message_id, current.message_id);
    assert_eq!(env.message_id.len(), 26, "ULID 是 26 个字符");

    // type 与 payload
    assert_eq!(env.r#type, PayloadType::Request as i32);
    let payload = hubkit::envelope::payload_json(env).expect("应当是 JSON 载荷");
    assert_eq!(payload, serde_json::json!({"sku": "A-1"}));

    // deadline 起算于本次预算（外层还有 60s，预算 4s 更早）
    assert!(
        env.deadline_ms <= now_ms() + 4_000 && env.deadline_ms > now_ms() - 1_000,
        "deadline 应当是 now + 预算，实际 {}",
        env.deadline_ms
    );

    running.stop.send(()).ok();
    running.task.await.expect("插件应当正常退出").expect("退出");
}

#[tokio::test]
async fn 互调不传当前信封时_新_trace_不带链() {
    let mock = start_mock(MockConfig::with_interval(1)).await;
    let running = start_gateway_plugin(&mock, "gw-plugin", "it-gw-top").await;
    let gateway = running.plugin.wait_for_gateway(Duration::from_secs(5)).await;

    let out = gateway
        .invoke_plugin(
            "order-pricer",
            serde_json::json!({}),
            InvokeOptions::default(),
        )
        .await
        .expect("顶层互调应当成功");
    assert!(!out.trace_id.is_empty(), "替身回显的信封里有新 trace");

    let sent = &mock.invoke_requests()[0];
    let env = sent.envelope.as_ref().unwrap();
    assert!(!env.trace_id.is_empty(), "顶层直调也必须有 trace_id");
    assert_eq!(env.run_id, "", "没有上游就没有 run_id");
    assert_eq!(env.node_id, "");
    assert!(
        !env.meta.contains_key(CALL_CHAIN_META),
        "没有上游就不该凭空造一段链——中台读不到这个键时按链为空处理"
    );
    assert_eq!(sent.timeout_ms, 0, "没给预算就走中台的兜底，线协议里是 0");
    // 信封契约里 deadline 必填：SDK 按中台兜底预算（30s）起算
    assert!(env.deadline_ms > now_ms(), "deadline 必须是未来时刻");

    running.stop.send(()).ok();
    running.task.await.expect("插件应当正常退出").expect("退出");
}

#[tokio::test]
async fn 业务结果映射成类型化错误_reason_与_issues_都随错误携带() {
    let mock = start_mock(MockConfig::with_interval(1)).await;
    let running = start_gateway_plugin(&mock, "gw-plugin", "it-gw-map").await;
    let gateway = running.plugin.wait_for_gateway(Duration::from_secs(5)).await;

    // ---- REJECTED：下游的意见，issues 结构化随行 ----
    mock.set_invoke_behavior(InvokeBehavior::Reject {
        reason: "库存不足".into(),
        issues: vec![ValidationIssue {
            path: "payload.sku".into(),
            message: "不存在".into(),
            severity: hubkit::proto::Severity::Error as i32,
        }],
    });
    let err = gateway
        .invoke_plugin(
            "inventory",
            serde_json::json!({"sku": "X"}),
            InvokeOptions::default(),
        )
        .await
        .expect_err("REJECTED 必须是错误");
    let GatewayError::Rejected { reason, issues } = &err else {
        panic!("应当映射成 Rejected，实际 {err:?}")
    };
    assert_eq!(reason, "库存不足");
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, "payload.sku");
    // 人读的呈现也要把 issues 带上——排障时它比 reason 更能定位
    assert!(err.to_string().contains("payload.sku"), "{err}");

    // ---- ERROR：中台或下游的裁决，reason 随行 ----
    mock.set_invoke_behavior(InvokeBehavior::Fail {
        reason: "未声明对 inventory 的调用授权".into(),
    });
    let err = gateway
        .invoke_plugin(
            "inventory",
            serde_json::json!({"sku": "X"}),
            InvokeOptions::default(),
        )
        .await
        .expect_err("ERROR 必须是错误");
    let GatewayError::Error { reason } = &err else {
        panic!("应当映射成 Error，实际 {err:?}")
    };
    assert_eq!(reason, "未声明对 inventory 的调用授权");

    running.stop.send(()).ok();
    running.task.await.expect("插件应当正常退出").expect("退出");
}

#[tokio::test]
async fn publish_受理返回_run_id_被拒时_reason_随错误带给调用方() {
    let mock = start_mock(MockConfig::with_interval(1)).await;
    // Publish 走的是 StateClient（HubState 服务的另一个 RPC），所以起的是状态插件
    let plugin = StatePlugin::named("pub-plugin");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(hubkit::run_with_shutdown(
        plugin.clone(),
        config_for(&mock, "it-pub-1"),
        async move {
            let _ = stop_rx.await;
        },
    ));

    let state = plugin.wait_for_state(Duration::from_secs(5)).await;

    // 与真中台一致：message_id 是总线去重的幂等键
    let envelope = Envelope {
        message_id: "01J0PUBLISH".into(),
        trace_id: "01J0PTRACE".into(),
        deadline_ms: now_ms() + 30_000,
        ..Default::default()
    };

    // ---- 受理：拿到执行 id ----
    let run_id = state
        .publish("order-flow", envelope.clone())
        .await
        .expect("受理的发布应当返回 run_id");
    assert!(run_id.starts_with("mock-run-"), "实际 {run_id}");

    // ---- 中台侧收到了什么：target、信封、凭证 ----
    let sent = mock.publishes();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].target, "order-flow");
    assert_eq!(
        sent[0].envelope.as_ref().unwrap().message_id,
        "01J0PUBLISH"
    );
    assert_eq!(
        mock.state_seen_tokens().last().unwrap(),
        &mock.state_token(),
        "发布请求也必须带上凭证"
    );

    // ---- 被拒（防环 / 配额）：reason 必须原样到达调用方，而不是被吞成「发了没反应」----
    let ring_reason = "检测到环：order-flow 已在本次触发链上（order-flow）";
    mock.reject_publishes(ring_reason);
    let err = state
        .publish("order-flow", envelope)
        .await
        .expect_err("被拦下的发布必须是错误");
    let StateError::Rejected(reason) = &err else {
        panic!("应当是 Rejected，实际 {err:?}")
    };
    assert_eq!(reason, ring_reason);
    assert!(err.to_string().contains("order-flow"), "{err}");

    // ---- 恢复受理 ----
    mock.accept_publishes();
    let _ = state
        .publish(
            "order-flow",
            Envelope {
                message_id: "01J0PUBLISH2".into(),
                ..Default::default()
            },
        )
        .await
        .expect("恢复后发布应当重新受理");

    stop_tx.send(()).ok();
    task.await.expect("插件应当正常退出").expect("退出");
}

#[tokio::test]
async fn 网关调用撞上_401_同样走重注册自愈() {
    let mock = start_mock(MockConfig::with_interval(1)).await;
    let running = start_gateway_plugin(&mock, "gw-plugin", "it-gw-401").await;
    let gateway = running.plugin.wait_for_gateway(Duration::from_secs(5)).await;
    let first_token = mock.state_token();

    // 走完冷却窗口再吊销——窗口内的 denial 会被丢掉，那是另一条测试要验的行为
    tokio::time::sleep(Duration::from_millis(200)).await;
    mock.revoke_state();

    let err = gateway
        .list_plugins(false)
        .await
        .expect_err("凭证被吊销后网关调用应当失败");
    assert!(
        err.is_unauthenticated(),
        "应当是 401 语义的错误，实际 {err:?}"
    );

    // ---- 骨架应当已经因此重新注册（与状态调用同一条自愈路径）----
    assert!(
        mock.wait_for_registrations(2, Duration::from_secs(5)).await,
        "{}",
        msg("401 之后的第二次注册")
    );
    let second_token = mock.state_token();
    assert_ne!(first_token, second_token, "重新注册应当换一张新凭证");
    assert!(
        wait_until(Duration::from_secs(5), || running.plugin.injections() >= 2).await,
        "{}",
        msg("重新注册后的再次注入网关客户端")
    );

    // ---- 换到新凭证之后，同一个客户端继续可用 ----
    let list = gateway.list_plugins(false).await.expect("应当恢复");
    assert!(list.plugins.is_empty());
    let seen = mock.gateway_seen_tokens();
    assert_eq!(
        seen.last().unwrap(),
        &second_token,
        "恢复后的请求带的必须是新凭证"
    );

    running.stop.send(()).ok();
    running.task.await.expect("插件应当正常退出").expect("退出");
}
