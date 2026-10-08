//! 同步链执行引擎：按执行计划跑完一条编排。
//!
//! 顺序语义（每一条都是刻意的）：
//!
//! - **同层并发、层间串行**：同层节点互不依赖，串行跑纯属浪费；层间有依赖，必须串。
//! - **失败即短路**：某个节点失败后，依赖它的下游拿不到数据，继续跑只会产生一串
//!   连带的假故障。已经在跑的兄弟节点让它跑完（中断它们反而会留下半截状态）。
//! - **trace 贯穿、run/node 每跳更新**：trace_id 全程不变（排障要的就是完整链路），
//!   run_id 与 node_id 逐跳更新（定位要的是「哪一跳」）。
//! - **message_id 不变**：它是**数据**的幂等键，同一条数据穿过多少插件都是同一个。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use hub_flow::{ExecutionPlan, FlowDefinition, Node, plan};
use hub_proto::v1::Envelope;
use hub_registry::{InstanceTarget, Registry};
use tokio::task::JoinSet;
use ulid::Ulid;

use crate::{InvokeOutcome, Invoker};

/// 默认的节点超时。节点没声明时用它。
pub const DEFAULT_NODE_TIMEOUT_MS: i64 = 30_000;

/// 重试的退避基数：第 n 次重试等 `BASE * 2^(n-1)`，封顶 2 秒。
const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(50);
const RETRY_BACKOFF_CAP: Duration = Duration::from_secs(2);

/// 节点的治理参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePolicy {
    /// 节点超时（毫秒）。真正生效的是它与信封剩余预算中**更小**的那个。
    pub timeout_ms: i64,

    /// 失败重试次数（不含首次）
    pub retries: u32,
}

impl NodePolicy {
    pub fn from_node(node: &Node) -> Self {
        Self {
            timeout_ms: node.timeout_ms.unwrap_or(DEFAULT_NODE_TIMEOUT_MS),
            retries: node.retries.unwrap_or(0),
        }
    }
}

/// 一次 flow 执行的结果。
#[derive(Debug)]
pub struct FlowRun {
    pub run_id: String,
    pub trace_id: String,
    pub status: RunStatus,

    /// 按声明顺序的节点结果（只含真正跑过的）
    pub nodes: Vec<NodeOutcome>,

    /// 叶子节点的输出。多叶子（扇出）时给出声明顺序里最后一个成功的。
    pub output: Option<Envelope>,

    pub error: Option<String>,
    pub elapsed_ms: u64,
}

impl FlowRun {
    pub fn succeeded(&self) -> bool {
        self.status == RunStatus::Succeeded
    }

    /// 在实际调用上花掉的毫秒数之和——与 [`Self::elapsed_ms`] 的差就是编排自身的开销。
    pub fn node_time_ms(&self) -> u64 {
        self.nodes.iter().map(|n| n.duration_ms).sum()
    }

    /// 把本次执行摊成 span 列表：一个根 span + 每个节点一个子 span。
    ///
    /// 从结果倒推而不是执行路径上埋点：结果里已经有 span 需要的一切（起止、状态、
    /// 节点身份），而在执行路径上多传一层状态只会让那段本就复杂的代码更难读。
    pub fn spans(&self, flow_name: &str) -> Vec<SpanRecord> {
        let root_id = format!("{}-root", self.run_id);

        // 根 span 的起点取最早开始的节点；没有节点时（排不出计划）用当前时刻
        let root_started_at = self
            .nodes
            .iter()
            .map(|n| n.started_at)
            .min()
            .unwrap_or_else(chrono::Utc::now);

        let mut spans = Vec::with_capacity(self.nodes.len() + 1);
        spans.push(SpanRecord {
            span_id: root_id.clone(),
            parent_span_id: None,
            name: format!("flow:{flow_name}"),
            node_id: None,
            started_at: root_started_at,
            duration_ms: self.elapsed_ms,
            status: match self.status {
                RunStatus::Succeeded => "ok",
                RunStatus::Failed => "error",
                RunStatus::Rejected => "rejected",
            }
            .to_string(),
            attributes: serde_json::json!({
                "run_id": self.run_id,
                "node_count": self.nodes.len(),
                "node_time_ms": self.node_time_ms(),
                "error": self.error,
            }),
        });

        for node in &self.nodes {
            spans.push(SpanRecord {
                span_id: format!("{}-{}", self.run_id, node.node_id),
                parent_span_id: Some(root_id.clone()),
                name: format!("plugin:{}", node.plugin),
                node_id: Some(node.node_id.clone()),
                started_at: node.started_at,
                duration_ms: node.duration_ms,
                status: match node.status {
                    NodeStatus::Succeeded => "ok",
                    NodeStatus::Failed => "error",
                    NodeStatus::Rejected => "rejected",
                }
                .to_string(),
                attributes: serde_json::json!({
                    "plugin": node.plugin,
                    "version": node.version,
                    "instance_id": node.instance_id,
                    "attempts": node.attempts,
                    "error": node.error,
                }),
            });
        }

        spans
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Succeeded,
    /// 插件调用失败（不可达、超时、报错）
    Failed,
    /// 插件的校验器拒绝了数据——链路按设计短路
    Rejected,
}

#[derive(Debug, Clone)]
pub struct NodeOutcome {
    pub node_id: String,
    pub plugin: String,
    pub version: String,
    pub instance_id: String,
    pub status: NodeStatus,

