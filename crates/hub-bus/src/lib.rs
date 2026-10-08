//! 事件总线：Redis Stream 上的消费组、ACK、重投与死信的**传输层**。
//!
//! 这一层只管「消息怎么安全地从 A 到 B」，不关心消息里是什么业务。上层的异步编排
//! （`hub-engine` 的异步执行器）拿 [`Delivery`] 去跑节点，跑成功就 [`Bus::ack`]，
//! 跑失败就让它超时被重新接管；重投次数用完（[`Delivery::exhausted`]）则写进死信表
//! 再 [`Bus::ack`]——**死信的落库由上层负责**，总线只提供取到消息与确认消息这两件事。
//!
//! 为什么是 Redis Stream 而不是 RocketMQ：
//!
//! - 中台的队列是**中短途**——消息活不过几分钟，真正的持久化在 PG（run / run_node /
//!   span 都落库）。为这个量级单独运维一套 MQ 不划算。
//! - Stream 的消费组 + `XAUTOCLAIM` 恰好覆盖三件必需的事：ACK、重投、**消费者挂掉后
//!   消息被接管**。少了最后一件，一次 OOM Kill 就能让一批消息永久卡住。
//! - 代价是 Redis 内存，所以我们的应答是两条：`XLEN` 超限时**拒绝入队**（背压），
//!   以及留存巡检把躺太久的消息送进死信而不是静默丢弃。
//!
//! 投递语义是 **at-least-once**：不丢的代价就是会重。所以幂等不是可选项，它是这个
//! 模块能被安全使用的前提——上层的 [`Delivery`] 处理必须自己按 `(run_id, node_id)`
//! 去重。

pub mod stream;

use std::time::Duration;

use hub_proto::BusMessage;
use prost::Message as _;
use redis::aio::ConnectionManager;

/// 默认 Stream 名。所有异步 flow 的节点消息共用一条流，靠消息里的 `flow_name` 路由。
const DEFAULT_STREAM: &str = "hub:flows";
const DEFAULT_GROUP: &str = "hub-workers";

/// 响应超时相对阻塞读的余量。
///
/// 阻塞读（`XREADGROUP ... BLOCK`）在没消息时会**故意**占满整个 block 时间才返回，
/// 所以响应超时必须比 block 长；否则每一次空闲读都会被客户端自己掐断。
const RESPONSE_TIMEOUT_MARGIN: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusConfig {
    /// Stream 名
    pub stream: String,

    /// 消费组名。多实例部署时**必须相同**——不同消费组会各自收到全量消息
    pub group: String,

    /// 本消费者的名字。多实例部署时必须互不相同，否则重投接管会互相抢
    pub consumer: String,

    /// 一次最多取几条
    pub batch: usize,

    /// `XREADGROUP` 的阻塞时长。到点没消息就返回空，让调用方有机会做别的事
    /// （比如检查关停信号）
    pub block: Duration,

    /// 闲置多久的消息视为「原消费者已死」，可以被接管重投
    pub claim_min_idle: Duration,

    /// 投递多少次后进死信
    pub max_delivery: i64,

    /// 堆积上限。`XLEN` 超过它时 [`Bus::publish`] 直接拒绝——这是背压的落点
    pub max_depth: usize,
}

impl Default for BusConfig {
    fn default() -> Self {
        Self {
            stream: DEFAULT_STREAM.to_string(),
            group: DEFAULT_GROUP.to_string(),
            consumer: default_consumer(),
            batch: 16,
            block: Duration::from_secs(5),
            claim_min_idle: Duration::from_secs(60),
            max_delivery: 5,
            max_depth: 100_000,
        }
    }
}

