//! 触发器调度：cron 定时与 MQ 订阅。
//!
//! 这两类是「需要常驻监听才有意义」的触发方式——HTTP 触发天然有个入口，它们没有，
//! 不装配就没人知道要跑。定义存在 `triggers` 表里，调度器每 [`RELOAD_INTERVAL`]
//! 重新读一次：改配置不该要求重启中台。
//!
//! # 两条明确的语义
//!
//! **错过的定时不补跑**。中台停机期间到点的 cron，恢复后不会一次性补上。补跑看起来
//! 「更不丢事」，实际后果是恢复瞬间涌出一串积压的执行——把一次漏跑放大成一次雪崩，
//! 而且那些执行拿着的是过期的输入。要补就跑一次手动触发或重放死信，那是人来决定的。
//!
//! **MQ 订阅的载荷取自消息的 `d` 字段，且必须是 JSON**。约定与中台自己在总线上发的
//! 消息完全一致——外部系统只需写 `XADD <stream> * d '{"orderId":"SO-1"}'`，
//! 不必先学一套新格式。

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use chrono::Utc;
use cron::Schedule;
use hub_bus::{Bus, BusConfig};
use hub_engine::AsyncExecutor;
use hub_proto::{Envelope, PayloadType};
use hub_store::Store;
use hub_store::model::TriggerRow;
use hub_store::triggers;
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::{info, warn};

/// 重新读取触发器定义的周期。改配置不该要求重启中台。
const RELOAD_INTERVAL: Duration = Duration::from_secs(30);

/// MQ 消费取不到消息时最多阻塞多久。短一点，好让循环有机会看一眼配置变更与关停信号。
const MQ_BLOCK: Duration = Duration::from_secs(3);

/// 一轮 cron 最多触发几条。防止配置写错时一次涌出上百个执行。
const MAX_FIRES_PER_ROUND: usize = 32;

/// 起 cron 调度循环。
pub fn spawn_cron(store: Store, exec: AsyncExecutor, mut shutdown: watch::Receiver<bool>) {
    tokio::spawn(async move {
        info!("cron 调度器已启动");
        loop {
            let plans = match load_cron_plans(&store).await {
                Ok(plans) => plans,
                Err(err) => {
                    warn!(error = %err, "读取 cron 触发器失败");
                    Vec::new()
                }
            };

            if plans.is_empty() {
                // 没有定时任务：只等配置变更或关停
                tokio::select! {
                    _ = shutdown.changed() => break,
                    _ = tokio::time::sleep(RELOAD_INTERVAL) => continue,
                }
            }

            let Some(next) = plans.iter().map(|p| p.next).min() else {
                continue;
            };
            let wait = (next - Utc::now()).to_std().unwrap_or(Duration::ZERO);

            tokio::select! {
                _ = shutdown.changed() => break,
                // 睡到最近的一次触发；到点后重新加载一遍，把刚触发的排到下一次
                _ = tokio::time::sleep(wait) => {}
                _ = tokio::time::sleep(RELOAD_INTERVAL) => {}
            }

            fire_due(&store, &exec, &plans).await;
        }
        info!("cron 调度器已停止");
    });
}

/// 起 MQ 订阅循环。
///
/// 每个触发器一条独立的消费循环，各自订阅自己声明的 Stream。多个中台实例共享同一个
/// 消费组，于是同一条外部消息只会被其中一个实例取到——这正是想要的分工。
pub fn spawn_mq(
    store: Store,
    exec: AsyncExecutor,
    redis_url: String,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        info!("MQ 订阅器已启动");
        // 每条触发器一个循环句柄，重载时按需增删
        let mut running: HashMap<i64, tokio::task::JoinHandle<()>> = HashMap::new();

        loop {
            match triggers::list_enabled(store.pool(), Some(triggers::KIND_MQ)).await {
                Ok(rows) => {
                    for row in rows {
                        if running.contains_key(&row.id) {
                            continue;
                        }
                        let Ok(bus) = connect_external(&redis_url, &row).await else {
                            warn!(trigger = row.id, "连接外部 Stream 失败，跳过这条订阅");
                            continue;
                        };
                        let handle = spawn_one_mq(store.clone(), exec.clone(), bus, row.clone());
                        running.insert(row.id, handle);
                    }

                    // 被停用或删掉的触发器：停掉它的循环
                    let alive: Vec<i64> = enabled_mq_ids(&store).await;
                    running.retain(|id, handle| {
                        if alive.contains(id) {
                            true
                        } else {
                            handle.abort();
                            false
                        }
                    });
                }
                Err(err) => warn!(error = %err, "读取 MQ 触发器失败"),
            }

            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tokio::time::sleep(RELOAD_INTERVAL) => {}
            }
        }

        for (_, handle) in running {
            handle.abort();
        }
        info!("MQ 订阅器已停止");
    });
}

