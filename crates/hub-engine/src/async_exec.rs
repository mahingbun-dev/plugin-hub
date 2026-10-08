//! 异步链：节点级消费与投递。
//!
//! 与 [`crate::flow`] 的同步引擎是同一份编排的两种跑法。同步链在一次请求里跑完全程，
//! 异步链把**每个节点**做成一条消息：跑完一个节点就把它下游的节点各自投一条消息出去。
//!
//! 为什么粒度是节点而不是整条链：
//!
//! - **单条消息寿命短**：重投时的代价与消息体量成正比，而长链的中间产物往往比入口大
//! - **链路越长并行度越高**：同步引擎要等一层跑完才开下一层，异步链里「依赖就绪即执行」
//! - **节点级重试天然成立**：跑失败的那一跳自己重来，不必把整条链重放一遍
//!
//! 代价是需要**收敛判断**（哪些节点都跑完了），用 `runs` 行上的一把锁串行化完成，
//! 不靠内存计数——中台可以多实例，内存里数不清。
//!
//! # 不丢也不重做：两道防线
//!
//! Stream 的投递语义是 at-least-once，所以「不丢」的反面就是「会重投」。两道防线：
//!
//! 1. **幂等键**（`(run_id, node_id)`）挡住重投时的重复执行；
//! 2. **完成记录**（`run_nodes` 里有没有这个节点的行）区分「做完过」与「上一个持有者
//!    中途死了」。只看键在不在，一次进程被杀就会让那个节点永远没人跑——那是**静默丢
//!    数据**，比重复执行糟糕得多。
//!
//! 而「节点执行失败」不是基础设施故障：它是一条业务结果，记录下来、收敛 run、正常
//! ACK。只有发不出去、库连不上这类问题才让消息**不 ACK**，交给重投。

use std::time::Duration;

use hub_bus::{Bus, Delivery};
use hub_flow::plan;
use hub_proto::BusMessage;
use hub_proto::v1::bus_message::Kind;
use hub_store::Store;
use hub_store::StoreError;
use hub_store::flows;
use hub_store::runs::{self, NewRun, NewRunNode};
use hub_store::triggers;
use serde_json::json;

use crate::flow::{FlowExecutor, NodeStatus, prepare_envelope};

/// 默认的幂等键存活时间。与 `STREAM_RETENTION_HOURS` 对齐——超过这个窗口的消息不会
/// 再被重投，键留着也没有意义了。
pub const DEFAULT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncConfig {
    /// 幂等键存活多久
    pub idempotency_ttl: Duration,
}

