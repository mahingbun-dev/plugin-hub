//! 直连插件的 gRPC，验 `PluginRuntime` 那五个方法。
//!
//! 这一层就是接入四关里的 **L2 运行时自检**：不需要中台在线，验的是
//! 「插件作为 gRPC 进程是健康的、接口层不会出错」。它**证明不了网络可达性**——
//! 那是 L3（中台能不能拨通你上报的地址）的事。

mod support;

use std::time::Duration;

use hubkit::proto::{DescribeRequest, Envelope, HandleRequest, HealthRequest, ValidateRequest};
use hubkit::{Config, Level, Logger};
use support::*;

/// 起一个插件，返回（它的 gRPC 地址，停止信号）。
async fn start_plugin(mock: &MockHub) -> (String, tokio::sync::oneshot::Sender<()>) {
    let port = free_port();
    let config = Config {
        hub_addr: mock.url(),
        advertise_addr: local_addr(port),
        listen_addr: format!("127.0.0.1:{port}"),
        instance_id: "it-service".to_string(),
        logger: Logger::new(Level::Error),
        register_retry_interval: Duration::from_millis(50),
        heartbeat_fallback_interval: Duration::from_millis(100),
        // 其余取缺省：这些用例与 HubState 无关，状态调用超时用缺省值即可
        ..Default::default()
    };

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(hubkit::run_with_shutdown(DemoPlugin, config, async move {
        let _ = stop_rx.await;
    }));

    // 等它真的监听上：注册成功即意味着 gRPC 已经起来（监听先于注册）
    assert!(
        mock.wait_for_registrations(1, Duration::from_secs(5)).await,
        "{}",
        msg("插件起来并注册")
    );

    (local_addr(port), stop_tx)
}

fn json_envelope(payload: serde_json::Value) -> Envelope {
    hubkit::envelope::with_payload_json(
        &Envelope {
            message_id: "it-1".to_string(),
            ..Default::default()
        },
        payload,
    )
    .expect("载荷应当是合法 JSON 对象")
}

#[tokio::test]
async fn 五个方法都按契约应答() {
    let mut mock = start_mock(MockConfig::with_interval(1)).await;
    let (addr, stop) = start_plugin(&mock).await;

    let mut client = connect_runtime(&addr).await;

    // ---- Health ----
    let health = client
        .health(HealthRequest {})
        .await
        .expect("Health 应当应答")
        .into_inner();
    assert!(health.healthy, "插件自报不健康: {}", health.message);

    // ---- Describe ----
    let manifest = client
        .describe(DescribeRequest {})
        .await
        .expect("Describe 应当应答")
        .into_inner();
    assert_eq!(manifest.name, "test-plugin");
    assert_eq!(manifest.version, "0.1.0");

    // ---- Validate：合规载荷放行 ----
    let good = client
        .validate(ValidateRequest {
            envelope: Some(json_envelope(serde_json::json!({"text": "你好"}))),
        })
        .await
        .expect("Validate 应当应答")
        .into_inner();
    assert!(good.valid, "合规载荷应当放行：{:?}", good.issues);

    // ---- Validate：缺必填字段被拒，且问题定位到具体路径 ----
    let bad = client
        .validate(ValidateRequest {
            envelope: Some(json_envelope(serde_json::json!({}))),
        })
        .await
        .expect("Validate 应当应答")
        .into_inner();
    assert!(!bad.valid, "缺 text 应当被拒");
    assert_eq!(bad.issues[0].path, "payload.text");
    assert_eq!(
        bad.issues[0].severity,
        hubkit::proto::Severity::Error as i32
    );

    // ---- Validate：空信封不崩 ----
    // 中台在异常路径上可能送来一个没有信封的请求，它必须是明确的报错而不是 panic
    let empty = client.validate(ValidateRequest { envelope: None }).await;
    assert!(empty.is_err(), "缺信封应当回 Status 错误");

    // ---- Handle ----
    let out = client
        .handle(HandleRequest {
            envelope: Some(json_envelope(serde_json::json!({"text": "你好"}))),
        })
        .await
        .expect("Handle 应当应答")
        .into_inner();
    let envelope = out.envelope.expect("Handle 必须返回信封");
    let payload = hubkit::envelope::payload_json(&envelope).expect("输出应当是 JSON 载荷");
    assert_eq!(payload["echo"], "test-plugin 收到: 你好");

    // ---- HandleStream ----
    let mut stream = client
        .handle_stream(HandleRequest {
            envelope: Some(json_envelope(serde_json::json!({"text": "流"}))),
        })
        .await
        .expect("HandleStream 应当应答")
        .into_inner();

    let mut items = Vec::new();
    while let Some(item) = stream.message().await.expect("流上不应出错") {
        items.push(item);
    }
    assert_eq!(items.len(), 1, "默认实现应当把单条结果当成一条流");
    let payload =
        hubkit::envelope::payload_json(items[0].envelope.as_ref().expect("流里的条目也要有信封"))
            .expect("流里的载荷应当是 JSON");
    assert_eq!(payload["echo"], "test-plugin 收到: 流");

    stop.send(()).unwrap();
    mock.stop();
}

#[tokio::test]
async fn 插件体报错时回_internal_而不是空信封() {
    // 中台要能区分「插件明确失败了」与「插件返回了一份空结果」：
    // 前者该重试，后者会把一个空信封当成业务数据往下传。
    struct Failing;

    #[async_trait::async_trait]
    impl hubkit::Plugin for Failing {
        fn manifest(&self) -> hubkit::PluginManifest {
            hubkit::PluginManifest {
                name: "failing-plugin".to_string(),
                version: "0.1.0".to_string(),
                consumes: vec![hubkit::proto::MessageContract {
                    fq_name: hubkit::envelope::STRUCT_FQ_NAME.to_string(),
                    description: String::new(),
                }],
                ..Default::default()
            }
        }

        async fn validate(
            &self,
            _envelope: &Envelope,
        ) -> Result<hubkit::ValidateResponse, hubkit::PluginError> {
            Ok(hubkit::envelope::valid())
        }

        async fn handle(&self, _envelope: Envelope) -> Result<Envelope, hubkit::PluginError> {
            Err(hubkit::PluginError::msg("下游系统连不上"))
        }
    }

    let mut mock = start_mock(MockConfig::with_interval(1)).await;
    let port = free_port();
    let config = Config {
        hub_addr: mock.url(),
        advertise_addr: local_addr(port),
        listen_addr: format!("127.0.0.1:{port}"),
        instance_id: "it-failing".to_string(),
        logger: Logger::new(Level::Error),
        register_retry_interval: Duration::from_millis(50),
        heartbeat_fallback_interval: Duration::from_millis(100),
        // 其余取缺省：这些用例与 HubState 无关，状态调用超时用缺省值即可
        ..Default::default()
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(hubkit::run_with_shutdown(Failing, config, async move {
        let _ = stop_rx.await;
    }));
    assert!(mock.wait_for_registrations(1, Duration::from_secs(5)).await);

    let mut client = connect_runtime(&local_addr(port)).await;
    let status = client
        .handle(HandleRequest {
            envelope: Some(json_envelope(serde_json::json!({"text": "x"}))),
        })
        .await
        .expect_err("插件体报错时应当回 Status 错误");

    assert_eq!(status.code(), tonic::Code::Internal);
    assert!(
        status.message().contains("下游系统连不上"),
        "错误信息应当带上插件自己的原因：{}",
        status.message()
    );

    let _ = stop_tx.send(());
    mock.stop();
}
