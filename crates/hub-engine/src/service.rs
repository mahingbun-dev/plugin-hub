//! 编排服务：把定义、校验、执行与留痕串起来。
//!
//! 执行引擎（[`crate::flow`]）是纯的——不碰数据库。这一层负责：
//!
//! - 保存草稿前**先校验**，把问题连同定义一起存下来
//! - 发布前**再校验一次**（草稿可能是在校验通过之后被改坏的）
//! - 每次执行落成 run / run_node 记录，控制台与 MCP 才能回答「这条数据卡在哪一跳」
//!
//! 落库刻意**只存摘要不存报文**：存储与合规都受不了全量（见 docs/design.md）。

use std::collections::BTreeMap;
use std::sync::Arc;

use hub_flow::{
    FlowDefinition, FlowIssue, PluginAvailability, PluginVersion, has_errors, summarize, validate,
};
use hub_observe::{ExportedSpan, NoopExporter, SpanExporter};
use hub_proto::v1::Envelope;
use hub_store::Store;
use hub_store::StoreError;
use hub_store::flows::{self, DraftInput, FlowRow};
use hub_store::model::FlowRevisionRow;
use hub_store::runs::{NewRun, NewRunNode};
use serde_json::json;

use crate::flow::{FlowExecutor, FlowRun, RunStatus};

/// 保存草稿的结果。
#[derive(Debug, Clone)]
pub struct SaveDraftOutcome {
    pub revision: FlowRevisionRow,

    /// 校验发现的问题（可能有警告而无错误）
    pub issues: Vec<FlowIssue>,

    /// 有阻断性问题时为 true——此时不能发布
    pub blocked: bool,
}

/// 触发一次执行的结果。
#[derive(Debug)]
pub struct TriggerOutcome {
    pub run: FlowRun,

    /// 用的是哪一个修订版本（排障时要知道「跑的是哪一版编排」）
    pub flow_revision: i32,
}

#[derive(Clone)]
pub struct FlowService {
    store: Store,
    executor: FlowExecutor,

    /// span 导出器。没配 trace 后端时是 no-op——导出是旁路，不该给主链路添依赖。
    exporter: Arc<dyn SpanExporter>,
}

impl FlowService {
    pub fn new(store: Store, executor: FlowExecutor) -> Self {
        Self {
            store,
            executor,
            exporter: Arc::new(NoopExporter),
        }
    }

    /// 配上 span 导出器。
    pub fn with_exporter(mut self, exporter: Arc<dyn SpanExporter>) -> Self {
        self.exporter = exporter;
        self
    }

    /// 保存草稿。
    ///
    /// **校验不通过也存**——编排是一步步改出来的，中途存不下来会很别扭；
    /// 能不能发布由 [`SaveDraftOutcome::blocked`] 表达。
    pub async fn save_draft(
        &self,
        name: &str,
        description: &str,
        definition: &FlowDefinition,
        created_by: &str,
    ) -> Result<SaveDraftOutcome, StoreError> {
        if definition.name != name {
            return Err(StoreError::Invalid(format!(
                "编排定义里的名字（{}）与请求的 flow 名（{name}）不一致",
                definition.name
            )));
        }

        let availability = self.availability().await?;
        let issues = validate(definition, &availability);
        let blocked = has_errors(&issues);

        let definition_value = serde_json::to_value(definition)
            .map_err(|err| StoreError::Invalid(format!("编排定义无法序列化: {err}")))?;
        let validation = serde_json::to_value(&issues).ok();

        let revision = flows::upsert_draft(
            self.store.pool(),
            &DraftInput {
                name,
                description,
                definition: &definition_value,
                validation: validation.as_ref(),
                created_by,
            },
        )
        .await?;

        Ok(SaveDraftOutcome {
            revision,
            issues,
            blocked,
        })
    }

