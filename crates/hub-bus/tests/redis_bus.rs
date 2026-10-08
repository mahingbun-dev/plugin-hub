//! 总线对**真实 Redis** 的验证。
//!
//! 这一层不 mock：消费组、`XAUTOCLAIM` 的接管语义、`XACK` + `XDEL` 的配对效果，
//! 全都是 Redis 侧的行为，打桩只能验证「我们以为自己发了什么命令」，验证不了
//! 「Redis 实际做了什么」。而这里每一条待验证的性质——接管、去重、堆积——都是
//! 断言 Redis 的行为。

use std::time::Duration;

use hub_bus::{Bus, BusConfig, BusError};
use hub_proto::BusMessage;

/// 测试用 Redis。刻意用 db 9 而不是应用在用的 db 2：测试数据不该和应用数据混在一起。
/// 测试用的 Redis。
///
/// 默认硬编码到本地 db 9，**刻意不读 REDIS_URL**：.env 里的那个指向应用的
/// db 2，测试键混进去会污染正在跑的中台。
/// TEST_REDIS_URL 只用来在 CI 里指向另一个端口——CI 自己起一个 Redis，
/// 不假定 runner 上正好有一个（跟测试 PG 用非默认端口是同一个理由）。
fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/9".to_string())
}

/// 每条测试独占一个 Stream 名。共享 Stream 会让用例之间互相看见对方的消息，
/// 那种偶发的、依赖执行顺序的失败最难查。
fn unique_stream(tag: &str) -> String {
    format!("test:bus:{tag}:{}", ulid::Ulid::generate())
}

fn config(tag: &str, consumer: &str) -> BusConfig {
    BusConfig {
        stream: unique_stream(tag),
        group: "test-workers".to_string(),
        consumer: consumer.to_string(),
        batch: 16,
        // 测试里不要真的等 5 秒
        block: Duration::from_millis(100),
        claim_min_idle: Duration::from_millis(150),
        max_delivery: 3,
        max_depth: 1_000,
    }
}

fn message(run_id: &str, node_id: &str) -> BusMessage {
    BusMessage {
        kind: hub_proto::v1::bus_message::Kind::Node as i32,
        run_id: run_id.to_string(),
        flow_name: "test-flow".to_string(),
        flow_revision: 1,
        node_id: node_id.to_string(),
        enqueued_at_ms: 1_700_000_000_000,
        attempts: 1,
        envelope: None,
    }
}

async fn bus(tag: &str, consumer: &str) -> Bus {
    Bus::connect(&redis_url(), config(tag, consumer))
        .await
        .expect("连接 Redis 失败——本机 6379 上应有 Redis")
}

#[tokio::test]
async fn 发布的消息能被消费到() {
    let bus = bus("roundtrip", "c1").await;

    bus.publish(&message("run-1", "node-a"))
        .await
        .expect("入队应成功");

    let deliveries = bus.receive().await.expect("应取到消息");
    assert_eq!(deliveries.len(), 1);

    let got = &deliveries[0].message;
    assert_eq!(got.run_id, "run-1");
    assert_eq!(got.node_id, "node-a");
    assert_eq!(
        got.flow_revision, 1,
        "版本必须原样带回来：不锁定版本会让同一次执行跑到两个版本上"
    );
    assert_eq!(got.enqueued_at_ms, 1_700_000_000_000);
    assert_eq!(deliveries[0].attempts, 1, "新消息是第 1 次投递");

    bus.ack(&deliveries[0]).await.expect("ACK 应成功");
}

#[tokio::test]
async fn ack_之后消息从_stream_上消失() {
    let bus = bus("ack", "c1").await;

    bus.publish(&message("run-2", "node-a")).await.unwrap();
    assert_eq!(bus.depth().await.unwrap(), 1);

    let deliveries = bus.receive().await.unwrap();
    assert_eq!(deliveries.len(), 1);
    bus.ack(&deliveries[0]).await.unwrap();

    assert_eq!(
        bus.depth().await.unwrap(),
        0,
        "XACK 只摘 pending 列表，XDEL 才真正释放内存——两者必须一起做，\
         否则 XLEN 会一直涨，堆积降级的判据也就失真了"
    );
}

#[tokio::test]
async fn 没_ack_的消息会被另一个消费者接管() {
    let config = config("claim", "consumer-a");
    let stream = config.stream.clone();

    let first = Bus::connect(&redis_url(), config.clone()).await.unwrap();
    let second = Bus::connect(
        &redis_url(),
        BusConfig {
            consumer: "consumer-b".to_string(),
            ..config
        },
    )
    .await
    .unwrap();

    first.publish(&message("run-3", "node-a")).await.unwrap();

    // consumer-a 取走但「忘了」ACK（模拟进程被杀）
    let taken = first.receive().await.unwrap();
    assert_eq!(taken.len(), 1);
    drop(taken);

    // 另一个消费者此刻还看不到它——消息还挂在 a 的 pending 里，且没到接管时限
    assert!(
        second.receive().await.unwrap().is_empty(),
        "刚取走的消息不该立刻被别人抢走，否则正常的慢调用会被误判成消费者已死"
    );

    // 等到超过接管时限
    tokio::time::sleep(Duration::from_millis(250)).await;

    let claimed = second.receive().await.unwrap();
    assert_eq!(
        claimed.len(),
        1,
        "闲置超时的消息必须能被接管——少了这一步，一次 OOM Kill 就能让一批消息永久卡住"
    );
    assert_eq!(claimed[0].message.run_id, "run-3");
    assert!(
        claimed[0].attempts >= 2,
        "接管来的消息投递次数应当递增，否则死信判定永远不会触发：{}",
        claimed[0].attempts
    );

    second.ack(&claimed[0]).await.unwrap();
    assert_eq!(second.depth().await.unwrap(), 0, "stream: {stream}");
}

