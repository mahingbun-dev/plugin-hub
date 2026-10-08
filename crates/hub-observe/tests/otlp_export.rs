//! OTLP 导出对着 **mock collector** 的真实验证。
//!
//! 只测编码形状不够——「HTTP 发出去、collector 收到、内容正确」这一整条链路
//! 才是导出器真正要负责的事。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use hub_observe::{ExportedSpan, OtlpExporter, OtlpExporterConfig, SpanExporter};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// 一个只记录收到什么的 mock collector。
#[derive(Clone, Default)]
struct Collector {
    received: Arc<Mutex<Vec<Value>>>,
}

impl Collector {
    async fn start() -> (String, Self) {
        let state = Self::default();
        let app = Router::new()
            .route("/v1/traces", post(collect))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("监听失败");
        let addr: SocketAddr = listener.local_addr().expect("取地址失败");

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/v1/traces"), state)
    }

    fn payloads(&self) -> Vec<Value> {
        self.received.lock().expect("锁中毒").clone()
    }

    async fn wait_for(&self, n: usize) -> Vec<Value> {
        for _ in 0..100 {
            let got = self.payloads();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("等了 2 秒只收到 {} 批", self.payloads().len());
    }
}

async fn collect(State(state): State<Collector>, Json(body): Json<Value>) -> &'static str {
    state.received.lock().expect("锁中毒").push(body);
    ""
}

fn span(name: &str) -> ExportedSpan {
    ExportedSpan {
        trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_string(),
        span_id: "00f067aa0ba902b7".to_string(),
        parent_span_id: Some("0000000000000001".to_string()),
        name: name.to_string(),
        started_at_unix_nano: 1_700_000_000_000_000_000,
        duration_ms: 42,
        status: "ok".to_string(),
        attributes: json!({"plugin": "auth"}),
    }
}

#[tokio::test]
async fn 导出的_span_能被_collector_收到且内容正确() {
    let (endpoint, collector) = Collector::start().await;
    let exporter = OtlpExporter::new(OtlpExporterConfig::new(endpoint));

    exporter
        .export(vec![span("flow:intake"), span("plugin:auth")])
        .await
        .expect("导出应成功");

    let payloads = collector.wait_for(1).await;
    let spans = payloads[0]["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .expect("应有 span 列表");

    assert_eq!(spans.len(), 2, "两个 span 应在同一批里");
    assert_eq!(spans[0]["name"], "flow:intake");
    assert_eq!(spans[1]["name"], "plugin:auth");
    assert_eq!(spans[0]["parentSpanId"], "0000000000000001");
    assert_eq!(spans[0]["status"]["code"], 1);
    assert_eq!(
        payloads[0]["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
        "plugin-hub",
        "资源属性要标明来源服务"
    );
}

#[tokio::test]
async fn 多次导出产生多批() {
    let (endpoint, collector) = Collector::start().await;
    let exporter = OtlpExporter::new(OtlpExporterConfig::new(endpoint));

    exporter.export(vec![span("s1")]).await.expect("第一次失败");
    exporter.export(vec![span("s2")]).await.expect("第二次失败");

    let payloads = collector.wait_for(2).await;
    assert_eq!(payloads.len(), 2);
    assert_eq!(
        payloads[1]["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["name"],
        "s2"
    );
}

#[tokio::test]
async fn collector_返回错误码时导出报错() {
    // 只回 500 的 collector
    let app = Router::new().route(
        "/v1/traces",
        post(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("监听失败");
    let addr = listener.local_addr().expect("取地址失败");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let exporter = OtlpExporter::new(OtlpExporterConfig::new(format!("http://{addr}/v1/traces")));

    let err = exporter.export(vec![span("s")]).await.expect_err("应报错");
    match err {
        hub_observe::ExportError::Rejected { status, body } => {
            assert_eq!(status, 500);
            assert!(body.contains("boom"), "应带上 collector 的响应体：{body}");
        }
        other => panic!("应是 Rejected，实际 {other:?}"),
    }
}

#[tokio::test]
async fn 端点路径不对时报错而不是静默成功() {
    let (endpoint, _collector) = Collector::start().await;
    // 把路径改错：collector 只挂了 /v1/traces
    let wrong = endpoint.replace("/v1/traces", "/nope");
    let exporter = OtlpExporter::new(OtlpExporterConfig::new(wrong));

    assert!(
        exporter.export(vec![span("s")]).await.is_err(),
        "404 必须被发现——静默成功会让 trace 悄悄丢失"
    );
}