    /// 发布草稿。
    ///
    /// 发布前**重新校验**：草稿可能是在上次校验通过之后被改坏的，而发布决定生产
    /// 流量走向——这一道不能省。
    pub async fn publish(&self, name: &str) -> Result<FlowRevisionRow, StoreError> {
        let flow = self.find_flow_or_fail(name).await?;

        let draft = flows::find_draft(self.store.pool(), flow.id)
            .await?
            .ok_or_else(|| StoreError::Invalid(format!("flow {name} 没有草稿可发布")))?;

        let definition: FlowDefinition = serde_json::from_value(draft.definition.clone())
            .map_err(|err| StoreError::Invalid(format!("草稿无法解析为编排定义: {err}")))?;

        let availability = self.availability().await?;
        let issues = validate(&definition, &availability);
        if has_errors(&issues) {
            return Err(StoreError::Invalid(format!(
                "编排未通过校验，不能发布：{}",
                summarize(&issues).unwrap_or_else(|| "存在阻断性问题".to_string())
            )));
        }

        flows::publish_draft(self.store.pool(), name).await
    }

    /// 给从未发布过的 flow 改名。
    ///
    /// 新名字沿用编排 DSL 的同一套字符集校验（[`hub_flow::is_valid_name`]）：
    /// 名字要进 URL、要能当 MCP 工具名的一部分，标准不能比保存草稿时松。
    /// 存储层负责挡住「已发布不许改」与「新名字撞车」。
    pub async fn rename_flow(&self, name: &str, new_name: &str) -> Result<FlowRow, StoreError> {
        if !hub_flow::is_valid_name(new_name) {
            return Err(StoreError::Invalid(format!(
                "新名字 {new_name} 不合法：只能由字母、数字、下划线、中划线组成，且以字母或数字开头"
            )));
        }
        flows::rename_flow(self.store.pool(), name, new_name).await
    }

    /// 触发一条已发布的 flow。
    ///
    /// `trigger` 是触发来源的描述（HTTP / MCP / cron / MQ），落库供排障时看「谁把它跑起来的」。
    pub async fn trigger(
        &self,
        flow_name: &str,
        envelope: Envelope,
        trigger: Option<serde_json::Value>,
    ) -> Result<TriggerOutcome, StoreError> {
        let flow = self.find_flow_or_fail(flow_name).await?;

        let revision = flows::find_published(self.store.pool(), flow.id)
            .await?
            .ok_or_else(|| StoreError::Invalid(format!("flow {flow_name} 尚未发布，不能触发")))?;

        let definition: FlowDefinition = serde_json::from_value(revision.definition.clone())
            .map_err(|err| StoreError::Invalid(format!("已发布的编排无法解析: {err}")))?;

        let input_summary = envelope_summary(&envelope);
        let run = self.executor.run(&definition, envelope).await;

        self.record_run(&flow, revision.revision, &run, &input_summary, trigger)
            .await?;

        Ok(TriggerOutcome {
            run,
            flow_revision: revision.revision,
        })
    }

    /// 列出已发布的编排（控制台与 MCP 用）。
    pub async fn list_published(&self) -> Result<Vec<(FlowRow, FlowRevisionRow)>, StoreError> {
        let mut out = Vec::new();
        for flow in flows::list_flows(self.store.pool()).await? {
            if flow.published_revision == 0 {
                continue;
            }
            if let Some(revision) =
                flows::find_revision(self.store.pool(), flow.id, flow.published_revision).await?
            {
                out.push((
                    FlowRow {
                        id: flow.id,
                        name: flow.name.clone(),
                        description: flow.description.clone(),
                        published_revision: flow.published_revision,
                    },
                    revision,
                ));
            }
        }
        Ok(out)
    }

    async fn find_flow_or_fail(&self, name: &str) -> Result<FlowRow, StoreError> {
        flows::find_flow(self.store.pool(), name)
            .await?
            .ok_or_else(|| StoreError::Invalid(format!("flow {name} 未定义")))
    }

