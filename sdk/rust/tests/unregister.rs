//! **注销要凭注册时下发的凭证认属主**——插件侧的发送条件，与中台侧的判定。
//!
//! `instance_id` 由插件自己生成、可以跨插件相撞（缺省「主机名-PID」，同一 host 网络下
//! 容器 PID 又都是 1，必然撞），而注销原本只按 `instance_id` 删行、不看属主：撞了 id 的
//! 两个插件里，谁先退出谁就删掉**对方**那一行，对方的心跳仍按 `instance_id` 命中、
//! 完全察觉不到自己从注册表里消失了（见 `crates/hub-registry/src/lib.rs` 的 `unregister`）。
//!
//! 这里钉住修复之后的三条，缺一条这条路就还是漏的：
//!   1. 注销带上**注册回执里那一条**凭证，中台据此摘掉那一行；
//!   2. 注册从未成功过（凭证为空）时**根本不发**注销——那正是删掉属主那一行的路径；
//!   3. 中台侧空凭证或不符的凭证**一行都不删**。

mod support;

use std::time::Duration;

use hubkit::proto::{RegisterRequest, UnregisterRequest};
use hubkit::Config;
use support::*;

fn config_for(mock: &MockHub, instance: &str) -> Config {
    Config {
        hub_addr: mock.url(),
        advertise_addr: PLUGIN_ADVERTISE.to_string(),
        listen_addr: PLUGIN_LISTEN_ANY.to_string(),
        instance_id: instance.to_string(),
        logger: quiet(),
        register_retry_interval: Duration::from_millis(50),
        heartbeat_fallback_interval: Duration::from_millis(100),
        ..Default::default()
    }
}

/// 带对凭证的注销：请求发得出，中台那一行也真的被摘掉。
#[tokio::test]
async fn 注销带上注册时下发的凭证_中台据此摘掉那一行() {
    let mut mock = start_mock(MockConfig::with_interval(1)).await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        config_for(&mock, "it-unregister-1"),
        async move {
            let _ = stop_rx.await;
        },
    ));

    assert!(
        mock.wait_for_registrations(1, Duration::from_secs(5)).await,
        "{}",
        msg("第一次注册")
    );
    let issued = mock.state_token();
    assert!(!issued.is_empty(), "伪造中台应当下发了状态凭证");
    // 注册成功那一刻，中台那边就有一行了——没有它，下面的「删掉了没有」无从谈起
    assert_eq!(
        mock.instances().get("it-unregister-1"),
        Some(&issued),
        "注册成功后中台应当有一行该实例"
    );

    stop_tx.send(()).expect("停止信号应能送达");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("插件应在超时前退出")
        .expect("插件任务不应 panic")
        .expect("优雅退出不该报错");

    let unregisters = mock.unregisters();
    assert_eq!(unregisters.len(), 1, "退出时必须主动注销一次");
    assert_eq!(unregisters[0].instance_id, "it-unregister-1");
    assert_eq!(unregisters[0].reason, "插件优雅退出");
    // 核心断言一：带的是**注册回执里那一条**，不是空串、也不是自己编的
    assert_eq!(
        unregisters[0].state_token, issued,
        "注销必须带上注册时下发的凭证"
    );
    // 核心断言二：凭证对了，中台真的把那一行摘掉了。
    // 只断言「请求发了」是不够的——旧行为下请求也发，只是不带凭证。
    assert!(
        !mock.instances().contains_key("it-unregister-1"),
        "凭证正确的注销应当摘掉那一行，实际还剩 {:?}",
        mock.instances()
    );

    mock.stop();
}

#[tokio::test]
async fn 注册从未成功过时退出不发注销() {
    use hubkit::proto::{RejectCode, Rejection};

    // 注册被拒 = 手里根本没有凭证。这正是那起真实故障里 B 的处境：它的注册被
    // INSTANCE_CONFLICT 挡下（id 与 A 相撞），而它退出时按 `instance_id` 发的那次注销，
    // 删掉的是 A 那一行。修好之后，这条请求压根不该发出去。
    let mut mock = start_mock(MockConfig {
        rejections: vec![Rejection {
            code: RejectCode::InstanceConflict as i32,
            message: "instance_id 已被其它插件占用".to_string(),
            detail: String::new(),
        }],
        ..Default::default()
    })
    .await;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        config_for(&mock, "it-no-token"),
        async move {
            let _ = stop_rx.await;
        },
    ));

    // 先确认插件**真的跑起来了并在反复重试注册**——否则下面的断言只是因为「它压根没跑」
    assert!(
        mock.wait_for_registrations(2, Duration::from_secs(5)).await,
        "被拒后应当持续重试，实际只收到 {} 次",
        mock.registration_count()
    );

    let _ = stop_tx.send(());
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("插件应在超时前退出")
        .expect("插件任务不应 panic")
        .expect("优雅退出不该报错");

    assert!(
        mock.unregisters().is_empty(),
        "注册从未成功过就没有凭证、也没有实例行可摘除，不该发注销，实际发了 {:?}",
        mock.unregisters()
    );

    mock.stop();
}