    /// 实际尝试次数（含首次）
    pub attempts: u32,

    /// 节点开始时刻。span 用它还原「同一时刻在跑哪些节点」——同层并发时
    /// 光有 duration 是拼不出时间线的。
    pub started_at: chrono::DateTime<chrono::Utc>,

    pub duration_ms: u64,
    pub error: Option<String>,
}

/// 调用链上的一个 span。
///
/// 字段按 OTel 的形状组织：将来要接 OTel 后端时这是机械转换，不用改数据模型。
#[derive(Debug, Clone)]
pub struct SpanRecord {
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub node_id: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: u64,
    pub status: String,
    pub attributes: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    Succeeded,
    Failed,
    Rejected,
}

/// 单节点的执行结果。
///
/// 公开是因为异步执行器（`crate::async_exec`）逐节点推进，需要拿到单个节点的产出；
/// 同步引擎把它当作一层的中间结果。
#[derive(Debug)]
pub struct NodeRun {
    pub outcome: NodeOutcome,

    /// 成功时的输出信封；失败或拒绝时为 `None`
    pub output: Option<Envelope>,
}

#[derive(Clone)]
pub struct FlowExecutor {
    registry: Registry,
    invoker: Invoker,
}

impl FlowExecutor {
    pub fn new(registry: Registry, invoker: Invoker) -> Self {
        Self { registry, invoker }
    }

    /// 跑完一条编排。
    ///
    /// `envelope` 是触发时的那份信封：入口节点收到它的副本，载荷就是触发时的载荷。
    pub async fn run(&self, flow: &FlowDefinition, envelope: Envelope) -> FlowRun {
        let started = Instant::now();
        let run_id = Ulid::generate().to_string();
        let trace_id = envelope.trace_id.clone();

        let plan = match plan(flow) {
            Ok(plan) => plan,
            Err(err) => {
                return FlowRun {
                    run_id,
                    trace_id,
                    status: RunStatus::Failed,
                    nodes: Vec::new(),
                    output: None,
                    error: Some(format!("编排排不出执行计划: {err}")),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                };
            }
        };

        let mut outputs: HashMap<String, Envelope> = HashMap::new();
        let mut outcomes: Vec<NodeOutcome> = Vec::new();
        let mut failure: Option<(RunStatus, String)> = None;

        for level in &plan.levels {
            let results = self
                .run_level(flow, &plan, level, &envelope, &outputs, &run_id, &trace_id)
                .await;

            // 按声明顺序处理，保证结果可复现
            for (node_id, result) in level.iter().zip(results) {
                let node_run = match result {
                    Ok(node_run) => node_run,
                    Err(err) => {
                        // 只可能在计划不完整时出现（校验已保证不会）
                        failure =
                            Some((RunStatus::Failed, format!("节点 {node_id} 未能执行: {err}")));
                        break;
                    }
                };

                if let Some(output) = node_run.output {
                    outputs.insert(node_id.clone(), output);
                }

                let status = node_run.outcome.status;
                let error = node_run.outcome.error.clone();
                let plugin = node_run.outcome.plugin.clone();
                outcomes.push(node_run.outcome);

                if status != NodeStatus::Succeeded {
                    let run_status = match status {
                        NodeStatus::Rejected => RunStatus::Rejected,
                        _ => RunStatus::Failed,
                    };
                    failure = Some((
                        run_status,
                        format!(
                            "节点 {node_id}（{plugin}）{}",
                            error.unwrap_or_else(|| "执行失败".to_string())
                        ),
                    ));
                    break;
                }
            }

            if failure.is_some() {
                break;
            }
        }

        let (status, error) = match failure {
            Some((status, message)) => (status, Some(message)),
            None => (RunStatus::Succeeded, None),
        };

        // 输出取叶子节点里声明顺序最靠后的那个成功的
        let output = if status == RunStatus::Succeeded {
            flow.nodes
                .iter()
                .rev()
                .filter(|n| plan.is_leaf(&n.id))
                .find_map(|n| outputs.get(&n.id).cloned())
        } else {
            None
        };

        FlowRun {
            run_id,
            trace_id,
            status,
            nodes: outcomes,
            output,
            error,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }
    }