    /// 把中台注册表里的插件契约整理成校验器要的形状。
    ///
    /// 版本**按新到旧**排列——校验器据此把「取最新版本」解析成具体版本。
    async fn availability(&self) -> Result<BTreeMap<String, PluginAvailability>, StoreError> {
        let rows = hub_store::plugins::versions_with_contracts(self.store.pool()).await?;

        let mut map: BTreeMap<String, PluginAvailability> = BTreeMap::new();
        for row in rows {
            map.entry(row.plugin.clone())
                .or_insert_with(|| PluginAvailability {
                    plugin: row.plugin.clone(),
                    versions: Vec::new(),
                })
                .versions
                .push(PluginVersion {
                    version: row.version,
                    produces: row.produces,
                    consumes: row.consumes,
                });
        }
        Ok(map)
    }

    /// 落执行记录。
    async fn record_run(
        &self,
        flow: &FlowRow,
        flow_revision: i32,
        run: &FlowRun,
        input_summary: &str,
        trigger: Option<serde_json::Value>,
    ) -> Result<(), StoreError> {
        let now = chrono::Utc::now();

        // 起点取**最早开始的节点**，与 `FlowRun::spans` 里根 span 的算法一致。
        //
        // 这里原来直接用 `now`（结算时刻）同时充当 started_at 与 finished_at，
        // 于是 `runs.started_at` 记的是「结算发生的时刻」而不是「这次执行何时开始」：
        // 两列恒等 → 控制台按起止时间算执行耗时，算出来永远是 0；
        // 列表按 started_at 排序排的也是结算顺序。
        // 排不出计划时（没有节点）才退回 now——那种执行本来就是当场失败的。
        let started_at = run.nodes.iter().map(|n| n.started_at).min().unwrap_or(now);

        hub_store::runs::upsert_run(
            self.store.pool(),
            &NewRun {
                run_id: &run.run_id,
                flow_id: flow.id,
                flow_revision,
                trace_id: &run.trace_id,
                subject: None,
                trigger: trigger.as_ref(),
                status: status_str(run.status),
                input_summary: Some(input_summary),
                error: run.error.as_deref(),
                started_at,
                finished_at: Some(now),
            },
        )
        .await?;

        for node in &run.nodes {
            hub_store::runs::insert_run_node(
                self.store.pool(),
                &NewRunNode {
                    run_id: &run.run_id,
                    node_id: &node.node_id,
                    plugin: &node.plugin,
                    version: &node.version,
                    instance_id: if node.instance_id.is_empty() {
                        None
                    } else {
                        Some(node.instance_id.as_str())
                    },
                    attempt: node.attempts as i32,
                    status: node_status_str(node.status),
                    duration_ms: Some(node.duration_ms as i64),
                    io_summary: None,
                    error: node.error.as_deref(),
                    started_at: node.started_at,
                    finished_at: Some(
                        node.started_at + chrono::Duration::milliseconds(node.duration_ms as i64),
                    ),
                },
            )
            .await?;
        }

        // 调用链 span：控制台按 traceId 查「这条数据经过了哪些插件、在哪一跳慢了」。
        // 自存而不是只依赖 OTel 后端——采样后的 trace 后端给不了这个确定性。
        let spans = run.spans(&flow.name);
        for span in &spans {
            hub_store::spans::insert_span(
                self.store.pool(),
                &hub_store::spans::NewSpan {
                    trace_id: &run.trace_id,
                    span_id: &span.span_id,
                    parent_span_id: span.parent_span_id.as_deref(),
                    run_id: Some(&run.run_id),
                    node_id: span.node_id.as_deref(),
                    name: &span.name,
                    started_at: span.started_at,
                    duration_ms: span.duration_ms as i64,
                    status: &span.status,
                    attributes: Some(&span.attributes),
                },
            )
            .await?;
        }

        // 顺便推一份给标准 trace 后端。
        //
        // **spawn 而不是 await**：导出是旁路，让请求路径去等一次外部 HTTP 往返没有道理；
        // 失败也只记日志——编排跑不跑得通与 trace 后端在不在没关系。
        // 没配端点时 exporter 是 no-op，这里什么也不会发生。
        let exported: Vec<ExportedSpan> = spans
            .into_iter()
            .map(|span| ExportedSpan {
                trace_id: run.trace_id.clone(),
                span_id: span.span_id,
                parent_span_id: span.parent_span_id,
                name: span.name,
                started_at_unix_nano: span.started_at.timestamp_nanos_opt().unwrap_or_default(),
                duration_ms: span.duration_ms as i64,
                status: span.status,
                attributes: span.attributes,
            })
            .collect();

        let exporter = Arc::clone(&self.exporter);
        tokio::spawn(async move {
            if let Err(err) = exporter.export(exported).await {
                tracing::warn!(error = %err, "推送 span 到 trace 后端失败（不影响主链路）");
            }
        });

        Ok(())
    }
}