/// 默认消费者名：主机名 + 进程号。
///
/// 必须**每个进程都不同**，否则两个实例会以同一个消费者身份注册，`XAUTOCLAIM`
/// 接管时拿到的是「自己也有一份」的错觉，重投会打到仍然活着的那个实例上。
fn default_consumer() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "hub".to_string());
    format!("{host}-{}", std::process::id())
}

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("Redis 操作失败: {0}")]
    Redis(#[from] redis::RedisError),

    #[error("消息编码失败: {0}")]
    Encode(#[from] prost::EncodeError),

    #[error("消息解码失败: {0}")]
    Decode(#[from] prost::DecodeError),

    /// Redis 的回复形状不对。**不是**业务错误，是「我们和 Redis 的约定漂了」
    #[error("总线消息格式不对: {0}")]
    Malformed(String),

    /// 堆积到顶，拒绝入队。
    ///
    /// 单独一个变体而不是复用 Redis 错误：调用方对它的处置完全不同——它要做的
    /// 是**降级或稍后重试**，而不是「总线坏了」。
    #[error("总线堆积已达上限（{depth}/{limit}），拒绝入队")]
    Overloaded { depth: usize, limit: usize },
}

/// 取到的一条消息。
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    /// Redis 的消息 id。ACK 时要用
    pub id: String,

    pub message: BusMessage,

    /// 这是第几次投递（从 1 开始）。达到 `max_delivery` 就该进死信
    pub attempts: i64,
}

impl Delivery {
    /// 是否已重投到上限。
    pub fn exhausted(&self, max_delivery: i64) -> bool {
        self.attempts >= max_delivery
    }
}

/// 总线。`Clone` 是共享同一条连接（`ConnectionManager` 内部是 `Arc`）。
#[derive(Clone)]
pub struct Bus {
    conn: ConnectionManager,
    config: BusConfig,
}

impl Bus {
    /// 连接并建好消费组。
    ///
    /// **建组放在这里而不是留给调用方**：漏建组的表现是「消息发出去了但永远收不到」，
    /// 且没有任何报错——这类静默失败值得在构造函数里一次性排除。
    pub async fn connect(url: &str, config: BusConfig) -> Result<Self, BusError> {
        let client = redis::Client::open(url)?;

        // **必须显式设响应超时**。redis-rs 的 `ConnectionManager` 默认把它设成 500ms，
        // 而我们的阻塞读默认 `BLOCK 5s`——没消息时 `XREADGROUP` 会占满 5 秒才返回，
        // 于是每一次空闲读都在 500ms 处被掐断，报「Redis 操作失败: timed out」。
        // 表现是每个空闲消费者每几百毫秒刷一条警告，而**消息其实照常能收到**，
        // 所以很容易被当成噪音放过去。余量给 10 秒，既容下阻塞读，也仍能发现
        // 「命令发出去十几秒没有回音」这种真正的异常。
        let conn = client
            .get_connection_manager_with_config(
                redis::aio::ConnectionManagerConfig::new()
                    .set_response_timeout(Some(config.block + RESPONSE_TIMEOUT_MARGIN)),
            )
            .await?;

        let mut bus = Self { conn, config };
        let created =
            stream::ensure_group(&mut bus.conn, &bus.config.stream, &bus.config.group).await?;
        if created {
            tracing::info!(
                stream = %bus.config.stream,
                group = %bus.config.group,
                "已创建消费组"
            );
        }
        Ok(bus)
    }

    pub fn config(&self) -> &BusConfig {
        &self.config
    }

    /// 入队一条消息，返回 Redis 的消息 id。
    ///
    /// 先看 `XLEN` 再 `XADD`（两次往返）。选两次而不是「先加再查」：后者在堆积已经
    /// 到顶时仍然把消息写进去，拒绝就变成了「先污染再补救」。本机 Redis 每秒能处理
    /// 十万级命令，目标量级（5000 msg/s）下这两次往返不构成瓶颈——真到瓶颈时该换的
    /// 是队列方案，不是省这一次往返。
    pub async fn publish(&self, message: &BusMessage) -> Result<String, BusError> {
        let mut conn = self.conn.clone();
        let depth = stream::len(&mut conn, &self.config.stream).await?;
        if depth >= self.config.max_depth {
            return Err(BusError::Overloaded {
                depth,
                limit: self.config.max_depth,
            });
        }

        let payload = message.encode_to_vec();
        stream::add(&mut conn, &self.config.stream, &payload).await
    }

    /// 当前堆积深度。
    pub async fn depth(&self) -> Result<usize, BusError> {
        let mut conn = self.conn.clone();
        stream::len(&mut conn, &self.config.stream).await
    }

    /// 取一批消息。
    ///
    /// 先接管闲置超时的（上一任消费者可能已经死了），再读新的。两步都做是刻意的：
    /// 只读新的会让挂掉那次留下的消息永远卡在 pending 里；只接管则收不到新流量。
    pub async fn receive(&self) -> Result<Vec<Delivery>, BusError> {
        let mut conn = self.conn.clone();
        let mut deliveries = Vec::new();

        let stale = stream::claim_stale(
            &mut conn,
            &self.config.stream,
            &self.config.group,
            &self.config.consumer,
            self.config.claim_min_idle.as_millis() as u64,
            self.config.batch,
        )
        .await?;

        for raw in stale {
            // 接管来的消息要查它的真实投递次数，否则「重投了多少次」永远停在 1，
            // 死信判定就永远不会触发
            let attempts =
                stream::delivery_count(&mut conn, &self.config.stream, &self.config.group, &raw.id)
                    .await
                    .unwrap_or(1);
            deliveries.push(decode(raw.id, raw.payload, attempts)?);
        }

        let fresh = stream::read_new(
            &mut conn,
            &self.config.stream,
            &self.config.group,
            &self.config.consumer,
            self.config.batch,
            self.config.block.as_millis() as u64,
        )
        .await?;

        for raw in fresh {
            deliveries.push(decode(raw.id, raw.payload, 1)?);
        }

        Ok(deliveries)
    }

    /// 确认处理完成，把消息从 Stream 上摘掉。
    pub async fn ack(&self, delivery: &Delivery) -> Result<(), BusError> {
        let mut conn = self.conn.clone();
        stream::ack(
            &mut conn,
            &self.config.stream,
            &self.config.group,
            &delivery.id,
        )
        .await
    }

    /// 取一批消息，**不解码**。
    ///
    /// 给「订阅外部 Stream」用：那些消息不是中台发的，消息体是别的系统写的 JSON，
    /// 拿 [`BusMessage`] 去解只会失败。总线在这里退化成「一个带 ACK 的流」。
    pub async fn receive_raw(&self) -> Result<Vec<RawDelivery>, BusError> {
        let mut conn = self.conn.clone();
        let mut out = Vec::new();

        let stale = stream::claim_stale(
            &mut conn,
            &self.config.stream,
            &self.config.group,
            &self.config.consumer,
            self.config.claim_min_idle.as_millis() as u64,
            self.config.batch,
        )
        .await?;
        for entry in stale {
            out.push(RawDelivery {
                id: entry.id,
                payload: entry.payload,
                claimed: true,
            });
        }

        let fresh = stream::read_new(
            &mut conn,
            &self.config.stream,
            &self.config.group,
            &self.config.consumer,
            self.config.batch,
            self.config.block.as_millis() as u64,
        )
        .await?;
        for entry in fresh {
            out.push(RawDelivery {
                id: entry.id,
                payload: entry.payload,
                claimed: false,
            });
        }

        Ok(out)
    }

    /// 按 id 确认并摘掉一条消息。留存巡检用——它手上的消息是 [`StaleMessage`]，
    /// 可能根本解不出 [`Delivery`]。
    pub async fn discard(&self, id: &str) -> Result<(), BusError> {
        let mut conn = self.conn.clone();
        stream::ack(&mut conn, &self.config.stream, &self.config.group, id).await
    }

    /// 扫出在 Stream 里躺了超过 `older_than` 的消息，不改变 pending 状态。
    ///
    /// 留存巡检用它。返回的消息交给上层决定进死信还是丢弃——总线不替上层做这个判断，
    /// 因为「这条消息还有没有救」取决于业务（比如一个已经完成的 run 的迟到消息就该丢）。
    ///
    /// `message` 是 `Option`：**解不开的消息也要能被扫出来**。它们同样占着 Redis 的
    /// 内存，同样是「躺太久了」，而如果这里直接报错跳过，它们会永远留在流里、每次巡检
    /// 都报一次同样的错。上层拿到 `None` 时可以只按 id 送死信。
    pub async fn older_than(
        &self,
        older_than: Duration,
        limit: usize,
    ) -> Result<Vec<StaleMessage>, BusError> {
        let mut conn = self.conn.clone();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let cutoff_ms = now_ms.saturating_sub(older_than.as_millis() as u64);

        let raw = stream::read_range(&mut conn, &self.config.stream, 0, cutoff_ms, limit).await?;

        let mut stale = Vec::with_capacity(raw.len());
        for entry in raw {
            let message = BusMessage::decode(entry.payload.as_slice()).ok();
            stale.push(StaleMessage {
                id: entry.id,
                message,
            });
        }
        Ok(stale)
    }
}

/// 一条尚未解码的投递。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDelivery {
    /// Redis 的消息 id。ACK 时要用
    pub id: String,

    /// 消息体的原始字节
    pub payload: Vec<u8>,

    /// 是不是从别人手里接管来的（上一个消费者没 ACK）
    pub claimed: bool,
}