#[tokio::test]
async fn 中台没下发凭证时退出也不发注销() {
    // `omit_state_token` 模拟「注册成功但这个实例没有凭证」（中台没配状态面，
    // 或迁移前登记的旧行）。注册是成功的，所以插件确实在册——但手里没有凭证，
    // 注销照样不能发：发的会是一条注定被拒的请求。
    let mut mock = start_mock(MockConfig {
        omit_state_token: true,
        ..Default::default()
    })
    .await;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        config_for(&mock, "it-no-state-token"),
        async move {
            let _ = stop_rx.await;
        },
    ));

    assert!(
        mock.wait_for_registrations(1, Duration::from_secs(5)).await,
        "{}",
        msg("注册成功")
    );
    assert_eq!(mock.state_token(), "", "本用例的前提就是中台不下发凭证");

    let _ = stop_tx.send(());
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("插件应在超时前退出")
        .expect("插件任务不应 panic")
        .expect("优雅退出不该报错");

    assert!(
        mock.unregisters().is_empty(),
        "没有凭证就不该发注销，实际发了 {:?}",
        mock.unregisters()
    );
    // 那一行还在，只能等心跳超时被中台的 `sweep_stale` 兜底摘除
    assert!(mock.instances().contains_key("it-no-state-token"));

    mock.stop();
}

#[tokio::test]
async fn 凭证不符时中台一行都不删() {
    // 这一条直接打中台的 Unregister，不经过插件骨架：它验的是**中台的判定**，
    // 也就是插件侧那道「没凭证就不发」之外的第二道防线。
    let mut mock = start_mock(MockConfig::default()).await;
    let mut client =
        hubkit::proto::plugin_registry_client::PluginRegistryClient::connect(mock.url())
            .await
            .expect("应当连得上伪造中台");

    // 让中台那边先有行、有凭证（就当它是 A）
    let registered = client
        .register(RegisterRequest {
            plugin_name: "plugin-a".to_string(),
            version: "1.0.0".to_string(),
            instance_id: "shared-id".to_string(),
            advertise_addr: "http://127.0.0.1:1".to_string(),
            ..Default::default()
        })
        .await
        .expect("注册请求本身不该失败")
        .into_inner();
    assert!(registered.accepted, "伪造中台应当接受注册");
    let token = registered.state_token;
    assert!(!token.is_empty());
    assert!(mock.instances().contains_key("shared-id"));

    // 1) 空凭证：旧版 SDK 的行为，必须一行都不删
    client
        .unregister(UnregisterRequest {
            instance_id: "shared-id".to_string(),
            reason: "旧版插件退出".to_string(),
            state_token: String::new(),
        })
        .await
        .expect("凭证不符时中台回的是成功响应，不是错误");
    assert!(
        mock.instances().contains_key("shared-id"),
        "空凭证不该删掉任何行"
    );

    // 2) 别人的凭证：撞了 id 的另一方拿自己那张来注销，同样删不动
    client
        .unregister(UnregisterRequest {
            instance_id: "shared-id".to_string(),
            reason: "撞了 id 的另一个插件退出".to_string(),
            state_token: "别的插件手里的凭证".to_string(),
        })
        .await
        .expect("凭证不符时中台回的是成功响应，不是错误");
    assert!(
        mock.instances().contains_key("shared-id"),
        "凭证不符不该删掉任何行"
    );

    // 3) 属主自己的凭证：这时才摘得掉——所以上面两条的「还在」不是因为行本来就在
    client
        .unregister(UnregisterRequest {
            instance_id: "shared-id".to_string(),
            reason: "属主退出".to_string(),
            state_token: token.clone(),
        })
        .await
        .expect("注销请求本身不该失败");
    assert!(
        !mock.instances().contains_key("shared-id"),
        "凭证相符时应当摘掉那一行"
    );

    // 三次请求都记下来了：中台回成功不代表它认了，插件只能靠**不发**来避免误删
    assert_eq!(mock.unregisters().len(), 3);

    mock.stop();
}