    /// 跑一层：同层节点并发，返回顺序与入参一致。
    #[allow(clippy::too_many_arguments)]
    async fn run_level(
        &self,
        flow: &FlowDefinition,
        plan: &ExecutionPlan,
        level: &[String],
        trigger: &Envelope,
        outputs: &HashMap<String, Envelope>,
        run_id: &str,
        trace_id: &str,
    ) -> Vec<Result<NodeRun, String>> {
        let mut set: JoinSet<(usize, Result<NodeRun, String>)> = JoinSet::new();

        for (index, node_id) in level.iter().enumerate() {
            let Some(node) = flow.node(node_id) else {
                continue;
            };

            // 入口节点收触发信封；其余收上游的输出
            let input = match plan.upstream_of(node_id) {
                Some(upstream) => outputs.get(upstream).cloned(),
                None => Some(trigger.clone()),
            };
            let policy = NodePolicy::from_node(node);
            let executor = self.clone();
            let node = node.clone();
            let node_label = node_id.clone();
            let run_id = run_id.to_string();
            let trace_id = trace_id.to_string();
            let trigger = trigger.clone();

            set.spawn(async move {
                let Some(input) = input else {
                    // 上游失败时它的输出不存在，而失败已经短路了整条链路——
                    // 走到这里说明上游被跳过了
                    return (index, Err(format!("节点 {node_label} 的上游没有产出数据")));
                };

                let envelope =
                    prepare_envelope(&input, &trigger.message_id, &node, &run_id, &trace_id);
                (index, Ok(executor.run_node(&node, envelope, &policy).await))
            });
        }

        let mut collected: Vec<Option<Result<NodeRun, String>>> =
            (0..level.len()).map(|_| None).collect();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((index, result)) => collected[index] = Some(result),
                Err(err) => {
                    // 任务 panic：不能让它悄悄消失
                    return vec![Err(format!("节点任务异常退出: {err}"))];
                }
            }
        }

        collected
            .into_iter()
            .map(|slot| slot.unwrap_or_else(|| Err("节点任务没有返回结果".to_string())))
            .collect()
    }

    /// 跑**一个**节点。异步执行器用它：它按消息逐个节点推进，而不是一次跑完整条链。
    ///
    /// 治理参数取自节点声明（超时、重试），与同步引擎共用同一段实现——两条执行路径
    /// 的重试与超时语义必须一致，各写一份迟早会漂。
    pub async fn run_single(&self, node: &Node, envelope: Envelope) -> NodeRun {
        let policy = NodePolicy::from_node(node);
        self.run_node(node, envelope, &policy).await
    }

    /// 跑一个节点：解析实例 → 按策略调用（含重试）→ 记录结果。
    async fn run_node(&self, node: &Node, envelope: Envelope, policy: &NodePolicy) -> NodeRun {
        let started = Instant::now();
        let started_at = chrono::Utc::now();
        let mut attempts = 0u32;

        loop {
            attempts += 1;

            // 每次尝试都重新解析实例：上一次失败可能就是因为那个实例挂了
            let target = match self
                .registry
                .resolve(&node.plugin, node.version.as_deref())
                .await
            {
                Ok(target) => target,
                Err(err) => {
                    return NodeRun {
                        outcome: NodeOutcome {
                            node_id: node.id.clone(),
                            plugin: node.plugin.clone(),
                            version: node.version.clone().unwrap_or_default(),
                            instance_id: String::new(),
                            status: NodeStatus::Failed,
                            attempts,
                            started_at,
                            duration_ms: started.elapsed().as_millis() as u64,
                            error: Some(err.to_string()),
                        },
                        output: None,
                    };
                }
            };

            let result = self
                .invoker
                .invoke_target(
                    &target,
                    envelope.clone(),
                    // 预算取「节点超时」与「信封剩余」中更小的那个
                    budget_of(&envelope, policy.timeout_ms),
                )
                .await;

            match result {
                Ok(outcome) => {
                    return node_run_from_outcome(
                        node, target, outcome, attempts, started, started_at,
                    );
                }

                Err(err) if attempts <= policy.retries && err.is_retryable() => {
                    tracing::warn!(
                        node = %node.id,
                        plugin = %node.plugin,
                        attempt = attempts,
                        error = %err,
                        "节点调用失败，将重试"
                    );
                    tokio::time::sleep(backoff_for(attempts)).await;
                }

                Err(err) => {
                    return NodeRun {
                        outcome: NodeOutcome {
                            node_id: node.id.clone(),
                            plugin: target.plugin_name.clone(),
                            version: target.version.clone(),
                            instance_id: target.instance_id.clone(),
                            status: NodeStatus::Failed,
                            attempts,
                            started_at,
                            duration_ms: started.elapsed().as_millis() as u64,
                            error: Some(err.to_string()),
                        },
                        output: None,
                    };
                }
            }
        }
    }
}

