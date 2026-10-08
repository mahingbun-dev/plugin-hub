//! 实例级治理：并发上限与熔断半开。
//!
//! 治理的粒度是**实例**而不是插件：一个插件可以有多副本，其中一个副本所在的机器
//! 出问题（网卡、磁盘、GC 卡死）不该牵连其他副本。按插件熔断会把整个插件打成不可用，
//! 那正好是这层想避免的结果。
//!
//! 两个刻意的取舍：
//!
//! - **背压即快速失败，不无限排队**：并发到顶时最多等 `queue_timeout`（默认 100ms），
//!   等不到就报 [`GovernError::Overloaded`]。无限排队会把过载翻译成延迟爆炸——调用方
//!   看到的是一路变慢而不是明确的失败，反而更难处置。
//! - **熔断与背压都不重试**：与 [`crate::InvokeError::is_retryable`] 里「超时不重试」
//!   是同一条理由——并发已到顶或实例明确在冷却时，重试只是把同一份压力再推一次。
//!   正确的动作是让上游降级或稍后再来。
//!
//! 状态机的判据只有「调用本身是否成功」：`Handled` 与 `Rejected` 都算**健康**——
//! 校验器拒绝了数据，说明插件正常干活了，只是数据没过规则，那是业务结果不是故障。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 每实例的默认并发上限。
///
/// 32 是保守值：同机插件一次调用是亚毫秒级，32 个在途已经能打满一个核；
/// 跨机插件受网络往返支配，这个上限远达不到瓶颈。真正的峰值靠压测校准（M4）。
pub const DEFAULT_MAX_CONCURRENCY: usize = 32;

/// 背压排队的默认上限。
///
/// 不是 0：完全不让排队会让突发流量产生大量假失败。100ms 是个折中——
/// 够平滑毫秒级的抖动，又不至于把过载藏成延迟。
pub const DEFAULT_QUEUE_TIMEOUT_MS: u64 = 100;

/// 连续失败多少次跳闸。
pub const DEFAULT_FAILURE_THRESHOLD: u32 = 5;

/// 跳闸后多久放一个探测请求过去。
pub const DEFAULT_OPEN_COOLDOWN_SECS: u64 = 10;

/// 治理参数。默认值保守，M4 按压测校准。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernorConfig {
    /// 每个实例的在途调用上限
    pub max_concurrency: usize,

    /// 拿不到许可时的最长等待；等不到就背压失败
    pub queue_timeout: Duration,

    /// 连续失败达到这个数就跳闸
    pub failure_threshold: u32,

    /// 跳闸后到「放一个探测请求」之间的冷却时间
    pub open_cooldown: Duration,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            queue_timeout: Duration::from_millis(DEFAULT_QUEUE_TIMEOUT_MS),
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            open_cooldown: Duration::from_secs(DEFAULT_OPEN_COOLDOWN_SECS),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GovernError {
    /// 实例已跳闸，正在冷却；或半开的探测名额已被占。
    #[error(
        "实例 {instance_id}（插件 {plugin}）已熔断：连续失败 {failures} 次，{retry_after_ms}ms 后转半开"
    )]
    CircuitOpen {
        plugin: String,
        instance_id: String,
        failures: u32,
        retry_after_ms: u64,
    },

    /// 并发到顶且排队超时。
    #[error("实例 {instance_id}（插件 {plugin}）并发已达上限 {limit}，排队 {waited_ms}ms 仍无空位")]
    Overloaded {
        plugin: String,
        instance_id: String,
        limit: usize,
        waited_ms: u64,
    },
}

/// 熔断器的三个状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Breaker {
    /// 正常放行
    Closed,

    /// 跳闸中，`until` 之前一律拒绝
    Open { until: Instant },

    /// 半开：已经放了一个探测请求过去，等它回报
    Probing,
}

