//! **被摘除后自愈**——心跳回执带 `reregister_required` 时重走注册流程。
//!
//! 这是「实例掉线后能恢复」的唯一路径：中台把实例摘掉之后，插件那边的 gRPC 一切正常、
//! 心跳也照发，**只有重新注册能让它回来**。缺了这条分支，插件会安静地「活着但接不进来」，
//! 而这正是最难排查的一种故障——两边看自己都是好的。

mod support;

use std::time::Duration;

use hubkit::Config;
use support::*;

/// 心跳周期压到 100ms（中台回 0，插件用兜底值），让自愈在一秒内就能观察到。
fn fast_config(mock: &MockHub) -> Config {
    Config {
        hub_addr: mock.url(),
        advertise_addr: PLUGIN_ADVERTISE.to_string(),
        listen_addr: PLUGIN_LISTEN_ANY.to_string(),
        instance_id: "it-self-heal".to_string(),
        logger: quiet(),
        register_retry_interval: Duration::from_millis(50),
        heartbeat_fallback_interval: Duration::from_millis(100),
        // 其余取缺省：这些用例与 HubState 无关，状态调用超时用缺省值即可
        ..Default::default()
    }
}

#[tokio::test]
async fn 中台要求重注册时插件会立刻重新注册() {
    // 收到第 2 拍之后中台开始要求重注册（reregister_required = true）
    let mut mock = start_mock(MockConfig {
        heartbeat_interval_seconds: 0,
        reregister_after_beats: 2,
        ..Default::default()
    })
    .await;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        fast_config(&mock),
        async move {
            let _ = stop_rx.await;
        },
    ));

    assert!(
        mock.wait_for_registrations(1, Duration::from_secs(5)).await,
        "{}",
        msg("首次注册")
    );

    // 核心断言：中台要求重注册之后，插件**真的又注册了一次**
    assert!(
        mock.wait_for_registrations(2, Duration::from_secs(5)).await,
        "被要求重注册后应当重新注册，实际只注册了 {} 次（心跳 {} 拍）",
        mock.registration_count(),
        mock.heartbeats()
    );

    // 重新注册用的是同一个 instance_id：中台据此把它当成「同一个实例回来了」
    let registrations = mock.registrations();
    assert!(registrations.len() >= 2);
    assert_eq!(registrations[1].instance_id, "it-self-heal");
    assert_eq!(registrations[1].plugin_name, "test-plugin");

    let _ = stop_tx.send(());
    let _ = task.await;
    mock.stop();
}

#[tokio::test]
async fn 心跳被拒也走重新注册() {
    // accepted = false 与 reregister_required = true 在中台侧是两件事，
    // 但对插件都是「你已经不在册了」——两条都得回到注册流程。
    let mut mock = start_mock(MockConfig {
        heartbeat_interval_seconds: 0,
        reregister_after_beats: 1,
        ..Default::default()
    })
    .await;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        fast_config(&mock),
        async move {
            let _ = stop_rx.await;
        },
    ));

    assert!(
        mock.wait_for_registrations(2, Duration::from_secs(5)).await,
        "心跳被拒后应当重新注册，实际注册 {} 次",
        mock.registration_count()
    );

    let _ = stop_tx.send(());
    let _ = task.await;
    mock.stop();
}

#[tokio::test]
async fn 反复被要求重注册也不会把注册打成自旋() {
    // 中台一直要求重注册（每拍都是）时，插件应当**按心跳周期**回来，
    // 而不是零延迟地反复注册——后者等于对中台发起注册洪泛。
    // 第 1 拍之后就一直要求重注册
    let mut mock = start_mock(MockConfig {
        heartbeat_interval_seconds: 0,
        reregister_after_beats: 1,
        ..Default::default()
    })
    .await;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        fast_config(&mock),
        async move {
            let _ = stop_rx.await;
        },
    ));

    // 给它 1 秒：心跳周期 100ms、注册是本地环回，自旋的话这里会堆出成百上千次
    tokio::time::sleep(Duration::from_secs(1)).await;
    let count = mock.registration_count();
    assert!(count >= 2, "应当至少重注册过一次，实际 {count} 次");
    assert!(
        count < 100,
        "一小时内的注册次数远超心跳周期能解释的量（{count} 次）——注册被打成自旋了"
    );

    let _ = stop_tx.send(());
    let _ = task.await;
    mock.stop();
}
