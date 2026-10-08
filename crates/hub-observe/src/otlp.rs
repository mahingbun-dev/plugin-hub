//! OTLP/HTTP 导出：把调用链 span 推给标准后端（Tempo / Jaeger / 各类 collector）。
//!
//! 用 OTLP/HTTP 的 JSON 编码，**而不是引 opentelemetry 那一套 SDK**：
//! UAT 目前没有 trace 后端，引进来的一堆依赖无法端到端验证；而 OTLP/HTTP 本质上
//! 就是「按固定形状 POST 一段 JSON」，自己编码反而能对着 mock collector 真测。

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

/// 一条待导出的 span。
///
/// 与中台自存的 `SpanRecord` 对应；id 都是 W3C 的十六进制形式（trace 32 位、span 16 位），
/// 这正是 OTLP 要求的格式——两边不用转换。
#[derive(Debug, Clone, PartialEq)]
pub struct ExportedSpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,

    /// Unix 纳秒。OTLP 用它而不是毫秒。
    pub started_at_unix_nano: i64,

    pub duration_ms: i64,

    /// `ok` / `error` / `rejected`
    pub status: String,

    pub attributes: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExportError {
    #[error("构造导出请求失败: {0}")]
    Request(String),

    #[error("推送 span 失败: {0}")]
    Transport(String),

    #[error("collector 拒绝了这批 span：HTTP {status} {body}")]
    Rejected { status: u16, body: String },
}

/// span 导出器。
///
/// **导出是旁路**：失败只记日志，绝不影响主链路——编排跑不跑得通与
/// trace 后端在不在没关系。
#[async_trait]
pub trait SpanExporter: Send + Sync + 'static {
    async fn export(&self, spans: Vec<ExportedSpan>) -> Result<(), ExportError>;
}

/// 什么都不做的导出器。没配 OTLP 端点时用它。
pub struct NoopExporter;

#[async_trait]
impl SpanExporter for NoopExporter {
    async fn export(&self, _spans: Vec<ExportedSpan>) -> Result<(), ExportError> {
        Ok(())
    }
}

/// OTLP/HTTP 导出器的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpExporterConfig {
    /// collector 的 OTLP/HTTP 端点，例如 `http://127.0.0.1:4318/v1/traces`
    pub endpoint: String,

    /// `service.name` 资源属性，后端靠它区分来源
    pub service_name: String,

    pub timeout: Duration,
}

impl OtlpExporterConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            service_name: "anc-hub".to_string(),
            timeout: Duration::from_secs(5),
        }
    }
}

/// OTLP/HTTP 导出器。
pub struct OtlpExporter {
    cfg: OtlpExporterConfig,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl OtlpExporter {
    pub fn new(cfg: OtlpExporterConfig) -> Self {
        let client = Client::builder(TokioExecutor::new()).build_http();
        Self { cfg, client }
    }

    pub fn endpoint(&self) -> &str {
        &self.cfg.endpoint
    }
}

#[async_trait]
impl SpanExporter for OtlpExporter {
    async fn export(&self, spans: Vec<ExportedSpan>) -> Result<(), ExportError> {
        if spans.is_empty() {
            return Ok(());
        }

        let payload = encode(&spans, &self.cfg.service_name);
        let body = serde_json::to_vec(&payload)
            .map_err(|err| ExportError::Request(format!("载荷无法序列化: {err}")))?;

        let request = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(&self.cfg.endpoint)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|err| ExportError::Request(err.to_string()))?;

        let response = tokio::time::timeout(self.cfg.timeout, self.client.request(request))
            .await
            .map_err(|_| ExportError::Transport(format!("推送超时（{:?}）", self.cfg.timeout)))?
            .map_err(|err| ExportError::Transport(err.to_string()))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body = response
            .into_body()
            .collect()
            .await
            .map(|collected| String::from_utf8_lossy(&collected.to_bytes()).to_string())
            .unwrap_or_default();

        Err(ExportError::Rejected {
            status: status.as_u16(),
            body,
        })
    }
}