/// 熔断闸门的放行结果。
///
/// **必须区分这两种**：只有「冷却结束后放过去的那个探测」才有资格在成功时关掉熔断、
/// 在失败时立刻回到跳闸。普通调用做不到这两件事——否则一个在跳闸**之前**起飞、跳闸
/// **之后**才回报成功的慢调用，会把刚跳的闸直接抹掉（而它的成功是过期消息：这段时间
/// 里实例确实一直在失败）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// 熔断关闭时的普通放行
    Normal,

    /// 冷却结束后的那一个探测
    Probe,
}

/// 单个实例的治理单元。
#[derive(Debug)]
struct Cell {
    /// 并发闸门。放进 `Cell` 而不是全局表，是为了让许可的生命周期与实例解耦——
    /// 实例从表里被清理掉时，已在飞的调用仍持着自己的许可。
    gate: Arc<Semaphore>,

    /// 熔断状态。临界区全是同步代码，**绝不在持锁时 await**。
    state: Mutex<BreakerState>,

    /// 当前有几次「占用」——每次调用从取到 `Cell` 到回报结束算一次。
    ///
    /// 这是 `prune` 的判据。**必须在 `cells` 锁内加减**：`prune` 持同一把锁读它，
    /// 于是「取到 Cell」与「登记占用」之间不可能插入一次清理。少了它会有这样一条
    /// 窗口——调用方拿到 `Cell` 引用、还没去拿信号量许可，此时闸门全空、熔断关闭，
    /// 巡检正好判定它「空闲」把表项删掉；调用方继续用这条被摘掉的 `Cell` 发许可，
    /// 而新调用方又新建一条。结果同一个实例有两套信号量与两套失败计数：
    /// **并发上限翻倍，熔断永远不生效**。
    checked_out: AtomicUsize,
}

impl Cell {
    fn new(max_concurrency: usize) -> Self {
        Self {
            gate: Arc::new(Semaphore::new(max_concurrency)),
            state: Mutex::new(BreakerState {
                breaker: Breaker::Closed,
                consecutive_failures: 0,
                admitted: 0,
            }),
            checked_out: AtomicUsize::new(0),
        }
    }

    /// 此刻是否完全空闲：没有占用者，也没有在飞的许可。
    ///
    /// 「没有占用者」已经蕴含「没有在飞的许可」——每个 `Permit` 都持着一个
    /// [`Checkout`]。所以判据只有占用计数一项。
    fn is_idle(&self) -> bool {
        self.checked_out.load(Ordering::SeqCst) == 0
    }
}

#[derive(Debug)]
struct BreakerState {
    breaker: Breaker,

    /// 连续失败次数。跳闸后**不清零**——半开探测再失败时它继续往上加，
    /// 「已达阈值」因此天然成立；只有探测成功才归零。
    consecutive_failures: u32,

    /// 累计放行过的调用数。纯观测用。
    admitted: u64,
}

/// 熔断此刻处于哪个阶段（对外的形态）。
///
/// 与内部的 [`Breaker`] 分开：那个带着 `Instant`，是调度用的；
/// 这个是给人看的，只要「正常 / 跳闸 / 半开」三个状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerPhase {
    /// 正常放行
    Closed,

    /// 跳闸中，冷却结束前一律拒绝
    Open,

    /// 半开：已经放了一个探测过去，等它回报
    Probing,
}

/// 一个实例此刻的治理状态。
///
/// 纯数据、不带 serde：这是从治理器里读出来的**事实快照**，
/// 对外长什么样由接口层决定（见 `hub-api` 的治理面）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceGovernance {
    pub instance_id: String,

    /// 并发上限（配置值，所有实例相同）
    pub max_concurrency: usize,

    /// 此刻还剩几个并发名额
    pub available: usize,

    /// 在飞的调用数
    pub in_flight: usize,

    pub phase: BreakerPhase,

    /// 距离冷却结束还有多久；未跳闸时为 `None`
    pub open_for: Option<std::time::Duration>,

    /// 连续失败次数。跳闸后不清零，只有探测成功才归零
    pub consecutive_failures: u32,

    /// 累计放行过的调用数
    pub admitted: u64,
}

