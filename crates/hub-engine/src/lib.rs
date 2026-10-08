//! 调用执行：把一条 Envelope 送进插件。
//!
//! M1 只有「调用单个插件」这个退化形态——`resolve → Validate → Handle`。
//! M2 在它之上长出完整的 DAG 执行器（见 [`flow`]），这一层补上实例级治理（见 [`govern`]）。
//!
//! 链路里两个刻意的顺序决定：
//!
//! 1. **先 Validate 再 Handle**：校验规则随插件发布（同一个插件导出两个方法），
//!    数据必须先过校验器，不通过就短路，绝不进插件体。
//! 2. **预算来自信封的绝对 deadline**：deadline 逐跳递减，插件据此提前放弃，
//!    而不是把时间耗光后让上层的超时兜底。

pub mod async_exec;
pub mod flow;
pub mod govern;
pub mod service;

use std::future::Future;
use std::time::{Duration, Instant};

pub use async_exec::{AsyncConfig, AsyncError, AsyncExecutor, Disposition};
pub use flow::{FlowExecutor, FlowRun, NodeOutcome, NodePolicy, NodeStatus, RunStatus};
pub use govern::{GovernError, Governor, GovernorConfig, Permit};
pub use service::{FlowService, SaveDraftOutcome, TriggerOutcome};

use chrono::Utc;
use hub_plugin_client::{ClientError, PluginClient};
use hub_proto::v1::{Envelope, ValidationIssue};
use hub_registry::{InstanceTarget, Registry, RegistryError};
use tokio::time::timeout;

#[derive(Debug, thiserror::Error)]
pub enum InvokeError {
    #[error(transparent)]
    Resolve(#[from] RegistryError),

    /// 被实例级治理拦下（熔断中或并发到顶）。
    #[error(transparent)]
    Govern(#[from] GovernError),

    #[error("调用插件 {plugin} 的 {stage} 失败: {source}")]
    Client {
        plugin: String,
        stage: &'static str,
        #[source]
        source: ClientError,
    },

    #[error("调用插件 {plugin} 的 {stage} 超出预算（剩余 {budget_ms}ms）")]
    Timeout {
        plugin: String,
        stage: &'static str,
        budget_ms: u64,
    },

    #[error("插件 {plugin} 的 Handle 未返回信封")]
    EmptyResponse { plugin: String },
}

impl InvokeError {
    /// 这次失败值不值得重试。
    ///
    /// 只有「调用本身失败」（连接断开、插件临时不可用）才重试：
    ///
    /// - **超时不重试**：下游多半已经忙不过来，再压一次只会更糟
    /// - **插件报错不重试**：那是插件内部的逻辑失败，同样的输入重发一次结果还是失败，
    ///   重试只会把一次故障放大成三次
    /// - **治理拦下不重试**：并发到顶或实例明确在冷却，重试只是把同一份压力再推一次。
    ///   正确的动作是让上游降级或稍后再来——这与「超时不重试」是同一条理由
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Client { .. })
    }
}

pub type Result<T> = std::result::Result<T, InvokeError>;

/// 一次调用的结果。
///
/// `Handled` 里的信封装箱：`Envelope` 有 500+ 字节（含 `Any`、多个字符串与 map），
/// 而 `Rejected` 只有一个小 Vec，不装箱会让整个枚举按最大变体占空间。
#[derive(Debug)]
pub enum InvokeOutcome {
    /// 插件体处理完成
    Handled {
        target: InstanceTarget,
        envelope: Box<Envelope>,
        elapsed_ms: u64,
    },

    /// 校验器拒绝，链路在此短路
    Rejected {
        target: InstanceTarget,
        issues: Vec<ValidationIssue>,
        elapsed_ms: u64,
    },
}

impl InvokeOutcome {
    pub fn target(&self) -> &InstanceTarget {
        match self {
            Self::Handled { target, .. } | Self::Rejected { target, .. } => target,
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        match self {
            Self::Handled { elapsed_ms, .. } | Self::Rejected { elapsed_ms, .. } => *elapsed_ms,
        }
    }
}

#[derive(Clone)]
pub struct Invoker {
    registry: Registry,
    client: PluginClient,