/// 当前启用中的 MQ 触发器 id。用来停掉已被禁用或删掉的那些订阅循环。
async fn enabled_mq_ids(store: &Store) -> Vec<i64> {
    triggers::list_enabled(store.pool(), Some(triggers::KIND_MQ))
        .await
        .map(|rows| rows.into_iter().map(|r| r.id).collect())
        .unwrap_or_default()
}

/// 一个 MQ 触发器的消费循环。
fn spawn_one_mq(
    store: Store,
    exec: AsyncExecutor,
    bus: Bus,
    row: TriggerRow,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let deliveries = match bus.receive_raw().await {
                Ok(deliveries) => deliveries,
                Err(err) => {
                    warn!(trigger = row.id, error = %err, "读取外部 Stream 失败");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            for delivery in deliveries {
                match payload_of(&delivery.payload) {
                    Ok(payload) => {
                        if let Err(err) = fire(&store, &exec, &row, payload, "mq").await {
                            // 入队失败**不 ACK**：消息还留在外部流上，重投会再来一次。
                            // 这里不能吞掉——丢掉它才是真的丢数据。
                            warn!(trigger = row.id, error = %err.to_string(), "MQ 触发失败，不 ACK");
                            continue;
                        }
                        let _ =
                            triggers::record_fired(store.pool(), row.id, Utc::now(), None).await;
                    }
                    Err(err) => {
                        warn!(trigger = row.id, message_id = %delivery.id, error = %err, "外部消息载荷解析失败");
                        let _ = triggers::record_fired(
                            store.pool(),
                            row.id,
                            Utc::now(),
                            Some("外部消息载荷解析失败"),
                        )
                        .await;
                    }
                }
                let _ = bus.discard(&delivery.id).await;
            }
        }
    })
}

/// 连接一个外部 Stream。
async fn connect_external(redis_url: &str, row: &TriggerRow) -> Result<Bus, hub_bus::BusError> {
    let stream = row
        .config
        .get("stream")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    Bus::connect(
        redis_url,
        BusConfig {
            stream,
            // 外部流也有消费组：**组名必须与中台自己的流区分开**，否则两个订阅者
            // 会互相以为对方是「同一个组的另一个实例」而分工，结果谁都拿不全
            group: format!("hub-trigger-{}", row.id),
            consumer: format!("{}-{}", row.flow_id, std::process::id()),
            block: MQ_BLOCK,
            ..BusConfig::default()
        },
    )
    .await
}

/// 从外部消息的字节里解析出业务载荷。
///
/// 约定：消息的 `d` 字段（`hub_bus` 会取出来）必须是 JSON。是别的东西就报错——让调用
/// 方知道它发错了格式，而不是拿到一份看似成功的空载荷。
fn payload_of(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(bytes).map_err(|err| format!("消息体不是 JSON: {err}"))
}

/// 一条 cron 触发器与它的下一次触发时刻。
///
/// 计划里不存 `Schedule`：算完下一次就够了，下一轮会重新读配置并重算——把表达式
/// 缓存下来会让「改了表达式但没生效」变成一种要重启才能排除的故障。
struct CronPlan {
    trigger: TriggerRow,
    next: chrono::DateTime<Utc>,
}