/// 实例级治理器。`Clone` 是共享同一份状态（内部全是 `Arc`）。
#[derive(Clone)]
pub struct Governor {
    cells: Arc<Mutex<HashMap<String, Arc<Cell>>>>,
    config: GovernorConfig,
}

impl Governor {
    pub fn new(config: GovernorConfig) -> Self {
        Self {
            cells: Arc::new(Mutex::new(HashMap::new())),
            // 并发上限至少为 1。环境变量那条路径由 `hub-core` 的配置校验拦着，
            // 但 `Governor` 是公开 API，直接构造一个 0 会让每一次调用都被背压拦下
            // ——中台等于死了，而且死得没有任何报错。
            config: GovernorConfig {
                max_concurrency: config.max_concurrency.max(1),
                ..config
            },
        }
    }

    pub fn config(&self) -> &GovernorConfig {
        &self.config
    }

    /// 取一个实例的调用许可。
    ///
    /// 顺序是**先查熔断再排队**：熔断判定是纯内存判断，零成本；反过来先排队再判定，
    /// 会在实例已熔断时白白让调用方等满 `queue_timeout`。
    ///
    /// `budget` 是调用方剩余的时间预算（来自信封 deadline）。排队等待取它与
    /// `queue_timeout` 的较小值——已经没预算的调用不该再排 100ms 队。
    pub async fn acquire(
        &self,
        plugin: &str,
        instance_id: &str,
        budget: Option<Duration>,
    ) -> Result<Permit, GovernError> {
        // 占用要在锁内登记（见 `checkout` 的说明）。下面几条提前返回都会把 `checkout`
        // 自动归还，这正是把它做成一个 Drop 类型的目的。
        let checkout = self.checkout(instance_id);

        let admission = match self.admit(&checkout.cell, plugin, instance_id) {
            Ok(admission) => admission,
            Err(rejection) => {
                metrics::counter!(
                    "hub_govern_rejected_total",
                    "plugin" => plugin.to_string(),
                    "reason" => "circuit_open",
                )
                .increment(1);
                return Err(rejection);
            }
        };

        // 探测名额的凭据。**必须是一个跨越 await 的局部变量**：取到探测名额之后还要
        // 排队等并发许可，如果这段等待被取消（外层 timeout、客户端断开），future 直接
        // 被丢掉，后面一行代码都不会执行——只有 Drop 才会跑。少了它，实例会永远停在
        // 「有一个探测在飞」，期间所有调用都被误报成熔断，也不再尝试探测。
        let probe =
            (admission == Admission::Probe).then(|| ProbeGuard::new(Arc::clone(&checkout.cell)));

        // 并发闸门可能要等待，所以**不持锁**。
        let limit = self.config.max_concurrency;
        let wait = match budget {
            Some(budget) => self.config.queue_timeout.min(budget),
            None => self.config.queue_timeout,
        };

        let cell = &checkout.cell;
        let gate = Arc::clone(&cell.gate);
        let permit = match tokio::time::timeout(wait, gate.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            // 信号量已关闭：本进程的治理表被拆了，按背压处理（不该发生）
            Ok(Err(_)) | Err(_) => {
                metrics::counter!(
                    "hub_govern_rejected_total",
                    "plugin" => plugin.to_string(),
                    "reason" => "overloaded",
                )
                .increment(1);
                // 若自己是那个探测，`probe` 在返回时 drop 会把名额退回去。
                // **绝不能在这里无条件退**：排队等超时的多半是无关调用，它无权
                // 抹掉别人的探测名额——那样半开会连着放第二个、第三个探测过去。
                return Err(GovernError::Overloaded {
                    plugin: plugin.to_string(),
                    instance_id: instance_id.to_string(),
                    limit,
                    waited_ms: wait.as_millis() as u64,
                });
            }
        };

        metrics::gauge!("hub_govern_inflight", "plugin" => plugin.to_string()).increment(1.0);

        Ok(Permit {
            plugin: plugin.to_string(),
            instance_id: instance_id.to_string(),
            threshold: self.config.failure_threshold,
            cooldown: self.config.open_cooldown,
            checkout,
            probe,
            _permit: permit,
        })
    }