/// 一条躺太久的消息。
#[derive(Debug, Clone, PartialEq)]
pub struct StaleMessage {
    /// Redis 的消息 id。即使解不开，也能靠它把消息摘掉
    pub id: String,

    /// 解出来的消息；字节不合法时是 `None`
    pub message: Option<BusMessage>,
}

/// 解码一条消息。解码失败**不重投**——重投一万次也是同样的字节。
///
/// 调用方拿到这个错误后应当把消息送进死信，这正是 [`BusError::Decode`] 与
/// 「处理失败」需要区分开的原因。
fn decode(id: String, payload: Vec<u8>, attempts: i64) -> Result<Delivery, BusError> {
    Ok(Delivery {
        id,
        message: BusMessage::decode(payload.as_slice())?,
        attempts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 消费者名默认带上主机与进程号() {
        let config = BusConfig::default();
        assert!(
            config.consumer.contains(&std::process::id().to_string()),
            "进程号是区分同一主机上多实例的关键：{}",
            config.consumer
        );
    }

    #[test]
    fn 默认配置是保守的() {
        let config = BusConfig::default();
        assert!(
            config.claim_min_idle >= Duration::from_secs(30),
            "太短会把慢调用误判成消费者已死"
        );
        assert!(config.max_delivery >= 2, "只有一次机会等于没有重投");
        assert!(config.max_depth > 0);
    }

    #[test]
    fn 投递次数到上限才算耗尽() {
        let delivery = |attempts| Delivery {
            id: "1-1".to_string(),
            message: BusMessage::default(),
            attempts,
        };

        assert!(!delivery(4).exhausted(5), "还差一次不该进死信");
        assert!(delivery(5).exhausted(5), "到上限就该进死信");
        assert!(delivery(6).exhausted(5), "接管也可能让它一次多跳几次");
    }

    #[test]
    fn 坏字节解不出来也不谎报成功() {
        let err = decode("1-1".to_string(), vec![0xff, 0xff, 0xff], 1).expect_err("应当解不出来");
        assert!(matches!(err, BusError::Decode(_)), "{err}");
    }
}
