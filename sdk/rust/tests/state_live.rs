//! **对着真中台的 L3 验收**：注册 → 下发凭证 → 注入 → 状态读写往返。
//!
//! 默认**跳过**（真中台不是每条流水线上都有），设了 `HUBKIT_LIVE_HUB_ADDR` 才跑：
//!
//! ```console
//! HUBKIT_LIVE_HUB_ADDR=http://127.0.0.1:8093 cargo test --offline --test state_live -- --nocapture
//! ```
//!
//! 中台地址就是插件面地址——注册面与状态面在**同一个端口**上。
//!
//! 这一层验的是 mock 验不了的东西：
//!
//!   - 中台真的会查库、把我们带上来的 `x-hub-state-token` 反查成插件名，
//!     并强制拼上前缀（所以「插件自报 namespace 就能越界」在这条路上是不成立的）；
//!   - 键名规则、单值上限、扫描上限这些**中台侧**的判据与我们的本地拦法不冲突：
//!     本地放行的请求，中台也放行。
//!
//! 与 mock 的集成测试（`tests/state.rs`）是互补关系，不是替代关系。

mod support;

use std::time::Duration;

use hubkit::{Config, StateError};
use support::*;

/// 真中台地址。没设就跳过——**不失败**：真中台不是每条流水线上都有。
fn hub_addr() -> Option<String> {
    match std::env::var("HUBKIT_LIVE_HUB_ADDR") {
        Ok(addr) if !addr.trim().is_empty() => Some(addr),
        _ => {
            eprintln!(
                "跳过 L3 验收：未设置 HUBKIT_LIVE_HUB_ADDR。\
                 对着真中台跑：HUBKIT_LIVE_HUB_ADDR=http://127.0.0.1:8093 cargo test --test state_live"
            );
            None
        }
    }
}

/// 状态命名空间固定成这个（不用进程号）：跑完之后能从中台侧（Redis）按它核对前缀。
const NS: &str = "liverust";

#[tokio::test]
async fn 真中台上注册注入与状态读写往返() {
    let Some(hub) = hub_addr() else {
        return;
    };

    let port = free_port();
    let instance = format!("it-rust-state-live-{}", std::process::id());

    let plugin = StatePlugin::named("rust-state-live");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let task = tokio::spawn(hubkit::run_with_shutdown(
        plugin.clone(),
        Config {
            hub_addr: hub.clone(),
            // 中台会连它做可达性探测，所以必须是**从中台那边拨得通**的地址
            advertise_addr: local_addr(port),
            listen_addr: format!("127.0.0.1:{port}"),
            instance_id: instance.clone(),
            // 这一层要看得到注册/心跳/拒绝原因，日志开到 info
            logger: hubkit::Logger::new(hubkit::Level::Info),
            register_retry_interval: Duration::from_millis(500),
            heartbeat_fallback_interval: Duration::from_secs(2),
            state_call_timeout: Duration::from_secs(5),
            gateway_call_timeout: Duration::from_secs(5),
        },
        async move {
            let _ = stop_rx.await;
        },
    ));

    // ---- 注册 + 注入。中台没起来时这里会等到超时，错误信息就是那句「状态客户端注入」----
    let state = plugin.wait_for_state(Duration::from_secs(15)).await;
    eprintln!("已注册到 {hub} 并拿到状态客户端（实例 {instance}）");

    // ---- Put → Get 往返（走真实的中台 → Redis）----
    state
        .put(NS, "k1", b"v1", Duration::ZERO)
        .await
        .expect("写状态应当成功");
    assert_eq!(
        state.get(NS, "k1").await.expect("读状态应当成功"),
        Some(b"v1".to_vec()),
        "读回来的必须就是写进去的那份"
    );

    // 不存在的键是 None，不是错误
    assert_eq!(
        state.get(NS, "nope").await.expect("读不存在的键不该报错"),
        None
    );

    // 空值：中台分得清「键不存在」与「值是空字节」，我们也要分得清
    state
        .put(NS, "k2", b"", Duration::ZERO)
        .await
        .expect("空值也要能写");
    assert_eq!(
        state.get(NS, "k2").await.expect("读状态应当成功"),
        Some(Vec::new())
    );

    // ---- Scan：前缀 + limit ----
    state.put(NS, "scan-a", b"1", Duration::ZERO).await.unwrap();
    state.put(NS, "scan-b", b"2", Duration::ZERO).await.unwrap();
    // 另一个命名空间：scan 不该越界看到它（中台的前缀隔离）
    state
        .put("liverust-other", "scan-c", b"3", Duration::ZERO)
        .await
        .expect("另一个命名空间也要能写");

    let mut keys: Vec<String> = state
        .scan(NS, "scan-", 100)
        .await
        .expect("扫描应当成功")
        .into_iter()
        .map(|e| e.key)
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["scan-a", "scan-b"], "前缀只该匹配本命名空间的键");

    let limited = state.scan(NS, "", 2).await.expect("限制条数的扫描");
    assert_eq!(limited.len(), 2, "limit 应当被中台照用");

    // ---- Delete ----
    assert!(state.delete(NS, "k1").await.expect("删除应当成功"));
    assert_eq!(state.get(NS, "k1").await.expect("读状态应当成功"), None);
    assert!(
        !state.delete(NS, "k1").await.expect("删不存在的键不该报错"),
        "删不存在的键返回 false"
    );

    // ---- TTL 路径：给一个短 TTL，等它过期 ----
    state
        .put(NS, "ttl", b"short", Duration::from_secs(1))
        .await
        .expect("带 TTL 的写应当成功");
    assert_eq!(
        state.get(NS, "ttl").await.unwrap(),
        Some(b"short".to_vec()),
        "TTL 未到时应当读得到"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        state.get(NS, "ttl").await.unwrap(),
        None,
        "TTL 到了之后键应当消失（中台侧是 Redis 的 EX 语义）"
    );

    // ---- 本地拦下的参数：中台也会拒，但本地先拦（这一层只确认**放行的都没被中台拒**）----
    let err = state
        .put(
            NS,
            "big",
            &vec![0u8; hubkit::MAX_VALUE_BYTES + 1],
            Duration::ZERO,
        )
        .await
        .expect_err("超限的值应当本地被拦");
    assert!(err.to_string().contains("1048576"), "{err}");
    assert!(matches!(
        state.get("bad:ns", "k").await.unwrap_err(),
        StateError::Invalid(_)
    ));

    // 留一个键**不删**：跑完之后从中台侧核对前缀（`hub:state:rust-state-live:liverust:*`）。
    // 给它一个 TTL，别在验证库的 Redis 里留垃圾。
    state
        .put(NS, "alive", b"yes", Duration::from_secs(600))
        .await
        .expect("留下一个可核对的键");
    eprintln!("留了一个键供核对：hub:state:rust-state-live:{NS}:alive（600s 后自动消失）");

    // 其余的自己收拾干净——验证库是共享的，别留一地键（`alive` 有 TTL，会自己走）
    for (ns, key) in [
        (NS, "k2"),
        (NS, "scan-a"),
        (NS, "scan-b"),
        ("liverust-other", "scan-c"),
    ] {
        state.delete(ns, key).await.expect("清理应当成功");
    }
    let left = state.scan(NS, "", 100).await.expect("清理后扫描");
    assert_eq!(
        left.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
        vec!["alive"],
        "跑完只该剩下那个带 TTL 的核对键"
    );

    stop_tx.send(()).ok();
    task.await.expect("插件应当正常退出").expect("正常退出");
}
