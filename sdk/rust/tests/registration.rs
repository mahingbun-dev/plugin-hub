//! 注册 / 心跳 / 优雅退出——**插件与中台之间那条链路的全部行为**。
//!
//! 这几条测试刻意都用「伪造中台收到了什么」来断言，而不是「插件没报错」：
//! 插件不报错太容易了（注册循环会一直重试），有意义的判据是**中台那边看到了什么**。
//!
//! 插件监听地址用 [`PLUGIN_LISTEN_ANY`]（`127.0.0.1:0`，内核在 bind 的原子时刻
//! 分配端口）：mock 从不回拨插件，预挑端口只剩竞态没有收益——并行门禁下被别的
//! 进程抢走预挑端口，插件会因 `AddrInUse` **静默退出**，表现为莫名其妙的
//! 「插件没注册上来」（根因排查记录见 [`free_port`] 的文档）。

mod support;

use std::time::Duration;

use hubkit::Config;
use support::*;

fn config_for(mock: &MockHub, instance: &str) -> Config {
    Config {
        hub_addr: mock.url(),
        advertise_addr: PLUGIN_ADVERTISE.to_string(),
        listen_addr: PLUGIN_LISTEN_ANY.to_string(),
        instance_id: instance.to_string(),
        logger: quiet(),
        // 测试里把两个周期都压到毫秒级：这两个旋钮存在就是为了这个
        register_retry_interval: Duration::from_millis(50),
        heartbeat_fallback_interval: Duration::from_millis(100),
        // 其余取缺省：这些用例与 HubState 无关，状态调用超时用缺省值即可
        ..Default::default()
    }
}

#[tokio::test]
async fn 插件会自注册_按周期发心跳_退出时主动注销() {
    let mut mock = start_mock(MockConfig::with_interval(1)).await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let mut task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        config_for(&mock, "it-register-1"),
        async move {
            let _ = stop_rx.await;
        },
    ));

    // ---- 注册 ----
    let registered = mock.wait_for_registrations(1, Duration::from_secs(5)).await;
    if !registered && task.is_finished() {
        // 插件若是「静默退出」（bind 失败只随返回值走，不打日志），把真实退出原因
        // 带进 panic——否则这种失败只有一句「没注册上来」，根因无从查起
        let result = tokio::time::timeout(Duration::from_millis(100), &mut task).await;
        panic!("{}；插件已提前退出：{result:?}", msg("第一次注册"));
    }
    assert!(registered, "{}", msg("第一次注册"));

    let registration = mock.last_registration().expect("应当收到注册请求");
    assert_eq!(registration.plugin_name, "test-plugin");
    assert_eq!(registration.version, "0.1.0");
    assert_eq!(registration.instance_id, "it-register-1");
    // advertise 地址必须**原样**上报：中台按它做可达性探测
    assert_eq!(registration.advertise_addr, PLUGIN_ADVERTISE);
    let manifest = registration
        .manifest
        .as_ref()
        .expect("注册请求必须带 manifest");
    assert_eq!(manifest.name, "test-plugin");
    assert_eq!(manifest.consumes.len(), 1);
    assert_eq!(manifest.consumes[0].fq_name, "google.protobuf.Struct");
    // 只用 Struct 载荷的插件没有自己的 proto，descriptor 就该是空的
    assert!(
        registration.descriptor_set.is_empty(),
        "Struct-only 插件不应提交 descriptor"
    );

    // ---- 心跳 ----
    // 中台在回执里指定了 1 秒，插件必须照它发——不是照自己的兜底值（100ms）
    assert!(
        mock.wait_for_heartbeats(2, Duration::from_secs(6)).await,
        "{}（实际 {} 拍）",
        msg("按中台指定周期发心跳"),
        mock.heartbeats()
    );
    let beats = mock.heartbeats();
    assert!(
        beats >= 2,
        "中台指定 1s 周期，6 秒内至少该有 2 拍，实际 {beats}"
    );
    // 上限用来钉住「插件用的是中台给的周期，不是自己的 100ms 兜底」——
    // 用兜底值的话 6 秒里会有几十拍
    assert!(
        beats <= 8,
        "心跳过密（{beats} 拍）—— 插件多半没采用中台回执里的周期"
    );

    // ---- 优雅退出 ----
    stop_tx.send(()).expect("停止信号应能送达");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("插件应在超时前退出")
        .expect("插件任务不应 panic")
        .expect("优雅退出不该报错");

    let unregisters = mock.unregisters();
    assert_eq!(unregisters.len(), 1, "退出时必须主动注销一次");
    assert_eq!(unregisters[0].instance_id, "it-register-1");
    assert_eq!(unregisters[0].reason, "插件优雅退出");

    mock.stop();
}