impl Default for AsyncConfig {
    fn default() -> Self {
        Self {
            idempotency_ttl: DEFAULT_IDEMPOTENCY_TTL,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AsyncError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Bus(#[from] hub_bus::BusError),

    /// 消息本身不成立：编排被删了、节点不在了、信封丢了。
    ///
    /// 与基础设施错误分开是必要的：这类错误**重投一万次也是同样的结果**，正确的动作
    /// 是记一笔再 ACK 掉，而不是让它永远占着队列。
    #[error("异步消息不成立: {0}")]
    Invalid(String),
}

/// 一条消息处理完之后该拿它怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// 做完了，可以 ACK
    Done,

    /// 已经做过了（重投），可以直接 ACK
    Duplicate,

    /// 消息不成立，记一笔再 ACK——重投不会让它变得成立
    Discarded,
}

/// 异步链执行器。
#[derive(Clone)]
pub struct AsyncExecutor {
    store: Store,
    bus: Bus,
    executor: FlowExecutor,
    config: AsyncConfig,
}

impl AsyncExecutor {
    pub fn new(store: Store, bus: Bus, executor: FlowExecutor) -> Self {
        Self {
            store,
            bus,
            executor,
            config: AsyncConfig::default(),
        }
    }

    pub fn with_config(mut self, config: AsyncConfig) -> Self {
        self.config = config;
        self
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// 入队一次异步执行，返回 run id。
    ///
    /// **先落 run 行，再投递**。反过来的话，投递成功而落行失败会留下一条无迹可寻的
    /// 消息；而先落行，投递失败就只是一个「停在 queued 的执行」——它可以被重投，
    /// 也可以被人看见并处置。前者的失败是静默的，后者不是。
    pub async fn enqueue(
        &self,
        flow_name: &str,
        trigger: hub_proto::Envelope,
        trigger_meta: Option<serde_json::Value>,
    ) -> Result<String, AsyncError> {
        let flow = flows::find_flow(self.store.pool(), flow_name)
            .await?
            .ok_or_else(|| StoreError::Invalid(format!("flow {flow_name} 未定义")))?;

        let revision = flows::find_published(self.store.pool(), flow.id)
            .await?
            .ok_or_else(|| StoreError::Invalid(format!("flow {flow_name} 尚未发布，不能触发")))?;

        let definition: hub_flow::FlowDefinition =
            serde_json::from_value(revision.definition.clone())
                .map_err(|err| StoreError::Invalid(format!("已发布的编排无法解析: {err}")))?;

        let run_id = ulid::Ulid::generate().to_string();
        let entry = definition
            .entry_nodes()
            .first()
            .map(|id| (*id).to_string())
            .ok_or_else(|| StoreError::Invalid("编排里没有入口节点".to_string()))?;

        runs::upsert_run(
            self.store.pool(),
            &NewRun {
                run_id: &run_id,
                flow_id: flow.id,
                flow_revision: revision.revision,
                trace_id: &trigger.trace_id,
                subject: None,
                trigger: trigger_meta.as_ref(),
                // 入队那一刻就落行，状态是 queued——「投递失败」因此是一个可补偿的
                // 状态，而不是一条查不到的消息
                status: "queued",
                input_summary: Some(&summarize(&trigger)),
                error: None,
                started_at: chrono::Utc::now(),
                finished_at: None,
            },
        )
        .await?;

        let node = definition
            .node(&entry)
            .ok_or_else(|| StoreError::Invalid(format!("入口节点 {entry} 不在编排里")))?;

        // 入口节点的输入就是触发信封（逐跳字段在 prepare 里更新）
        let envelope = prepare_envelope(
            &trigger,
            &trigger.message_id,
            node,
            &run_id,
            &trigger.trace_id,
        );

        self.publish_node(&run_id, flow_name, revision.revision, &entry, envelope)
            .await?;

        Ok(run_id)
    }

    /// 取一批消息并处理，返回处理条数。返回 0 表示这次没取到。
    ///
    /// 单条处理失败**不中断整批**：一条坏消息不该让同批里其他消息陪着一起等下一次
    /// 轮询。失败的那条不 ACK，交给超时重投。
    pub async fn run_once(&self) -> Result<usize, AsyncError> {
        let deliveries = self.bus.receive().await?;
        let count = deliveries.len();

        for delivery in &deliveries {
            match self.handle(delivery).await {
                Ok(Disposition::Done | Disposition::Duplicate | Disposition::Discarded) => {
                    self.bus.ack(delivery).await?;
                }
                Err(err) => {
                    // 这次已经是用完的那一次：送死信并 ACK，给它一个出口。
                    //
                    // 判定必须放在**失败之后**：`attempts` 是「这是第几次投递」，
                    // 处理之前判 `>= max` 会让 `max_delivery = 1` 的消息一次都没跑
                    // 就被判死。放在这里，语义才是「已经试满 max 次且都没成功」。
                    //
                    // 这条分支以前不存在：只会不 ACK 等重投，而重投没有尽头——
                    // 一条永远失败的消息（比如引用了一个不存在的 flow）会一直占着
                    // pending，每一轮都要完整重跑一遍注定失败的逻辑，
                    // 直到躺满 `STREAM_RETENTION_HOURS`（默认 24 小时）被留存巡检捞走。
                    // `BUS_MAX_DELIVERY` 因此是个不生效的配置项。
                    if delivery.exhausted(self.bus.config().max_delivery) {
                        match self.dead_letter(delivery).await {
                            Ok(()) => {
                                // 先写库再 ACK。反过来的话，写库失败时这条消息两头都没了：
                                // 既不在 pending 里等人重投，也不在死信表里等人处理
                                self.bus.ack(delivery).await?;
                            }
                            Err(write_err) => {
                                // 库写不进去就让它留在 pending：下一轮还会判定耗尽、再试一次。
                                // 这里不 ACK 是刻意的——死信落不了库时丢掉消息，
                                // 比让它多跑一轮糟糕得多
                                tracing::warn!(
                                    message_id = %delivery.id,
                                    attempts = delivery.attempts,
                                    error = %write_err,
                                    "写死信失败，消息留在 pending 等下一轮"
                                );
                            }
                        }
                        continue;
                    }

                    // 还没到上限：不 ACK，让它超时后被重投
                    tracing::warn!(
                        message_id = %delivery.id,
                        attempts = delivery.attempts,
                        error = %err,
                        "异步消息处理失败，不 ACK，等待重投"
                    );
                }
            }
        }
        Ok(count)
    }

    /// 把一条重投耗尽的消息写进死信表。
    ///
    /// 与留存巡检那条路径共用 `(stream, stream_id)` 这个判据（`dead_letters::insert`
    /// 会在重复时刷新错误信息而不是插新行）——两条路径判定同一条消息完全可能发生，
    /// 它们不该在表里留下两行。
    async fn dead_letter(&self, delivery: &Delivery) -> Result<(), AsyncError> {
        let message = &delivery.message;
        let summary = message.envelope.as_ref().map(summarize);
        let max_delivery = self.bus.config().max_delivery;
        let error = format!(
            "重投 {} 次仍未成功（上限 {max_delivery}），已停止重投",
            delivery.attempts
        );

        hub_store::dead_letters::insert(
            self.store.pool(),
            &hub_store::dead_letters::NewDeadLetter {
                stream: &self.bus.config().stream,
                stream_id: &delivery.id,
                run_id: Some(&message.run_id),
                flow_name: Some(&message.flow_name),
                node_id: Some(&message.node_id),
                attempts: delivery.attempts as i32,
                error: &error,
                payload_summary: summary.as_deref(),
            },
        )
        .await?;

        Ok(())
    }

    /// 处理一条消息。
    pub async fn handle(&self, delivery: &Delivery) -> Result<Disposition, AsyncError> {
        let message = &delivery.message;
        let pool = self.store.pool();

        if message.kind == Kind::Unspecified as i32 {
            return Ok(Disposition::Discarded);
        }

        // ---- 幂等：键在 ⇒ 再查完成记录 ----
        let key = hub_store::idempotency::node_key(&message.run_id, &message.node_id);
        let expires_at = chrono::Utc::now()
            + chrono::Duration::from_std(self.config.idempotency_ttl)
                .unwrap_or(chrono::Duration::hours(24));

        let claimed =
            hub_store::idempotency::claim(pool, &key, Some(&message.run_id), expires_at).await?;

        if !claimed {
            if runs::node_done(pool, &message.run_id, &message.node_id).await? {
                tracing::info!(
                    run_id = %message.run_id,
                    node = %message.node_id,
                    "节点已经跑过，跳过重投"
                );
                return Ok(Disposition::Duplicate);
            }
            // 键在、完成记录不在：上一个持有者中途死了。拿回来接着做——
            // 跳过它才是真正的丢数据。
            hub_store::idempotency::take_over(pool, &key).await?;
            tracing::warn!(
                run_id = %message.run_id,
                node = %message.node_id,
                "上一个持有者没有完成这个节点，接管重做"
            );
        }

        // ---- 载入编排（锁定在消息里的那一版）----
        let Some(flow) = flows::find_flow(pool, &message.flow_name).await? else {
            return Ok(Disposition::Discarded);
        };
        let Some(revision) = flows::find_revision(pool, flow.id, message.flow_revision).await?
        else {
            return Ok(Disposition::Discarded);
        };
        let definition: hub_flow::FlowDefinition =
            serde_json::from_value(revision.definition.clone())
                .map_err(|err| AsyncError::Invalid(format!("编排无法解析: {err}")))?;
        let exec_plan = plan(&definition)
            .map_err(|err| AsyncError::Invalid(format!("编排排不出执行计划: {err}")))?;
        let Some(node) = definition.node(&message.node_id) else {
            return Ok(Disposition::Discarded);
        };
        let Some(envelope) = message.envelope.clone() else {
            return Ok(Disposition::Discarded);
        };

        // ---- 跑这个节点 ----
        let payload_summary = summarize(&envelope);
        let node_run = self.executor.run_single(node, envelope.clone()).await;
        let outcome = &node_run.outcome;

        runs::insert_run_node(
            pool,
            &NewRunNode {
                run_id: &message.run_id,
                node_id: &outcome.node_id,
                plugin: &outcome.plugin,
                version: &outcome.version,
                instance_id: (!outcome.instance_id.is_empty())
                    .then_some(outcome.instance_id.as_str()),
                attempt: outcome.attempts as i32,
                status: node_status_str(outcome.status),
                duration_ms: Some(outcome.duration_ms as i64),
                io_summary: Some(&payload_summary),
                error: outcome.error.as_deref(),
                started_at: outcome.started_at,
                finished_at: Some(
                    outcome.started_at + chrono::Duration::milliseconds(outcome.duration_ms as i64),
                ),
            },
        )
        .await?;

        let expected = exec_plan.node_count() as i64;

        if outcome.status != NodeStatus::Succeeded {
            // 失败短路：下游不会再跑「等所有节点到齐」永远不成立，必须显式定终态
            let (status, message_text) = failure_of(outcome);
            runs::settle_run(
                pool,
                &message.run_id,
                expected,
                Some((status, &message_text)),
            )
            .await?;
            tracing::warn!(
                run_id = %message.run_id,
                node = %message.node_id,
                %message_text,
                "异步链在节点上失败，已收敛"
            );
            return Ok(Disposition::Done);
        }

        // ---- 投递下游 ----
        //
        // **先投递再收敛**：收敛用的计数读的是 run_nodes，两个动作互不依赖；
        // 而投递失败时我们要走不 ACK 的路（重投），此时 run 也还没被定成终态，
        // 顺序反过来会让一条永远投不出去的消息留下一个「看起来成功了」的 run。
        let output = node_run.output.as_ref().ok_or_else(|| {
            AsyncError::Invalid(format!("节点 {} 成功却没有产出", message.node_id))
        })?;

        for next in exec_plan
            .successors
            .get(&message.node_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let Some(next_node) = definition.node(next) else {
                continue;
            };
            let prepared = prepare_envelope(
                output,
                &envelope.message_id,
                next_node,
                &message.run_id,
                &envelope.trace_id,
            );
            self.publish_node(
                &message.run_id,
                &message.flow_name,
                message.flow_revision,
                next,
                prepared,
            )
            .await?;
        }

        runs::settle_run(pool, &message.run_id, expected, None).await?;

        // 记录异步执行的履约情况：控制台据此回答「这个定时器/MQ 订阅还活着吗」
        self.note_trigger_fired(&message.flow_name).await;

        Ok(Disposition::Done)
    }

    /// 投递一个节点的消息。
    async fn publish_node(
        &self,
        run_id: &str,
        flow_name: &str,
        flow_revision: i32,
        node_id: &str,
        envelope: hub_proto::Envelope,
    ) -> Result<(), AsyncError> {
        let message = BusMessage {
            kind: Kind::Node as i32,
            run_id: run_id.to_string(),
            flow_name: flow_name.to_string(),
            flow_revision,
            node_id: node_id.to_string(),
            enqueued_at_ms: chrono::Utc::now().timestamp_millis(),
            attempts: 1,
            envelope: Some(envelope),
        };

        self.bus.publish(&message).await?;
        Ok(())
    }

    /// 把「这条 flow 的触发器跑过了」记进触发器行。
    ///
    /// 只在**确实有触发器**时写：HTTP 触发的异步执行没有对应的触发器行，`triggers`
    /// 表里那三列是给「需要常驻监听才有意义」的 cron / MQ 用的（见该表的设计说明）。
    ///
    /// 记不上不影响主链路——触发器行的运行状态是给人看的，不是链路的正确性依据。
    async fn note_trigger_fired(&self, flow_name: &str) {
        let pool = self.store.pool();
        let Ok(rows) = triggers::list_of_flow(pool, flow_name).await else {
            return;
        };
        let now = chrono::Utc::now();
        for row in rows {
            if let Err(err) = triggers::record_fired(pool, row.id, now, None).await {
                tracing::warn!(trigger = row.id, error = %err, "记录触发器运行状态失败");
            }
        }
    }
}

/// 失败节点对应的 run 终态与说明。
///
/// 拒绝与失败分开：前者是数据没过插件的校验规则（调用方改数据就能解决），
/// 后者是插件自己出错。混成一个会让排障时找错方向。
fn failure_of(outcome: &crate::NodeOutcome) -> (&'static str, String) {
    let status = match outcome.status {
        NodeStatus::Rejected => "rejected",
        _ => "failed",
    };
    let reason = outcome.error.as_deref().unwrap_or("执行失败");
    (
        status,
        format!("节点 {}（{}）{reason}", outcome.node_id, outcome.plugin),
    )
}

fn node_status_str(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Succeeded => "succeeded",
        NodeStatus::Failed => "failed",
        NodeStatus::Rejected => "rejected",
    }
}

/// 信封摘要。落库用——**不落全量报文**，与同步链同一条留存策略。
fn summarize(envelope: &hub_proto::Envelope) -> String {
    let (type_url, payload_bytes) = envelope
        .payload
        .as_ref()
        .map(|p| (p.type_url.clone(), p.value.len()))
        .unwrap_or_default();

    json!({
        "message_id": envelope.message_id,
        "payload_type": type_url,
        "payload_bytes": payload_bytes,
    })
    .to_string()
}
