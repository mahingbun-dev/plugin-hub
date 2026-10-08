//! 常驻后台任务：异步消费、留存清理。
//!
//! 这三件事都不该挂在请求路径上：它们要一直跑、跑多久不确定、失败也不该让某个请求
//! 变慢。所以各自一个 tokio 任务，用一个共享的关停信号收口。
//!
//! **关停时的取舍**：消费循环被取消时可能正好处理到一半——那条消息没 ACK，会留在
//! pending 里，等 `claim_min_idle` 之后被重新接管。这是刻意的：宁可让下一位重做一次，
//! 也不要为了「干净退出」而在关停路径上等一个可能永远不返回的调用。

use std::time::Duration;

use hub_bus::Bus;
use hub_engine::AsyncExecutor;
use hub_store::Store;
use hub_store::dead_letters::{self, NewDeadLetter};
use hub_store::runs::{self, is_terminal};
use tokio::sync::watch;
use tracing::{info, warn};

/// 消费出错后的退避。Redis 挂了的时候不能空转打它。
const CONSUMER_BACKOFF: Duration = Duration::from_secs(2);

/// 留存巡检的周期。
///
/// 分钟级：这些数据的时间尺度是天，跑得比这更勤只是在做无用功。而清一次的量
/// 也用不着更勤——每次带上限，攒下来的量会在若干个周期里被逐步清完。
const RETENTION_INTERVAL: Duration = Duration::from_secs(600);

/// 一轮巡检最多处理多少条超期消息。
///
/// 有上限是为了**让巡检本身是可中断的**：一次性处理十万条会让它在关停时卡很久，
/// 而且会给 PG 一个突然的大事务。剩下的下一轮接着清。
const RETENTION_BATCH: usize = 256;

/// 留存策略。与 `hub-core::Config` 一一对应，在这里收成一个结构体是为了让巡检函数
/// 的参数表不至于变成七个数。
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    /// 消息在总线上最多躺多久；超了进死信
    pub stream: Duration,

    /// 执行记录的保留期
    pub runs: Duration,

    /// 调用链 span 的保留期
    pub spans: Duration,

    /// 死信自身的保留期（已重放的可以更早清，这个值是给没重放的那批）
    pub dead_letters: Duration,

    /// 注册拒绝留痕的保留期
    pub rejections: Duration,
}

/// 起若干个消费者。
///
/// **每个消费者要各有各的 Bus**：Redis 按消费者名字判断「谁的活卡住了」，同一个
/// 进程里的多个消费者共用名字的话，`XAUTOCLAIM` 会让它们互相抢对方正在处理的活。
pub fn spawn_consumers(executors: Vec<AsyncExecutor>, shutdown: watch::Receiver<bool>) {
    for (index, exec) in executors.into_iter().enumerate() {
        let mut rx = shutdown.clone();
        tokio::spawn(async move {
            info!(worker = index, "异步消费者已启动");
            loop {
                tokio::select! {
                    _ = rx.changed() => break,
                    result = exec.run_once() => {
                        if let Err(err) = result {
                            // 连续失败时退避：Redis 挂了的时候空转只会让恢复更慢
                            warn!(worker = index, error = %err, "异步消费失败，稍后重试");
                            tokio::time::sleep(CONSUMER_BACKOFF).await;
                        }
                    }
                }
            }
            info!(worker = index, "异步消费者已停止");
        });
    }
}

/// 起留存巡检。
pub fn spawn_retention(
    store: Store,
    bus: Bus,
    policy: RetentionPolicy,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RETENTION_INTERVAL);
        // 第一次 tick 会立即触发，跳过以免启动瞬间做一轮无意义的清理
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {}
            }
            sweep(&store, &bus, &policy).await;
        }
    });
}