/// 读出启用中的 cron 触发器，算出各自的下一次触发时刻。
async fn load_cron_plans(store: &Store) -> Result<Vec<CronPlan>, hub_store::StoreError> {
    let rows = triggers::list_enabled(store.pool(), Some(triggers::KIND_CRON)).await?;

    let mut plans = Vec::with_capacity(rows.len());
    for row in rows {
        let expr = row.config.get("expr").and_then(Value::as_str).unwrap_or("");

        let schedule = match Schedule::from_str(expr) {
            Ok(schedule) => schedule,
            Err(err) => {
                // 表达式写错不该让整轮调度停摆：记在触发器行上，控制台立刻看得见，
                // 其余的定时照跑
                warn!(trigger = row.id, expr, error = %err, "cron 表达式非法，跳过这条");
                let pool = store.pool();
                let _ = triggers::record_fired(
                    pool,
                    row.id,
                    Utc::now(),
                    Some("cron 表达式非法，这条触发器不会运行"),
                )
                .await;
                continue;
            }
        };

        let Some(next) = schedule.upcoming(Utc).next() else {
            continue;
        };

        plans.push(CronPlan { trigger: row, next });
    }

    Ok(plans)
}

/// 把到点的都触发掉。
async fn fire_due(store: &Store, exec: &AsyncExecutor, plans: &[CronPlan]) {
    let now = Utc::now();
    let mut fired = 0usize;

    for plan in plans {
        if plan.next > now {
            continue;
        }
        if fired >= MAX_FIRES_PER_ROUND {
            // 配置写错时（比如每秒一次）不该让一轮涌出上百个执行
            warn!(
                limit = MAX_FIRES_PER_ROUND,
                "一轮触发数达上限，其余留到下一轮"
            );
            break;
        }
        fired += 1;

        // 载荷带上「这次是哪个计划点触发的」：排障时最常见的疑问是「它到底按时跑了没有」
        let payload = json!({
            "scheduled_at": plan.next.to_rfc3339(),
            "trigger": plan.trigger.name,
        });

        match fire(store, exec, &plan.trigger, payload, "cron").await {
            Ok(()) => {
                let _ = triggers::record_fired(store.pool(), plan.trigger.id, now, None).await;
                info!(trigger = plan.trigger.id, flow = %plan.trigger.flow_id, "cron 已触发");
            }
            Err(err) => {
                // 触发失败记在触发器行上——**定时器最典型的故障是静默失效**，
                // 不记下来控制台就只剩「它好像没跑了」这一句猜测
                let _ = triggers::record_fired(
                    store.pool(),
                    plan.trigger.id,
                    now,
                    Some(&err.to_string()),
                )
                .await;
                warn!(trigger = plan.trigger.id, error = %err, "cron 触发失败");
            }
        }
    }
}

/// 触发一次：把载荷装配成信封并投给异步链。
///
/// cron 走异步链而不是同步执行：一次定时可能触发一条长链，把它压在一次调度循环里
/// 会让后面的定时全部顺延。
async fn fire(
    store: &Store,
    exec: &AsyncExecutor,
    trigger: &TriggerRow,
    payload: Value,
    kind: &str,
) -> Result<(), TriggerError> {
    let flow_name = flow_name_of(store, trigger.flow_id).await?;

    let encoded = hub_proto::encode_payload(&payload)
        .map_err(|err| TriggerError::Invalid(err.to_string()))?;
    let envelope = Envelope {
        message_id: ulid::Ulid::generate().to_string(),
        trace_id: hub_api::trace::new_trace_id(),
        r#type: PayloadType::Event as i32,
        payload: Some(encoded),
        meta: HashMap::from([("trigger".to_string(), kind.to_string())]),
        ..Default::default()
    };

    exec.enqueue(
        &flow_name,
        envelope,
        Some(json!({
            "kind": kind,
            "trigger_id": trigger.id,
            "trigger_name": trigger.name,
        })),
    )
    .await
    .map_err(|err| TriggerError::Enqueue(err.to_string()))?;

    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    #[error("触发器配置不成立: {0}")]
    Invalid(String),

    #[error("入队失败: {0}")]
    Enqueue(String),
}

/// 触发器挂的是 flow_id，异步链要的是 flow 名。
///
/// 每次都现查一次而不是把名字冗余在触发器行上：flow 改名是个合法操作，冗余一份名字
/// 就会在改名后指向一个不存在的东西，而且要到触发时才炸。
async fn flow_name_of(store: &Store, flow_id: i64) -> Result<String, TriggerError> {
    hub_store::flows::find_flow_by_id(store.pool(), flow_id)
        .await
        .map_err(|err| TriggerError::Invalid(err.to_string()))?
        .map(|flow| flow.name)
        .ok_or_else(|| TriggerError::Invalid(format!("flow id {flow_id} 已经不存在了")))
}