#[tokio::test]
async fn 中台没给心跳周期时用兜底值() {
    // heartbeat_interval_seconds = 0 表示中台没指定，插件该用 Config 里的兜底值
    let mut mock = start_mock(MockConfig::default()).await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        config_for(&mock, "it-fallback"),
        async move {
            let _ = stop_rx.await;
        },
    ));

    assert!(mock.wait_for_registrations(1, Duration::from_secs(5)).await);
    assert!(
        mock.wait_for_heartbeats(3, Duration::from_secs(5)).await,
        "兜底周期 100ms，5 秒内该有 3 拍以上，实际 {}",
        mock.heartbeats()
    );

    let _ = stop_tx.send(());
    let _ = task.await;
    mock.stop();
}

#[tokio::test]
async fn 中台不在线时会一直重试注册() {
    // 起一个 mock 拿到地址后立刻停掉——插件会连不上
    let dead = start_mock(MockConfig::with_interval(1)).await;
    let url = dead.url();
    drop(dead); // 端口随即释放

    let config = Config {
        hub_addr: url,
        advertise_addr: PLUGIN_ADVERTISE.to_string(),
        listen_addr: PLUGIN_LISTEN_ANY.to_string(),
        instance_id: "it-retry".to_string(),
        logger: quiet(),
        register_retry_interval: Duration::from_millis(50),
        heartbeat_fallback_interval: Duration::from_millis(100),
        // 其余取缺省：这些用例与 HubState 无关，状态调用超时用缺省值即可
        ..Default::default()
    };

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let mut task = tokio::spawn(hubkit::run_with_shutdown(DemoPlugin, config, async move {
        let _ = stop_rx.await;
    }));

    // 关键行为：中台不在线**不该让插件起不来**，进程要活着等中台
    tokio::time::sleep(Duration::from_millis(400)).await;
    if task.is_finished() {
        // 提前退出时把真实原因带进 panic（bind 失败只随返回值走，不打日志）
        let result = tokio::time::timeout(Duration::from_millis(100), &mut task).await;
        panic!("中台不在线时插件退出了，它该一直重试；退出结果 {result:?}");
    }

    // 让它干净退出（此时注册从未成功过，没有凭证，注销整个跳过——见 tests/unregister.rs）
    stop_tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("插件应当能被停掉")
        .expect("不应 panic");
    assert!(
        result.is_ok(),
        "注销这一步（发不出去、被中台拒）不该让退出变成错误：{result:?}"
    );
}

#[tokio::test]
async fn 中台拒绝注册时会把每条原因打出来并继续重试() {
    use hubkit::proto::{RejectCode, Rejection};

    let mut mock = start_mock(MockConfig {
        rejections: vec![Rejection {
            code: RejectCode::Unreachable as i32,
            message: "插件地址不可达".to_string(),
            detail: "请确认 HUB_ADVERTISE_ADDR 填的是中台视角下可达的地址".to_string(),
        }],
        ..Default::default()
    })
    .await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        config_for(&mock, "it-rejected"),
        async move {
            let _ = stop_rx.await;
        },
    ));

    // 被拒之后要**继续重试**，而不是放弃
    assert!(
        mock.wait_for_registrations(3, Duration::from_secs(5)).await,
        "被拒后应当持续重试，实际只收到 {} 次",
        mock.registration_count()
    );

    let _ = stop_tx.send(());
    let _ = task.await;
    mock.stop();
}

#[tokio::test]
async fn 必填配置缺失时立刻返回而不是空转() {
    let config = Config {
        hub_addr: String::new(),
        advertise_addr: String::new(),
        ..Default::default()
    };
    let err = hubkit::run_with_shutdown(DemoPlugin, config, async {})
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("HUB_ADDR"), "{text}");
    assert!(text.contains("HUB_ADVERTISE_ADDR"), "{text}");
}

#[tokio::test]
async fn 端口被占时报出是哪个地址() {
    let mock = start_mock(MockConfig::with_interval(1)).await;
    // 先占住一个端口，再让插件去监听它。这条用例**要的就是**「预挑的端口已被占」，
    // 与 PLUGIN_LISTEN_ANY 的免竞态设计相反，所以这里自己拿一个真实端口当 listen_addr
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = squatter.local_addr().unwrap().port();

    let config = Config {
        listen_addr: format!("127.0.0.1:{port}"),
        ..config_for(&mock, "it-busy")
    };
    let err = hubkit::run_with_shutdown(DemoPlugin, config, async {})
        .await
        .unwrap_err();
    assert!(err.to_string().contains(&port.to_string()), "{err}");
}
