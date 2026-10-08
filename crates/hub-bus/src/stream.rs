//! Redis Stream 的命令层。
//!
//! 这一层只做「把 Redis 的回复转成 Rust 结构」这件事，不含任何总线语义——消费组怎么用、
//! 什么时候重投、什么时候进死信，都在 [`crate::Bus`] 里。分开是为了让语义那部分能被
//! 读懂，而不是淹没在命令拼装里。
//!
//! 用 `redis` 的 streams 类型化 API 而不是自己拼 `redis::cmd` + 解析 `redis::Value`：
//! Stream 的回复是三层嵌套数组，手写解析是纯粹的出错来源，而这些类型已经跟着 crate
//! 一起在真实 Redis 上跑过了。只有 `XPENDING` 的范围形态是例外——crate 只给了 summary
//! 形态的类型，而 summary 会把**整个** pending 列表拉回来。

use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use redis::streams::{
    StreamAutoClaimOptions, StreamAutoClaimReply, StreamRangeReply, StreamReadOptions,
    StreamReadReply,
};

use crate::BusError;

/// 消息体存放的字段名。
///
/// 固定用单字符 `d`：Stream 的每条 entry 都是一个 field-value 列表，字段名会随每条
/// 消息一起占据内存。这里没有传第二份数据的需要。
const PAYLOAD_FIELD: &str = "d";

/// `XAUTOCLAIM` 的起始位置。`0-0` 表示从头扫 pending 列表。
const CLAIM_START: &str = "0-0";

/// 从 Stream 里读到的一条原始消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMessage {
    /// Redis 的消息 id（`<毫秒>-<序号>`）。ACK 与 XDEL 都要用它
    pub id: String,

    /// 消息体的原始字节（编码后的 `BusMessage`）
    pub payload: Vec<u8>,
}