    /// 熔断闸门：能放行就返回放行方式，否则返回该怎么拒。
    ///
    /// 顺带把「放行」这件事记进状态——冷却结束后的第一次放行要把状态切成半开，
    /// 这一步必须在同一个临界区里做，否则并发调用会同时穿过去，半开就成了摆设。
    fn admit(
        &self,
        cell: &Arc<Cell>,
        plugin: &str,
        instance_id: &str,
    ) -> Result<Admission, GovernError> {
        let now = Instant::now();
        let mut state = cell.state.lock().expect("治理表锁中毒");

        match state.breaker {
            Breaker::Closed => {
                state.admitted += 1;
                Ok(Admission::Normal)
            }
            Breaker::Open { until } if now >= until => {
                state.breaker = Breaker::Probing;
                state.admitted += 1;
                Ok(Admission::Probe)
            }
            Breaker::Open { until } => Err(GovernError::CircuitOpen {
                plugin: plugin.to_string(),
                instance_id: instance_id.to_string(),
                failures: state.consecutive_failures,
                retry_after_ms: until.saturating_duration_since(now).as_millis() as u64,
            }),
            Breaker::Probing => {
                // 半开只放一个探测，其余一律拒绝——放一批过去等于把刚恢复的实例再打倒
                Err(GovernError::CircuitOpen {
                    plugin: plugin.to_string(),
                    instance_id: instance_id.to_string(),
                    failures: state.consecutive_failures,
                    // 报的是「等一个冷却再试」，不是「现在这个冷却还剩多久」：
                    // 探测在飞时我们并不知道它什么时候回来，报一个假倒计时更糟
                    retry_after_ms: self.config.open_cooldown.as_millis() as u64,
                })
            }
        }
    }

    /// 清理不再存在的实例的治理表项。
    ///
    /// 不清理就是一条慢速泄漏：插件反复重启会不断产生新的 instance_id，表项只增不减。
    ///
    /// 判据是「**没人占用** 且已关闸」：
    ///
    /// - 「没人占用」由 [`Cell::checked_out`] 给出，且与 `checkout` 共享同一把锁，
    ///   所以不会出现「刚被人拿到手就被删掉」——那会导致同一个实例有两套信号量与
    ///   两套失败计数，并发上限翻倍而熔断永不生效。
    /// - 「已关闸」是因为跳闸中的实例必须留着，否则它刚攒下的失败计数会凭空归零。
    ///
    /// 返回清理掉的实例数。
    pub fn prune(&self, alive: &HashSet<String>) -> usize {
        let mut cells = self.cells.lock().expect("治理表锁中毒");
        let before = cells.len();
        cells.retain(|instance_id, cell| {
            if alive.contains(instance_id) || !cell.is_idle() {
                return true;
            }
            let state = cell.state.lock().expect("治理表锁中毒");
            state.breaker != Breaker::Closed
        });
        before - cells.len()
    }

    /// 当前有治理记录的实例数（测试与观测用）。
    pub fn tracked_instances(&self) -> usize {
        self.cells.lock().expect("治理表锁中毒").len()
    }