/// 把 span 列表编成 OTLP/HTTP 的 JSON 载荷。
///
/// 纯函数，便于把「编码形状对不对」与「网络能不能通」分开测。
pub fn encode(spans: &[ExportedSpan], service_name: &str) -> Value {
    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [attribute("service.name", &json!(service_name))]
            },
            "scopeSpans": [{
                "scope": { "name": "anc-hub" },
                "spans": spans.iter().map(encode_span).collect::<Vec<_>>()
            }]
        }]
    })
}

fn encode_span(span: &ExportedSpan) -> Value {
    let end_nano = span.started_at_unix_nano + span.duration_ms.saturating_mul(1_000_000);

    let mut out = json!({
        "traceId": span.trace_id,
        "spanId": span.span_id,
        "name": span.name,
        // SPAN_KIND_INTERNAL：这些是天生的内部 span，不是 server/client
        "kind": 1,
        // OTLP 用纳秒且**以字符串传 64 位整数**（JSON 的 number 精度不够）
        "startTimeUnixNano": span.started_at_unix_nano.to_string(),
        "endTimeUnixNano": end_nano.to_string(),
        "status": { "code": status_code(&span.status) },
    });

    if let Some(parent) = &span.parent_span_id {
        out["parentSpanId"] = json!(parent);
    }

    let attributes: Vec<Value> = span
        .attributes
        .as_object()
        .map(|map| {
            map.iter()
                // null 在 OTLP 里没有对应表示，丢掉比编成空串诚实
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| attribute(key, value))
                .collect()
        })
        .unwrap_or_default();

    if !attributes.is_empty() {
        out["attributes"] = json!(attributes);
    }

    out
}

/// OTLP 的 StatusCode：UNSET=0 / OK=1 / ERROR=2。
///
/// 中台的 `rejected` 是业务结果而不是故障，但在 trace 后端看来它就是「这次没成功」，
/// 所以归到 ERROR——在那里区分业务与否没有意义，那是中台自己的事。
fn status_code(status: &str) -> i32 {
    match status {
        "ok" => 1,
        _ => 2,
    }
}

fn attribute(key: &str, value: &Value) -> Value {
    json!({ "key": key, "value": any_value(value) })
}