fn status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Rejected => "rejected",
    }
}

fn node_status_str(status: crate::flow::NodeStatus) -> &'static str {
    match status {
        crate::flow::NodeStatus::Succeeded => "succeeded",
        crate::flow::NodeStatus::Failed => "failed",
        crate::flow::NodeStatus::Rejected => "rejected",
    }
}

/// 信封摘要。落库用——**不落全量报文**。
///
/// 留的是「这条数据长什么样、有多大」，够定位问题；真要看内容得去插件日志，
/// 那是刻意的：全量落库的存储与合规代价都受不了。
fn envelope_summary(envelope: &Envelope) -> String {
    let (type_url, payload_bytes) = envelope
        .payload
        .as_ref()
        .map(|p| (p.type_url.clone(), p.value.len()))
        .unwrap_or_default();

    let mut meta_keys: Vec<&str> = envelope.meta.keys().map(String::as_str).collect();
    meta_keys.sort_unstable();

    json!({
        "message_id": envelope.message_id,
        "payload_type": type_url,
        "payload_bytes": payload_bytes,
        "meta_keys": meta_keys,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::Any;

    #[test]
    fn 摘要是可读的而不是哈希() {
        let mut envelope = Envelope {
            message_id: "msg-1".to_string(),
            payload: Some(Any {
                type_url: "type.googleapis.com/google.protobuf.Struct".to_string(),
                value: vec![0u8; 42],
            }),
            ..Default::default()
        };
        envelope.meta.insert("tenant".to_string(), "t1".to_string());
        envelope.meta.insert("region".to_string(), "cn".to_string());

        let summary: serde_json::Value =
            serde_json::from_str(&envelope_summary(&envelope)).expect("应是 JSON");

        assert_eq!(summary["message_id"], "msg-1");
        assert_eq!(summary["payload_bytes"], 42);
        assert_eq!(
            summary["meta_keys"],
            json!(["region", "tenant"]),
            "meta 键要排序，否则同一条数据两次记录会不一样"
        );
    }

    #[test]
    fn 没有载荷时摘要也不崩() {
        let summary: serde_json::Value =
            serde_json::from_str(&envelope_summary(&Envelope::default())).expect("应是 JSON");
        assert_eq!(summary["payload_bytes"], 0);
        assert_eq!(summary["payload_type"], "");
    }

    #[test]
    fn 状态映射与数据库约束一致() {
        // 数据库对这两列有 CHECK 约束，映射写错会在插入时才炸
        for status in [RunStatus::Succeeded, RunStatus::Failed, RunStatus::Rejected] {
            assert!(
                ["succeeded", "failed", "rejected"].contains(&status_str(status)),
                "{status:?} 的映射不在数据库允许的取值里"
            );
        }
        for status in [
            crate::flow::NodeStatus::Succeeded,
            crate::flow::NodeStatus::Failed,
            crate::flow::NodeStatus::Rejected,
        ] {
            assert!(["succeeded", "failed", "rejected"].contains(&node_status_str(status)));
        }
    }
}