    /// 每个实例此刻的治理状态快照，按 instance_id 排序。
    ///
    /// 给控制台的治理面板用。**锁顺序与 `prune` 一致**（先 `cells` 再 `state`）——
    /// 反过来的话两个方向各持一把锁互相等，就成了死锁。
    pub fn snapshot(&self) -> Vec<InstanceGovernance> {
        let cells = self.cells.lock().expect("治理表锁中毒");

        let mut out: Vec<InstanceGovernance> = cells
            .iter()
            .map(|(instance_id, cell)| {
                let available = cell.gate.available_permits();
                let state = cell.state.lock().expect("治理表锁中毒");

                let (phase, open_for) = match state.breaker {
                    Breaker::Closed => (BreakerPhase::Closed, None),
                    Breaker::Probing => (BreakerPhase::Probing, None),
                    Breaker::Open { until } => (
                        BreakerPhase::Open,
                        Some(until.saturating_duration_since(Instant::now())),
                    ),
                };

                InstanceGovernance {
                    instance_id: instance_id.clone(),
                    max_concurrency: self.config.max_concurrency,
                    available,
                    // 在飞 = 上限 − 还剩的名额。两者之差比单独维护一个计数可靠：
                    // 许可由 `Permit` 持有，它的生命周期就是「在飞」的定义
                    in_flight: self.config.max_concurrency.saturating_sub(available),
                    phase,
                    open_for,
                    consecutive_failures: state.consecutive_failures,
                    admitted: state.admitted,
                }
            })
            .collect();

        out.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        out
    }

    /// 取出（必要时建）实例的治理单元，并**在锁内**把它登记为「占用中」。
    ///
    /// 「登记占用」这一步必须和「取到 Cell」在同一个临界区里，理由见
    /// [`Cell::checked_out`]——否则 `prune` 能挤在两步之间把表项删掉。
    fn checkout(&self, instance_id: &str) -> Checkout {
        let mut cells = self.cells.lock().expect("治理表锁中毒");
        let cell = Arc::clone(
            cells
                .entry(instance_id.to_string())
                .or_insert_with(|| Arc::new(Cell::new(self.config.max_concurrency))),
        );
        cell.checked_out.fetch_add(1, Ordering::SeqCst);
        Checkout { cell }
    }
}

/// 一次「占用」的凭据，drop 时归还。
///
/// 单独一个类型而不是手工配对加减：`acquire` 里有好几条提前返回的路（熔断拦下、
/// 排队超时），手工配平迟早会漏一条，而漏掉的表现是「表项永远清不掉」——一条不会
/// 报错、只会慢慢涨的泄漏。
#[derive(Debug)]
struct Checkout {
    cell: Arc<Cell>,
}