/// serde_json 的 Value → OTLP 的 AnyValue。
fn any_value(value: &Value) -> Value {
    match value {
        Value::Null => json!({ "stringValue": "" }),
        Value::Bool(b) => json!({ "boolValue": b }),
        // OTLP 的 int64 用字符串表示（JSON number 装不下 64 位整数）
        Value::Number(n) if n.is_i64() => {
            json!({ "intValue": n.as_i64().unwrap_or_default().to_string() })
        }
        Value::Number(n) if n.is_u64() => {
            json!({ "intValue": n.as_u64().unwrap_or_default().to_string() })
        }
        Value::Number(n) => json!({ "doubleValue": n.as_f64().unwrap_or_default() }),
        Value::String(s) => json!({ "stringValue": s }),
        Value::Array(items) => json!({
            "arrayValue": { "values": items.iter().map(any_value).collect::<Vec<_>>() }
        }),
        Value::Object(map) => json!({
            "kvlistValue": {
                "values": map
                    .iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| attribute(k, v))
                    .collect::<Vec<_>>()
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(name: &str) -> ExportedSpan {
        ExportedSpan {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_string(),
            span_id: "00f067aa0ba902b7".to_string(),
            parent_span_id: None,
            name: name.to_string(),
            started_at_unix_nano: 1_700_000_000_000_000_000,
            duration_ms: 42,
            status: "ok".to_string(),
            attributes: json!({"plugin": "auth", "attempts": 1}),
        }
    }

    #[test]
    fn 编码出_otlp_要求的顶层结构() {
        let payload = encode(&[span("flow:intake")], "anc-hub");

        let resource_spans = &payload["resourceSpans"][0];
        assert_eq!(
            resource_spans["resource"]["attributes"][0]["key"],
            "service.name"
        );
        assert_eq!(
            resource_spans["resource"]["attributes"][0]["value"]["stringValue"],
            "anc-hub"
        );

        let spans = &resource_spans["scopeSpans"][0]["spans"];
        assert_eq!(spans.as_array().map(Vec::len), Some(1));
        assert_eq!(spans[0]["name"], "flow:intake");
        assert_eq!(spans[0]["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[test]
    fn 时间戳用纳秒字符串而不是数字() {
        let payload = encode(&[span("s")], "hub");
        let s = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];

        // OTLP 用纳秒，且 64 位整数必须以字符串传——JSON number 精度不够
        assert_eq!(s["startTimeUnixNano"], "1700000000000000000");
        assert_eq!(
            s["endTimeUnixNano"], "1700000000042000000",
            "结束时间应为开始 + 42ms"
        );
    }

    #[test]
    fn 没有父_span_时不写_parent_span_id_字段() {
        let payload = encode(&[span("root")], "hub");
        let s = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert!(s.get("parentSpanId").is_none(), "根 span 不该有父字段");

        let mut child = span("child");
        child.parent_span_id = Some("0000000000000001".to_string());
        let payload = encode(&[child], "hub");
        let s = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(s["parentSpanId"], "0000000000000001");
    }

    #[test]
    fn 状态码按_otlp_取值映射() {
        let mut ok = span("s");
        ok.status = "ok".to_string();
        let mut failed = span("s");
        failed.status = "error".to_string();
        let mut rejected = span("s");
        rejected.status = "rejected".to_string();

        let payload = encode(&[ok, failed, rejected], "hub");
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .expect("应有 span")
            .clone();

        assert_eq!(spans[0]["status"]["code"], 1, "ok → STATUS_CODE_OK");
        assert_eq!(spans[1]["status"]["code"], 2, "error → STATUS_CODE_ERROR");
        assert_eq!(
            spans[2]["status"]["code"], 2,
            "rejected 在 trace 后端看来也是没成功"
        );
    }

    #[test]
    fn 属性按类型编成对应的_any_value() {
        let mut s = span("s");
        s.attributes = json!({
            "text": "hello",
            "count": 3,
            "ratio": 1.5,
            "flag": true,
            "list": ["a", "b"],
            "nested": {"k": "v"},
            "gone": null
        });

        let payload = encode(&[s], "hub");
        let attrs = payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"]
            .as_array()
            .expect("应有属性")
            .clone();

        let find = |key: &str| {
            attrs
                .iter()
                .find(|a| a["key"] == key)
                .unwrap_or_else(|| panic!("缺少属性 {key}"))
        };

        assert_eq!(find("text")["value"]["stringValue"], "hello");
        assert_eq!(
            find("count")["value"]["intValue"],
            "3",
            "int64 要以字符串传"
        );
        assert_eq!(find("ratio")["value"]["doubleValue"], 1.5);
        assert_eq!(find("flag")["value"]["boolValue"], true);
        assert_eq!(
            find("list")["value"]["arrayValue"]["values"][0]["stringValue"],
            "a"
        );
        assert_eq!(
            find("nested")["value"]["kvlistValue"]["values"][0]["key"],
            "k"
        );

        assert!(
            !attrs.iter().any(|a| a["key"] == "gone"),
            "null 在 OTLP 里没有对应表示，应被丢掉而不是编成空串"
        );
    }

    #[test]
    fn 没有属性时不写_attributes_字段() {
        let mut s = span("s");
        s.attributes = json!({});
        let payload = encode(&[s], "hub");
        let span_json = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert!(span_json.get("attributes").is_none());
    }

    #[test]
    fn 空列表也能编码出合法结构() {
        let payload = encode(&[], "hub");
        let spans = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"];
        assert_eq!(spans.as_array().map(Vec::len), Some(0));
    }

    #[tokio::test]
    async fn 空列表不发起请求() {
        // 端点故意填一个连不上的地址：没有 span 时不该有任何网络动作
        let exporter = OtlpExporter::new(OtlpExporterConfig::new("http://127.0.0.1:1"));
        assert!(exporter.export(vec![]).await.is_ok());
    }

    #[tokio::test]
    async fn 不可达的_collector_报传输错误() {
        let exporter = OtlpExporter::new(OtlpExporterConfig::new("http://127.0.0.1:1"));
        let err = exporter.export(vec![span("s")]).await.expect_err("应报错");
        assert!(
            matches!(err, ExportError::Transport(_)),
            "应是传输失败，实际 {err:?}"
        );
    }

    #[tokio::test]
    async fn noop_导出器永远成功() {
        let exporter = NoopExporter;
        assert!(exporter.export(vec![span("s")]).await.is_ok());
    }
}
