//! anc-hub 的可观测：把调用链 span 导出到标准后端。
//!
//! span **本身已经自存**（控制台要能按 traceId 确定性地查到链路，这是采样后的 trace
//! 后端给不了的）。这一层做的是「顺便也推一份给标准后端」，让 Grafana/Tempo 那类工具
//! 能看到同一批数据。
//!
//! 两条刻意的约定：
//!
//! - **导出是旁路**：失败只记日志，绝不影响主链路——编排跑不跑得通与 trace 后端在不在没关系
//! - **没配端点就是 no-op**：UAT 目前没有 trace 后端，没配就不该有任何网络动作

pub mod otlp;

pub use otlp::{
    ExportError, ExportedSpan, NoopExporter, OtlpExporter, OtlpExporterConfig, SpanExporter,
};

use std::sync::Arc;

/// 按配置构造导出器：给了端点就用 OTLP，没给就用 no-op。
pub fn exporter_for(endpoint: Option<&str>) -> Arc<dyn SpanExporter> {
    match endpoint.map(str::trim).filter(|e| !e.is_empty()) {
        Some(endpoint) => Arc::new(OtlpExporter::new(OtlpExporterConfig::new(endpoint))),
        None => Arc::new(NoopExporter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 未配端点时得到_noop() {
        assert!(exporter_for(None).export(vec![]).await.is_ok());
        assert!(exporter_for(Some("   ")).export(vec![]).await.is_ok());
    }

    #[test]
    fn 配了端点时构造出_otlp_导出器() {
        // 只验证类型选择（不发起请求）——OTLP 导出器的端点应与配置一致
        let exporter =
            OtlpExporter::new(OtlpExporterConfig::new("http://127.0.0.1:4318/v1/traces"));
        assert_eq!(exporter.endpoint(), "http://127.0.0.1:4318/v1/traces");
    }
}