/// 跑一轮留存清理。
///
/// **每一项单独兜错**：某一项失败（比如 PG 抖了一下）不该让后面几项都不跑。清理是
/// 幂等的，下一轮会补上。
pub async fn sweep(store: &Store, bus: &Bus, policy: &RetentionPolicy) {
    let pool = store.pool();
    let now = chrono::Utc::now();

    // ---- 总线上躺太久的消息 ----
    match bus.older_than(policy.stream, RETENTION_BATCH).await {
        Ok(stale) if !stale.is_empty() => {
            let mut dead_lettered = 0usize;
            let mut dropped = 0usize;

            for entry in stale {
                // 已经跑完的 run 的迟到消息直接丢：它的数据早就处理过了，
                // 送进死信只会让死信列表变成一个需要人逐个确认的噪音堆
                let settled = match entry.message.as_ref() {
                    Some(message) if !message.run_id.is_empty() => {
                        match runs::find_run(pool, &message.run_id).await {
                            Ok(Some(run)) => is_terminal(&run.status),
                            _ => false,
                        }
                    }
                    _ => false,
                };

                if settled {
                    if bus.discard(&entry.id).await.is_ok() {
                        dropped += 1;
                    }
                    continue;
                }

                let (run_id, flow_name, node_id, summary) = match entry.message.as_ref() {
                    Some(message) => (
                        Some(message.run_id.as_str()),
                        Some(message.flow_name.as_str()),
                        Some(message.node_id.as_str()),
                        message.envelope.as_ref().map(summarize),
                    ),
                    None => (None, None, None, None),
                };

                let error = if entry.message.is_some() {
                    "在总线上躺太久，重投窗口已过"
                } else {
                    "在总线上躺太久，且消息体解不开"
                };

                let inserted = dead_letters::insert(
                    pool,
                    &NewDeadLetter {
                        stream: &bus.config().stream,
                        stream_id: &entry.id,
                        run_id,
                        flow_name,
                        node_id,
                        attempts: bus.config().max_delivery as i32,
                        error,
                        payload_summary: summary.as_deref(),
                    },
                )
                .await;

                match inserted {
                    Ok(_) => {
                        dead_lettered += 1;
                        let _ = bus.discard(&entry.id).await;
                    }
                    Err(err) => warn!(error = %err, message_id = %entry.id, "写死信失败"),
                }
            }

            if dead_lettered > 0 || dropped > 0 {
                info!(dead_lettered, dropped, "留存巡检：超期消息已处理");
            }
        }
        Ok(_) => {}
        Err(err) => warn!(error = %err, "扫描超期消息失败"),
    }

    // ---- 各按各的保留期清理 ----
    purge(
        "调用链 span",
        hub_store::spans::purge_spans_before(pool, now - to_chrono(policy.spans)).await,
    );
    purge(
        "执行记录",
        runs::purge_runs_before(pool, now - to_chrono(policy.runs)).await,
    );
    purge(
        "幂等键",
        hub_store::idempotency::purge_expired(pool, now).await,
    );
    purge(
        "超限载荷",
        hub_store::payloads::purge_expired(pool, now).await,
    );
    purge(
        "死信",
        dead_letters::purge(
            pool,
            // 已经重放过的可以更早清：它已经完成使命，留着只占空间
            now - to_chrono(policy.dead_letters / 3),
            // 没重放的多留一阵——那正是需要人去看的一批
            now - to_chrono(policy.dead_letters),
        )
        .await,
    );
    purge(
        "注册拒绝留痕",
        hub_store::rejections::purge_before(pool, now - to_chrono(policy.rejections)).await,
    );
}

fn purge(what: &str, result: Result<u64, hub_store::StoreError>) {
    match result {
        Ok(0) => {}
        Ok(count) => info!(count, "留存巡检：已清理{what}"),
        Err(err) => warn!(error = %err, "清理{what}失败"),
    }
}

/// `std::time::Duration` → `chrono::Duration`。超长时退化成 0（不清理）而不是负数。
fn to_chrono(duration: Duration) -> chrono::Duration {
    chrono::Duration::from_std(duration).unwrap_or_default()
}

/// 信封摘要。落库用——**不落全量报文**，与 run / span 的留存策略一致。
fn summarize(envelope: &hub_proto::Envelope) -> String {
    let (type_url, bytes) = envelope
        .payload
        .as_ref()
        .map(|p| (p.type_url.clone(), p.value.len()))
        .unwrap_or_default();

    serde_json::json!({
        "message_id": envelope.message_id,
        "payload_type": type_url,
        "payload_bytes": bytes,
    })
    .to_string()
}