    /// 实例级治理。默认参数保守，M4 按压测校准。
    govern: Governor,
}

impl Invoker {
    pub fn new(registry: Registry, client: PluginClient) -> Self {
        Self {
            registry,
            client,
            govern: Governor::default(),
        }
    }

    /// 换一套治理参数。
    pub fn with_governor(mut self, govern: Governor) -> Self {
        self.govern = govern;
        self
    }

    /// 取治理器。服务端用它做实例下线后的表项清理。
    pub fn governor(&self) -> &Governor {
        &self.govern
    }

    /// 调用一个插件：解析实例 → 校验器 → 插件体。
    ///
    /// `version` 为 `None` 表示取该插件最新版本。
    pub async fn invoke(
        &self,
        plugin: &str,
        version: Option<&str>,
        envelope: Envelope,
    ) -> Result<InvokeOutcome> {
        let target = self.registry.resolve(plugin, version).await?;
        let budget = remaining_budget(&envelope);
        self.invoke_target(&target, envelope, budget).await
    }

    /// 对**已解析好的**实例调用，并给定预算。
    ///
    /// 编排引擎用它：实例由引擎解析（重试时它要重新解析——上一次失败可能就是因为
    /// 那个实例挂了），预算由节点策略与信封剩余共同决定。
    ///
    /// 许可在**整个 Validate + Handle 期间**都持有：两次 RPC 都占着插件资源，
    /// 只算一次调用才符合「这个实例同时在扛几个请求」的直觉。
    pub async fn invoke_target(
        &self,
        target: &InstanceTarget,
        envelope: Envelope,
        budget: Option<Duration>,
    ) -> Result<InvokeOutcome> {
        let started = Instant::now();
        let addr = target.advertise_addr.clone();

        let permit = self
            .govern
            .acquire(&target.plugin_name, &target.instance_id, budget)
            .await?;

        let validation = match with_budget(
            budget,
            target,
            "Validate",
            self.client.validate(&addr, envelope.clone()),
        )
        .await
        {
            Ok(validation) => validation,
            Err(err) => {
                permit.record(instance_healthy(&err));
                return Err(err);
            }
        };

        if !validation.valid {
            // 校验器拒绝了数据 —— 插件正常干活了，只是数据没过规则。算健康。
            permit.record(true);
            tracing::info!(
                plugin = %target.plugin_name,
                version = %target.version,
                issues = validation.issues.len(),
                "校验未通过，链路短路"
            );
            return Ok(InvokeOutcome::Rejected {
                target: target.clone(),
                issues: validation.issues,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }

        let handled = match with_budget(
            budget,
            target,
            "Handle",
            self.client.handle(&addr, envelope),
        )
        .await
        {
            Ok(handled) => handled,
            Err(err) => {
                permit.record(instance_healthy(&err));
                return Err(err);
            }
        };

        let output = match handled.envelope {
            Some(envelope) => envelope,
            None => {
                // 插件返回了空信封：调用通道是通的，但契约没履行。算不健康——
                // 这类插件重试多少次都是同样的结果，让它跳闸比反复空转好。
                let err = InvokeError::EmptyResponse {
                    plugin: target.plugin_name.clone(),
                };
                permit.record(false);
                return Err(err);
            }
        };

        permit.record(true);

        let elapsed_ms = started.elapsed().as_millis() as u64;
        tracing::info!(
            plugin = %target.plugin_name,
            version = %target.version,
            instance = %target.instance_id,
            elapsed_ms,
            "调用完成"
        );

        Ok(InvokeOutcome::Handled {
            target: target.clone(),
            envelope: Box::new(output),
            elapsed_ms,
        })
    }
}

/// 这次调用失败是否说明**实例**不健康。
///
/// 只有「插件侧」的失败算：连不上、RPC 报错。**超时不算**，这条是刻意的：
///
/// - 预算由**调用方**决定（HTTP 面来自 `timeout_ms`，编排里来自信封 deadline 与节点
///   策略取小）。把一个调用方自己设的、极小的预算算成实例故障，等于让任何人用
///   `timeout_ms=1` 就能把一个健康实例对所有人打成熔断——一次「我等不起」被放大成
///   全局不可用，而熔断的冷却期内其他调用方全都会收到 503。
/// - 真正挂死的实例走的是另一条路：心跳巡检会把它摘除，它不再收流量；偶发变慢则体现
///   在耗时指标与 `hub_govern_rejected_total` 上。这两条比「按调用方预算熔断」干净得多。
///
/// `EmptyResponse` 由调用点单独判定并在那里回报——它需要顺带说清是哪个插件。
/// 其他错误（解析不到实例、治理拦下）到不了这里：治理拦下时根本没有许可可回报。
fn instance_healthy(err: &InvokeError) -> bool {
    !matches!(err, InvokeError::Client { .. })
}

/// 按剩余预算执行，超预算则返回 [`InvokeError::Timeout`]。
///
/// 预算为 `None` 时不额外设限——客户端的默认请求超时兜底。
async fn with_budget<T, F>(
    budget: Option<Duration>,
    target: &InstanceTarget,
    stage: &'static str,
    fut: F,
) -> Result<T>
where
    F: Future<Output = std::result::Result<T, ClientError>>,
{
    let result = match budget {
        Some(budget) => timeout(budget, fut)
            .await
            .map_err(|_| InvokeError::Timeout {
                plugin: target.plugin_name.clone(),
                stage,
                budget_ms: budget.as_millis() as u64,
            })?,
        None => fut.await,
    };

    result.map_err(|source| InvokeError::Client {
        plugin: target.plugin_name.clone(),
        stage,
        source,
    })
}

/// 从信封的绝对 deadline 算出剩余预算。
///
/// `deadline_ms` 为 0 表示未设置，返回 `None` 交由客户端默认超时兜底；
/// 已经过期时返回 0，调用会立刻超时——这正是 deadline 该有的语义。
pub(crate) fn remaining_budget(envelope: &Envelope) -> Option<Duration> {
    if envelope.deadline_ms <= 0 {
        return None;
    }
    let left = envelope.deadline_ms - Utc::now().timestamp_millis();
    Some(Duration::from_millis(left.max(0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope_with_deadline(deadline_ms: i64) -> Envelope {
        Envelope {
            deadline_ms,
            ..Default::default()
        }
    }

    #[test]
    fn 未设置_deadline_时不设预算() {
        assert_eq!(remaining_budget(&envelope_with_deadline(0)), None);
        assert_eq!(remaining_budget(&envelope_with_deadline(-1)), None);
    }

    #[test]
    fn 未到期的_deadline_换算成剩余毫秒() {
        let deadline = Utc::now().timestamp_millis() + 5_000;
        let budget = remaining_budget(&envelope_with_deadline(deadline)).expect("应有预算");
        assert!(
            budget <= Duration::from_millis(5_000) && budget > Duration::from_millis(4_000),
            "约 5 秒，实际 {budget:?}"
        );
    }

    #[test]
    fn 已过期的_deadline_预算为零而非负() {
        let deadline = Utc::now().timestamp_millis() - 10_000;
        let budget = remaining_budget(&envelope_with_deadline(deadline)).expect("应有预算");
        assert_eq!(budget, Duration::ZERO, "过期就该立刻超时，不能变成无穷等待");
    }

    #[test]
    fn 结果能取回目标实例与耗时() {
        let target = InstanceTarget {
            plugin_name: "p".to_string(),
            version: "1.0.0".to_string(),
            instance_id: "i1".to_string(),
            advertise_addr: "http://127.0.0.1:1".to_string(),
        };
        let outcome = InvokeOutcome::Rejected {
            target: target.clone(),
            issues: Vec::new(),
            elapsed_ms: 42,
        };
        assert_eq!(outcome.target(), &target);
        assert_eq!(outcome.elapsed_ms(), 42);
    }
}