impl Drop for Checkout {
    fn drop(&mut self) {
        self.cell.checked_out.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 半开探测名额的凭据。
///
/// drop 时若状态**仍然**是 `Probing`，说明这次探测没能走到回报那一步（排队时被取消、
/// 或者调用方拿到许可后直接丢了），于是把名额退回「立即可再探测」。
///
/// 判据用「状态还是不是 Probing」而不是自己记一个标志位：探测正常回报时，`record`
/// 已经把状态改成了 `Closed` 或 `Open`，这里的退回自然变成空操作。少一个要维护的
/// 字段，就少一条会写错的分支。
#[derive(Debug)]
struct ProbeGuard {
    cell: Arc<Cell>,
}

impl ProbeGuard {
    fn new(cell: Arc<Cell>) -> Self {
        Self { cell }
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        let mut state = self.cell.state.lock().expect("治理表锁中毒");
        if state.breaker == Breaker::Probing {
            state.breaker = Breaker::Open {
                until: Instant::now(),
            };
        }
    }
}

impl Default for Governor {
    fn default() -> Self {
        Self::new(GovernorConfig::default())
    }
}

/// 一次调用的许可。持有它代表占用了这个实例的一个在途名额。
///
/// **必须调用 [`Permit::record`]**：不回报成败，熔断器就学不到任何东西。
/// 直接 drop（比如调用被取消）时按「未知」处理——只把半开名额退回去，不计成败。
#[derive(Debug)]
pub struct Permit {
    plugin: String,
    instance_id: String,
    /// 建许可时把配置烘进来，`record` 就不必再回头找 `Governor`
    threshold: u32,
    cooldown: Duration,
    /// 持着它，`prune` 才看得见「这条表项还有人用」
    checkout: Checkout,
    /// 只有冷却结束后放过去的那个探测才有；普通调用是 `None`
    probe: Option<ProbeGuard>,
    /// 保持许可存活；drop 时自动归还名额
    _permit: OwnedSemaphorePermit,
}

impl Permit {
    /// 回报本次调用是否说明实例健康。
    ///
    /// 判据是「调用本身有没有成功」，而不是「业务有没有成功」——校验器拒绝数据
    /// （`Rejected`）说明插件正常干活了，属于健康。
    pub fn record(self, healthy: bool) {
        // 探测凭据不能在回报之前就 drop 掉：它在 drop 时会把状态从 Probing 退回去，
        // 而下面正要基于「现在是不是 Probing」做判断
        let is_probe = self.probe.is_some();
        let cell = Arc::clone(&self.checkout.cell);
        let plugin = self.plugin.clone();
        let instance_id = self.instance_id.clone();
        let threshold = self.threshold;
        let cooldown = self.cooldown;

        let mut state = cell.state.lock().expect("治理表锁中毒");

        if healthy {
            if is_probe {
                state.consecutive_failures = 0;
                state.breaker = Breaker::Closed;
                drop(state);
                tracing::info!(plugin = %plugin, instance = %instance_id, "探测成功，实例已恢复");
                metrics::counter!("hub_govern_breaker_recovered_total",
                    "plugin" => plugin.clone())
                .increment(1);
                return;
            }

            // 普通调用：只在**没跳闸**时才把计数清零。
            //
            // 跳闸状态下回报的成功是**过期消息**——这次调用是在跳闸之前被放行的，
            // 它成功不代表这段时间实例没在失败。放它关闸会让一个慢但成功的调用抹掉
            // 刚跳的闸，冷却形同虚设。
            if state.breaker == Breaker::Closed {
                state.consecutive_failures = 0;
            }
            return;
        }

        state.consecutive_failures += 1;
        let failures = state.consecutive_failures;

        // 探测失败 → 立刻回到跳闸，不必再攒够阈值：它本来就因为达到阈值才跳的闸。
        // 普通调用 → 只在「仍未跳闸且达到阈值」时跳，避免给已经在冷却的实例反复
        // 重置冷却起点。
        let should_trip = if is_probe {
            true
        } else {
            state.breaker == Breaker::Closed && failures >= threshold
        };

        if !should_trip {
            return;
        }

        state.breaker = Breaker::Open {
            until: Instant::now() + cooldown,
        };
        drop(state);
        tracing::warn!(
            plugin = %plugin,
            instance = %instance_id,
            failures,
            cooldown_ms = cooldown.as_millis() as u64,
            "实例连续失败已跳闸"
        );
        metrics::counter!("hub_govern_breaker_tripped_total", "plugin" => plugin).increment(1);
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        metrics::gauge!("hub_govern_inflight", "plugin" => self.plugin.clone()).decrement(1.0);
        // 半开名额的退还由 `probe` 自己的 Drop 负责，这里不再重复处理
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 冷却短、阈值低、名额少的配置，好让测试用真实时间跑而不是靠猜睡眠时长。
    fn quick(threshold: u32) -> GovernorConfig {
        GovernorConfig {
            max_concurrency: 2,
            queue_timeout: Duration::from_millis(30),
            failure_threshold: threshold,
            open_cooldown: Duration::from_millis(60),
        }
    }

    /// 等到冷却结束。多等 20ms 是给计时器的余量，避免贴着边界抖动。
    async fn wait_cooldown(gov: &Governor) {
        tokio::time::sleep(gov.config().open_cooldown + Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn 并发到顶时背压失败而不是无限等待() {
        let gov = Governor::new(quick(99));

        let _first = gov
            .acquire("p", "i1", None)
            .await
            .expect("第一个应拿到许可");
        let _second = gov
            .acquire("p", "i1", None)
            .await
            .expect("第二个应拿到许可");

        let started = Instant::now();
        let err = gov
            .acquire("p", "i1", None)
            .await
            .expect_err("第三个应被背压拦下");

        assert!(
            matches!(err, GovernError::Overloaded { limit: 2, .. }),
            "应是背压而不是别的错：{err}"
        );
        assert!(
            started.elapsed() >= gov.config().queue_timeout,
            "应该真的等过一段再放弃，否则退化成「不排队」"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "不该无限等：实际 {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn 许可归还后名额重新可用() {
        let gov = Governor::new(quick(99)); // 名额 2

        let first = gov.acquire("p", "i1", None).await.expect("应拿到许可");
        let second = gov.acquire("p", "i1", None).await.expect("应拿到许可");
        assert!(
            gov.acquire("p", "i1", None).await.is_err(),
            "名额满了就该拦"
        );

        drop(first); // 取消的调用也归还名额

        gov.acquire("p", "i1", None)
            .await
            .expect("归还后应能再取到");
        drop(second);
    }

    #[tokio::test]
    async fn 不同实例的名额与熔断互不影响() {
        let gov = Governor::new(quick(2));

        // 把 i1 打到跳闸
        for _ in 0..2 {
            let permit = gov.acquire("p", "i1", None).await.expect("应拿到许可");
            permit.record(false);
        }
        assert!(
            gov.acquire("p", "i1", None).await.is_err(),
            "i1 已跳闸，应拒绝"
        );

        // i2 同插件但不同实例，必须照常可用——按实例熔断的意义就在这里
        let permit = gov
            .acquire("p", "i2", None)
            .await
            .expect("另一个实例不该被牵连：一个副本坏了不等于整个插件坏了");
        permit.record(true);
    }

    #[tokio::test]
    async fn 连续失败到阈值才跳闸() {
        let gov = Governor::new(quick(3));

        for n in 1..3 {
            let permit = gov.acquire("p", "i1", None).await.expect("应拿到许可");
            permit.record(false);
            assert!(
                gov.acquire("p", "i1", None).await.is_ok(),
                "第 {n} 次失败还没到阈值，不该跳闸"
            );
        }

        let permit = gov.acquire("p", "i1", None).await.expect("应拿到许可");
        permit.record(false); // 第 3 次

        let err = gov
            .acquire("p", "i1", None)
            .await
            .expect_err("到阈值应跳闸");
        assert!(
            matches!(err, GovernError::CircuitOpen { failures: 3, .. }),
            "应报出累计失败次数，排障时要用：{err}"
        );
    }

    #[tokio::test]
    async fn 中途成功会把失败计数清零() {
        let gov = Governor::new(quick(3));

        for _ in 0..2 {
            let permit = gov.acquire("p", "i1", None).await.expect("应拿到许可");
            permit.record(false);
        }
        let permit = gov.acquire("p", "i1", None).await.expect("应拿到许可");
        permit.record(true); // 成功一次

        for _ in 0..2 {
            let permit = gov.acquire("p", "i1", None).await.expect("应拿到许可");
            permit.record(false);
            assert!(
                gov.acquire("p", "i1", None).await.is_ok(),
                "计数该从零重来，否则偶发失败会累积成误跳闸"
            );
        }
    }

    #[tokio::test]
    async fn 冷却结束后半开只放一个探测() {
        let gov = Governor::new(quick(1));

        let permit = gov.acquire("p", "i1", None).await.expect("应拿到许可");
        permit.record(false); // 立刻跳闸
        assert!(gov.acquire("p", "i1", None).await.is_err());

        wait_cooldown(&gov).await;

        let probe = gov
            .acquire("p", "i1", None)
            .await
            .expect("冷却结束后应放一个探测过去");

        // 探测在飞期间其余一律拒绝——放一批过去等于把刚恢复的实例再打倒
        let err = gov
            .acquire("p", "i1", None)
            .await
            .expect_err("半开期间只放一个");
        assert!(matches!(err, GovernError::CircuitOpen { .. }), "{err}");

        probe.record(true); // 探测成功

        gov.acquire("p", "i1", None)
            .await
            .expect("探测成功后应完全恢复");
    }

    #[tokio::test]
    async fn 半开探测失败立刻回到跳闸() {
        let gov = Governor::new(quick(1));

        let permit = gov.acquire("p", "i1", None).await.expect("应拿到许可");
        permit.record(false);
        wait_cooldown(&gov).await;

        let probe = gov.acquire("p", "i1", None).await.expect("应放探测");
        probe.record(false); // 还没恢复

        let err = gov
            .acquire("p", "i1", None)
            .await
            .expect_err("探测失败应立刻回到跳闸，不必再攒够阈值");
        assert!(matches!(err, GovernError::CircuitOpen { .. }), "{err}");
    }

    #[tokio::test]
    async fn 探测过程中被取消会退回可探测状态() {
        let gov = Governor::new(quick(1));

        let permit = gov.acquire("p", "i1", None).await.expect("应拿到许可");
        permit.record(false);
        wait_cooldown(&gov).await;

        // 探测许可被直接丢掉（调用被取消，没来得及回报）
        drop(gov.acquire("p", "i1", None).await.expect("应放探测"));

        // 不能因此永远停在「有一个探测在飞」，否则这个实例再也放不出探测
        gov.acquire("p", "i1", None)
            .await
            .expect("取消后应能重新探测");
    }

    #[tokio::test]
    async fn 排队等待不超过剩余预算() {
        // 排队上限很大，但预算只有 40ms——已经没预算的调用不该再排长队
        let gov = Governor::new(GovernorConfig {
            max_concurrency: 1,
            queue_timeout: Duration::from_secs(30),
            ..quick(99)
        });

        let _held = gov.acquire("p", "i1", None).await.expect("应拿到许可");

        let started = Instant::now();
        let err = gov
            .acquire("p", "i1", Some(Duration::from_millis(40)))
            .await
            .expect_err("应背压失败");
        assert!(matches!(err, GovernError::Overloaded { .. }), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "等待应被预算压到 40ms 而不是排满 30 秒：实际 {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn 清理只删已关闸且空闲的实例() {
        let gov = Governor::new(quick(1));

        // 空闲且正常：该清
        drop(gov.acquire("p", "idle", None).await.expect("应拿到许可"));
        // 还有在途调用：不能清
        let inflight = gov.acquire("p", "busy", None).await.expect("应拿到许可");
        // 已跳闸：不能清，否则刚攒的失败计数会凭空归零
        let tripped = gov.acquire("p", "tripped", None).await.expect("应拿到许可");
        tripped.record(false);
        // 仍在注册表里：不能清
        drop(gov.acquire("p", "alive", None).await.expect("应拿到许可"));

        let alive: HashSet<String> = ["alive".to_string()].into_iter().collect();
        assert_eq!(gov.prune(&alive), 1, "只该清掉 idle 一个");
        assert_eq!(gov.tracked_instances(), 3);

        drop(inflight);
        assert_eq!(gov.prune(&alive), 1, "在途调用结束后 busy 才可清");
    }

    #[tokio::test]
    async fn 同一个实例的治理表项只有一份() {
        let gov = Governor::new(quick(99));
        drop(gov.acquire("p", "i1", None).await.expect("应拿到许可"));
        drop(gov.acquire("p", "i1", None).await.expect("应拿到许可"));
        assert_eq!(gov.tracked_instances(), 1, "键是 instance_id，不该重复建项");
    }
}