#[tokio::test]
async fn 堆积到上限时拒绝入队() {
    let bus = Bus::connect(
        &redis_url(),
        BusConfig {
            max_depth: 2,
            ..config("overload", "c1")
        },
    )
    .await
    .unwrap();

    bus.publish(&message("run-4", "n1")).await.unwrap();
    bus.publish(&message("run-4", "n2")).await.unwrap();

    let err = bus
        .publish(&message("run-4", "n3"))
        .await
        .expect_err("到顶就该拒绝");

    assert!(
        matches!(err, BusError::Overloaded { depth: 2, limit: 2 }),
        "应报出真实深度与上限，调用方据此决定降级还是稍后重试：{err}"
    );
    assert_eq!(
        bus.depth().await.unwrap(),
        2,
        "被拒绝的消息不该已经写进去——先污染再补救等于没拒绝"
    );
}

#[tokio::test]
async fn 坏字节报解码错误而不是当成处理失败() {
    let bus = bus("malformed", "c1").await;

    // 直接往流里塞一段不是 protobuf 的字节
    let client = redis::Client::open(redis_url()).expect("Redis 地址应合法");
    let mut conn = client.get_connection_manager().await.expect("应连上");
    let _: String = redis::cmd("XADD")
        .arg(&bus.config().stream)
        .arg("*")
        .arg("d")
        .arg(vec![0xffu8, 0xff, 0xff, 0xff])
        .query_async(&mut conn)
        .await
        .expect("塞入应成功");

    let err = bus.receive().await.expect_err("坏字节应报错");
    assert!(
        matches!(err, BusError::Decode(_)),
        "要与「处理失败」区分开：重投一万次也是同样的字节，正确的动作是进死信而不是重投：{err}"
    );
}

#[tokio::test]
async fn 可以扫出躺太久的消息() {
    let bus = bus("older", "c1").await;

    bus.publish(&message("run-5", "n1")).await.unwrap();

    // 刚写进去的消息还「年轻」
    assert!(
        bus.older_than(Duration::from_secs(3600), 64)
            .await
            .unwrap()
            .is_empty(),
        "一小时前的阈值不该扫出刚刚写的消息"
    );

    // 把阈值压到 0 秒，就都算「躺太久」了
    let stale = bus.older_than(Duration::ZERO, 64).await.unwrap();
    assert_eq!(stale.len(), 1, "留存巡检要能看见它");
    assert_eq!(stale[0].message.as_ref().expect("应解得开").run_id, "run-5");
    assert!(
        !stale[0].id.is_empty(),
        "id 必须给出来——解不开的消息也要能靠它摘掉"
    );
}

#[tokio::test]
async fn 解不开的消息也能被扫出来() {
    let bus = bus("stale-broken", "c1").await;

    let client = redis::Client::open(redis_url()).expect("地址合法");
    let mut conn = client.get_connection_manager().await.expect("应连上");
    let _: String = redis::cmd("XADD")
        .arg(bus.config().stream.as_str())
        .arg("*")
        .arg("d")
        .arg(vec![0xffu8, 0xff, 0xff, 0xff])
        .query_async(&mut conn)
        .await
        .expect("塞入应成功");

    let stale = bus.older_than(Duration::ZERO, 64).await.unwrap();
    assert_eq!(
        stale.len(),
        1,
        "解不开的消息同样占着 Redis 内存、同样躺太久了；\
         这里跳过它会让它永远留在流里，每次巡检都报同一个错"
    );
    assert!(stale[0].message.is_none(), "确实解不开");
    assert!(!stale[0].id.is_empty(), "但 id 还在，能摘掉它");
}

#[tokio::test]
async fn 空闲的阻塞读不该被客户端掐断() {
    // 一个比 redis-rs 默认响应超时（500ms）长得多的阻塞读。
    //
    // 这条用例守的是一个很容易漏掉的坑：`ConnectionManager` 默认把响应超时设成
    // 500ms，而 `XREADGROUP ... BLOCK` 在没消息时会**故意**占满整个 block 时间。
    // 两者一撞，每次空闲读都在 500ms 处被掐断报「timed out」——而消息其实照常能
    // 收到，所以这个故障只会表现为日志里刷警告，不会让任何一条用例变红。
    let bus = Bus::connect(
        &redis_url(),
        BusConfig {
            block: Duration::from_secs(2),
            ..config("idle-block", "c1")
        },
    )
    .await
    .expect("连接应成功");

    let started = std::time::Instant::now();
    let deliveries = bus
        .receive()
        .await
        .expect("空闲时的阻塞读应当正常返回空，而不是超时");
    assert!(deliveries.is_empty());
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "确实等过一段——太快说明根本没走阻塞读，用例也就没测到东西：{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn 重复建组不报错() {
    let config = config("group", "c1");
    let stream = config.stream.clone();

    let first = Bus::connect(&redis_url(), config.clone()).await.unwrap();
    // 同一个 Stream 再连一次（模拟服务重启）
    let second = Bus::connect(&redis_url(), config).await.unwrap();

    second.publish(&message("run-6", "n1")).await.unwrap();
    let deliveries = second.receive().await.unwrap();
    assert_eq!(
        deliveries.len(),
        1,
        "重启后仍应能收到消息（stream: {stream}）"
    );
    first.ack(&deliveries[0]).await.unwrap();
}