/// 把上游输出整理成当前节点的输入信封。
///
/// trace/run/node 逐跳更新，message_id 与载荷保持不变——见本模块顶部的说明。
///
/// `fallback_message_id` 在输入信封自带的 message_id 为空时兜底。**必须兜底**：
/// message_id 是这条**数据**的幂等键，同一条数据穿过多少插件都该是同一个；
/// 某个插件没把它带回来时就换一个新的，等于把幂等悄悄关掉了。
///
/// 公开给异步执行器用——那条路径由生产者准备好下游节点的信封再投递，
/// 好让消费者拿到消息就能直接跑，不必自己再推一遍。
pub fn prepare_envelope(
    input: &Envelope,
    fallback_message_id: &str,
    node: &Node,
    run_id: &str,
    trace_id: &str,
) -> Envelope {
    let mut envelope = input.clone();
    envelope.trace_id = trace_id.to_string();
    envelope.run_id = run_id.to_string();
    envelope.node_id = node.id.clone();

    // 入口节点的载荷可能还是触发时那份，但 message_id 必须与触发保持一致
    if envelope.message_id.is_empty() {
        envelope.message_id = fallback_message_id.to_string();
    }
    envelope
}

fn node_run_from_outcome(
    node: &Node,
    target: InstanceTarget,
    outcome: InvokeOutcome,
    attempts: u32,
    started: Instant,
    started_at: chrono::DateTime<chrono::Utc>,
) -> NodeRun {
    let duration_ms = started.elapsed().as_millis() as u64;

    match outcome {
        InvokeOutcome::Handled { envelope, .. } => NodeRun {
            outcome: NodeOutcome {
                node_id: node.id.clone(),
                plugin: target.plugin_name,
                version: target.version,
                instance_id: target.instance_id,
                status: NodeStatus::Succeeded,
                attempts,
                started_at,
                duration_ms,
                error: None,
            },
            output: Some(*envelope),
        },
        InvokeOutcome::Rejected { issues, .. } => NodeRun {
            outcome: NodeOutcome {
                node_id: node.id.clone(),
                plugin: target.plugin_name,
                version: target.version,
                instance_id: target.instance_id,
                status: NodeStatus::Rejected,
                attempts,
                started_at,
                duration_ms,
                error: Some(
                    issues
                        .iter()
                        .map(|i| {
                            if i.path.is_empty() {
                                i.message.clone()
                            } else {
                                format!("{}: {}", i.path, i.message)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("；"),
                ),
            },
            output: None,
        },
    }
}

/// 节点这次调用能花多久：节点超时与信封剩余预算取小。
fn budget_of(envelope: &Envelope, node_timeout_ms: i64) -> Option<Duration> {
    let node_budget = Duration::from_millis(node_timeout_ms.max(0) as u64);
    let remaining = crate::remaining_budget(envelope);

    Some(match remaining {
        Some(remaining) => node_budget.min(remaining),
        None => node_budget,
    })
}

fn backoff_for(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    (RETRY_BACKOFF_BASE * 2u32.pow(shift)).min(RETRY_BACKOFF_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            plugin: "p".to_string(),
            version: None,
            timeout_ms: None,
            retries: None,
        }
    }

    #[test]
    fn 节点策略取默认值() {
        let policy = NodePolicy::from_node(&node("n1"));
        assert_eq!(policy.timeout_ms, DEFAULT_NODE_TIMEOUT_MS);
        assert_eq!(policy.retries, 0);
    }

    #[test]
    fn 节点策略读声明的值() {
        let mut n = node("n1");
        n.timeout_ms = Some(1500);
        n.retries = Some(3);

        let policy = NodePolicy::from_node(&n);
        assert_eq!(policy.timeout_ms, 1500);
        assert_eq!(policy.retries, 3);
    }

    #[test]
    fn 预算取节点超时与信封剩余的较小者() {
        // 信封给了 10 秒，节点只要 1 秒 → 1 秒
        let envelope = Envelope {
            deadline_ms: chrono::Utc::now().timestamp_millis() + 10_000,
            ..Default::default()
        };
        let budget = budget_of(&envelope, 1_000).expect("应有预算");
        assert!(budget <= Duration::from_millis(1_000), "实际 {budget:?}");

        // 信封只剩 200ms，节点要 5 秒 → 200ms（信封的约束更紧）
        let tight = Envelope {
            deadline_ms: chrono::Utc::now().timestamp_millis() + 200,
            ..Default::default()
        };
        let budget = budget_of(&tight, 5_000).expect("应有预算");
        assert!(
            budget <= Duration::from_millis(200),
            "信封预算更紧时应以信封为准，实际 {budget:?}"
        );
    }

    #[test]
    fn 未设_deadline_时以节点超时为准() {
        let envelope = Envelope::default();
        let budget = budget_of(&envelope, 2_500).expect("应有预算");
        assert_eq!(budget, Duration::from_millis(2_500));
    }

    #[test]
    fn 退避随重试次数增长且有上限() {
        assert_eq!(backoff_for(1), Duration::from_millis(50));
        assert_eq!(backoff_for(2), Duration::from_millis(100));
        assert_eq!(backoff_for(3), Duration::from_millis(200));
        assert_eq!(
            backoff_for(20),
            RETRY_BACKOFF_CAP,
            "再多次重试也不该退避到天荒地老"
        );
    }

    #[test]
    fn 准备信封时更新逐跳字段但保留数据标识() {
        let input = Envelope {
            message_id: "msg-1".to_string(),
            trace_id: "trace-old".to_string(),
            run_id: "run-old".to_string(),
            node_id: "prev".to_string(),
            ..Default::default()
        };
        let fallback = input.message_id.clone();

        let prepared = prepare_envelope(&input, &fallback, &node("current"), "run-1", "trace-1");

        assert_eq!(prepared.trace_id, "trace-1", "trace 要贯穿全程（同一条）");
        assert_eq!(prepared.run_id, "run-1");
        assert_eq!(prepared.node_id, "current", "node 逐跳更新");
        assert_eq!(
            prepared.message_id, "msg-1",
            "message_id 是数据的幂等键，穿过多少插件都不能变"
        );
    }

    #[test]
    fn 信封没有_message_id_时从触发信封补上() {
        let input = Envelope::default();

        // 兜底值来自调用方——同步链传触发信封的 message_id，异步链传上游那一跳的
        let prepared =
            prepare_envelope(&input, "msg-from-trigger", &node("n1"), "run-1", "trace-1");
        assert_eq!(prepared.message_id, "msg-from-trigger");
    }

    #[test]
    fn 结果能算出编排自身开销() {
        let run = FlowRun {
            run_id: "r".to_string(),
            trace_id: "t".to_string(),
            status: RunStatus::Succeeded,
            nodes: vec![
                NodeOutcome {
                    node_id: "a".to_string(),
                    plugin: "p".to_string(),
                    version: "1".to_string(),
                    instance_id: "i".to_string(),
                    status: NodeStatus::Succeeded,
                    attempts: 1,
                    started_at: chrono::Utc::now(),
                    duration_ms: 30,
                    error: None,
                },
                NodeOutcome {
                    node_id: "b".to_string(),
                    plugin: "p".to_string(),
                    version: "1".to_string(),
                    instance_id: "i".to_string(),
                    status: NodeStatus::Succeeded,
                    attempts: 1,
                    started_at: chrono::Utc::now(),
                    duration_ms: 50,
                    error: None,
                },
            ],
            output: None,
            error: None,
            elapsed_ms: 100,
        };

        assert!(run.succeeded());
        assert_eq!(run.node_time_ms(), 80, "总耗时 100ms 里有 80ms 花在插件上");
    }
}