/// 建消费组。已存在时返回 `Ok(false)`。
///
/// `MKSTREAM` 让 Stream 在不存在时被创建——首次部署时不该还要求先手工 XADD 一条。
/// 起始位置用 `$`：消费组只看建组之后的消息，历史消息由留存策略负责清理，
/// 不该在服务重启时被重放一遍。
pub async fn ensure_group(
    conn: &mut ConnectionManager,
    stream: &str,
    group: &str,
) -> Result<bool, BusError> {
    let result: Result<(), redis::RedisError> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(stream)
        .arg(group)
        .arg("$")
        .arg("MKSTREAM")
        .query_async(conn)
        .await;

    match result {
        Ok(()) => Ok(true),
        Err(err) if err.code() == Some("BUSYGROUP") => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// 追加一条消息，返回它的 id。
pub async fn add(
    conn: &mut ConnectionManager,
    stream: &str,
    payload: &[u8],
) -> Result<String, BusError> {
    let id: String = redis::cmd("XADD")
        .arg(stream)
        // `*` 让 Redis 自己生成 id：中台不需要自己编排时间戳，
        // 而 Redis 的 `<毫秒>-<序号>` 天然单调，正好做「谁先谁后」的判据
        .arg("*")
        .arg(PAYLOAD_FIELD)
        .arg(payload)
        .query_async(conn)
        .await?;
    Ok(id)
}

/// Stream 里现有多少条消息。用作堆积深度。
pub async fn len(conn: &mut ConnectionManager, stream: &str) -> Result<usize, BusError> {
    let len: usize = conn.xlen(stream).await?;
    Ok(len)
}

/// 按时间范围读消息（`XRANGE`），**不改变 pending 状态**。
///
/// 留给留存清理用：它要找出「在 Stream 里躺了太久」的消息，其中一部分**从未被任何
/// 消费者读过**（消费者整体挂了）——那些不在 pending 列表里，`XAUTOCLAIM` 够不着，
/// 只能按 id 里的时间戳扫出来。
///
/// Stream 的 id 前缀就是写入时刻的毫秒数，所以时间范围可以直接表达成 id 范围，
/// 不需要额外的索引。
pub async fn read_range(
    conn: &mut ConnectionManager,
    stream: &str,
    from_ms: u64,
    to_ms: u64,
    count: usize,
) -> Result<Vec<RawMessage>, BusError> {
    let reply: StreamRangeReply = conn
        .xrange_count(stream, format!("{from_ms}-0"), format!("{to_ms}-0"), count)
        .await?;

    reply
        .ids
        .into_iter()
        .map(|entry| {
            Ok(RawMessage {
                id: entry.id,
                payload: extract_payload(&entry.map)?,
            })
        })
        .collect()
}

/// 一条消息已被投递过多少次。用于判断是否该进死信。
pub async fn delivery_count(
    conn: &mut ConnectionManager,
    stream: &str,
    group: &str,
    id: &str,
) -> Result<i64, BusError> {
    // 用范围形态 `XPENDING <stream> <group> <id> <id> 1` 精确取一条：
    // summary 形态会把整个 pending 列表拉回来，而重投路径上不该付这个代价
    let reply: redis::Value = redis::cmd("XPENDING")
        .arg(stream)
        .arg(group)
        .arg(id)
        .arg(id)
        .arg(1)
        .query_async(conn)
        .await?;

    let entries: Vec<(String, String, u64, i64)> =
        redis::from_redis_value(reply).map_err(|err| BusError::Malformed(err.to_string()))?;

    Ok(entries.first().map_or(1, |(_, _, _, count)| *count))
}

/// 确认一条消息并把它从 Stream 里删掉。
///
/// **XACK 与 XDEL 必须成对**：XACK 只是把消息从 pending 列表里摘掉，消息体仍然留在
/// Stream 里占内存。中台的持久化记录在 PG（run / run_node / 死信表），Stream 是纯粹的
/// 传输通道——处理完就该让它消失，否则 `XLEN` 会一直涨，堆积降级的判据也就失真了。
pub async fn ack(
    conn: &mut ConnectionManager,
    stream: &str,
    group: &str,
    id: &str,
) -> Result<(), BusError> {
    let _: () = redis::pipe()
        .cmd("XACK")
        .arg(stream)
        .arg(group)
        .arg(id)
        .ignore()
        .cmd("XDEL")
        .arg(stream)
        .arg(id)
        .ignore()
        .query_async(conn)
        .await?;
    Ok(())
}

/// 读一批新消息（`>` 语义：从未投递给任何消费者的消息）。
pub async fn read_new(
    conn: &mut ConnectionManager,
    stream: &str,
    group: &str,
    consumer: &str,
    count: usize,
    block_ms: u64,
) -> Result<Vec<RawMessage>, BusError> {
    let options = StreamReadOptions::default()
        .group(group, consumer)
        .count(count)
        .block(block_ms as usize);

    let reply: StreamReadReply = conn.xread_options(&[stream], &[">"], &options).await?;

    let mut messages = Vec::new();
    for key in reply.keys {
        for entry in key.ids {
            messages.push(RawMessage {
                id: entry.id,
                payload: extract_payload(&entry.map)?,
            });
        }
    }
    Ok(messages)
}

/// 接管闲置超过 `min_idle_ms` 的消息——它原来的消费者多半已经死了。
///
/// 这是「消费者进程被杀」时的唯一兜底：它没有机会 ACK，消息会一直挂在 pending 里。
/// 少了这一步，一次 OOM Kill 就能让一批消息永久卡住。
pub async fn claim_stale(
    conn: &mut ConnectionManager,
    stream: &str,
    group: &str,
    consumer: &str,
    min_idle_ms: u64,
    count: usize,
) -> Result<Vec<RawMessage>, BusError> {
    let reply: StreamAutoClaimReply = conn
        .xautoclaim_options(
            stream,
            group,
            consumer,
            min_idle_ms,
            CLAIM_START,
            StreamAutoClaimOptions::default().count(count),
        )
        .await?;

    let mut messages = Vec::new();
    for entry in reply.claimed {
        // 被 XDEL 掉的消息会出现在 claimed 里但字段表为空，跳过
        if entry.map.is_empty() {
            continue;
        }
        messages.push(RawMessage {
            id: entry.id,
            payload: extract_payload(&entry.map)?,
        });
    }
    Ok(messages)
}

/// 从 entry 的字段表里取消息体。
fn extract_payload(
    map: &std::collections::HashMap<String, redis::Value>,
) -> Result<Vec<u8>, BusError> {
    let value = map
        .get(PAYLOAD_FIELD)
        .ok_or_else(|| BusError::Malformed(format!("消息里没有 {PAYLOAD_FIELD} 字段")))?;
    redis::from_redis_value(value.clone()).map_err(|err| BusError::Malformed(err.to_string()))
}
