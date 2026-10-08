//! **外置状态（HubState）**——凭证怎么到手、请求怎么带凭证、401 怎么自愈。
//!
//! 断言分两层，缺一不可：
//!   1. **插件侧看到了什么**（返回值对不对）；
//!   2. **中台侧收到了什么**（`MockHub` 记下来的凭证与存下来的值）。
//!
//! 只验第 1 层的话，「凭证其实没发出去、只是 mock 恰好也放行了」这种错会漏过去。
//!
//! 这里的伪造中台把注册面与状态面挂在**同一个端口**上（与真中台一样），
//! 所以走的正是生产的那条路：注册拿凭证 → 状态请求带 `x-hub-state-token`。

mod support;

use std::time::Duration;

use hubkit::{Config, StateError, STATE_TOKEN_METADATA};
use support::*;

/// 状态调用要用的命名空间。规则要求 `[A-Za-z0-9_.-]`，不能有冒号。
const NS: &str = "session";

fn config_for(mock: &MockHub, instance: &str) -> Config {
    Config {
        hub_addr: mock.url(),
        advertise_addr: PLUGIN_ADVERTISE.to_string(),
        listen_addr: PLUGIN_LISTEN_ANY.to_string(),
        instance_id: instance.to_string(),
        logger: quiet(),
        register_retry_interval: Duration::from_millis(50),
        heartbeat_fallback_interval: Duration::from_millis(100),
        state_call_timeout: Duration::from_secs(2),
        gateway_call_timeout: Duration::from_secs(2),
    }
}

/// 起一个「需要用状态」的插件，返回插件句柄（测试从它拿注入进来的客户端）。
struct Running {
    plugin: StatePlugin,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), hubkit::HubkitError>>,
}

async fn start_state_plugin(mock: &MockHub, name: &str, instance: &str) -> Running {
    let plugin = StatePlugin::named(name);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let task = tokio::spawn(hubkit::run_with_shutdown(
        // run 要吃走插件本体，测试这边留一份克隆读结果（内部共享同一份状态）
        plugin.clone(),
        config_for(mock, instance),
        async move {
            let _ = stop_rx.await;
        },
    ));

    Running {
        plugin,
        stop: stop_tx,
        task,
    }
}

#[tokio::test]
async fn 注册成功后凭证被注入_且读写删扫都带上了它() {
    let mock = start_mock(MockConfig::with_interval(1)).await;
    let running = start_state_plugin(&mock, "state-plugin", "it-state-1").await;

    // ---- 注入：插件作者不该自己管凭证，这一步由骨架完成 ----
    let state = running.plugin.wait_for_state(Duration::from_secs(5)).await;
    assert!(
        !mock.state_token().is_empty(),
        "伪造中台应当在注册回执里下发凭证"
    );

    // ---- Put → Get 往返 ----
    state
        .put(NS, "alice", b"token-1", Duration::ZERO)
        .await
        .expect("写状态应当成功");
    assert_eq!(
        state.get(NS, "alice").await.expect("读状态应当成功"),
        Some(b"token-1".to_vec()),
        "读回来的必须就是写进去的那份"
    );
    // 中台那边确实存下了（只验插件侧的话，「其实没发出去」也会过）
    assert_eq!(
        mock.state_value(NS, "alice"),
        Some(b"token-1".to_vec()),
        "中台侧应当存下了这个键"
    );

    // 「键不存在」与「值是空字节」是两回事，两侧都要分得清
    assert_eq!(state.get(NS, "nobody").await.unwrap(), None);
    state
        .put(NS, "empty", b"", Duration::ZERO)
        .await
        .expect("空值也要能写");
    assert_eq!(
        state.get(NS, "empty").await.unwrap(),
        Some(Vec::new()),
        "空值读回来是 Some(空)，不是 None"
    );

    // ---- Scan：前缀 + limit ----
    state
        .put(NS, "bob", b"token-2", Duration::ZERO)
        .await
        .unwrap();
    state
        .put("other", "carol", b"x", Duration::ZERO)
        .await
        .unwrap();

    let mut all: Vec<String> = state
        .scan(NS, "", 100)
        .await
        .expect("扫描应当成功")
        .into_iter()
        .map(|e| e.key)
        .collect();
    all.sort();
    assert_eq!(all, vec!["alice", "bob", "empty"], "扫描只该看到本命名空间");

    let prefix: Vec<String> = state
        .scan(NS, "bo", 100)
        .await
        .expect("带前缀的扫描应当成功")
        .into_iter()
        .map(|e| e.key)
        .collect();
    assert_eq!(prefix, vec!["bob"]);

    assert_eq!(
        state.scan(NS, "", 1).await.unwrap().len(),
        1,
        "limit 应当被中台（这里是 mock）照用"
    );

    // ---- Delete ----
    assert!(state.delete(NS, "alice").await.expect("删除应当成功"));
    assert_eq!(state.get(NS, "alice").await.unwrap(), None);
    assert!(
        !state
            .delete(NS, "alice")
            .await
            .expect("删不存在的键不是错误"),
        "删不存在的键返回 false"
    );

    // ---- 中台侧收到的凭证 ----
    let seen = mock.state_seen_tokens();
    assert!(!seen.is_empty(), "状态请求必须带上凭证");
    let expected = mock.state_token();
    assert!(
        seen.iter().all(|t| t == &expected),
        "每一次状态请求带的都该是中台刚下发的那张 {expected:?}，实际收到 {seen:?}"
    );
    // 键名写错的话上面那条会看到空串——这里再显式钉一次契约里的键名
    assert_eq!(STATE_TOKEN_METADATA, "x-hub-state-token");

    running.stop.send(()).ok();
    running.task.await.expect("插件应当正常退出").expect("退出");
}

#[tokio::test]
async fn 状态调用撞上_401_会触发重新注册并换成新凭证() {
    let mock = start_mock(MockConfig::with_interval(1)).await;
    let running = start_state_plugin(&mock, "state-plugin", "it-state-401").await;

    let state = running.plugin.wait_for_state(Duration::from_secs(5)).await;
    let first_token = mock.state_token();

    state
        .put(NS, "k", b"v", Duration::ZERO)
        .await
        .expect("第一次写应当成功");

    // 走完冷却窗口（心跳循环对 denial 的速率下限），再吊销凭证。
    // 不睡的话这次 401 会落在窗口内被丢掉——那是另一条测试要验的行为。
    tokio::time::sleep(Duration::from_millis(200)).await;
    mock.revoke_state();

    // 这一次必须**失败**：自愈是后台的事，不该把这次失败吞掉当成成功
    let err = state
        .get(NS, "k")
        .await
        .expect_err("凭证被吊销后读应当失败");
    assert!(
        err.is_unauthenticated(),
        "应当是 401 语义的错误，实际 {err:?}"
    );

    // ---- 骨架应当已经因此重新注册 ----
    assert!(
        mock.wait_for_registrations(2, Duration::from_secs(5)).await,
        "{}",
        msg("401 之后的第二次注册")
    );
    let second_token = mock.state_token();
    assert_ne!(
        first_token, second_token,
        "重新注册应当换一张新凭证（与真中台一致）"
    );
    // 每次注册后都注入一次：插件完全可以借这个时机清理自己那份缓存
    assert!(
        wait_until(Duration::from_secs(5), || running.plugin.injections() >= 2).await,
        "{}",
        msg("重新注册后的再次注入")
    );

    // ---- 换到新凭证之后，同一个客户端继续可用 ----
    state
        .put(NS, "k", b"v2", Duration::ZERO)
        .await
        .expect("重新注册后写应当恢复");
    assert_eq!(state.get(NS, "k").await.unwrap(), Some(b"v2".to_vec()));
    let seen = mock.state_seen_tokens();
    assert_eq!(
        seen.last().unwrap(),
        &second_token,
        "恢复后的请求带的必须是新凭证"
    );

    running.stop.send(()).ok();
    running.task.await.expect("插件应当正常退出").expect("退出");
}

#[tokio::test]
async fn 冷却窗口内的_401_不会把注册打成自旋() {
    // 冷却窗口设成 2s：窗口内到达的 denial 必须被丢掉，而不是让注册循环空转
    let mut mock = start_mock(MockConfig::with_interval(1)).await;
    let plugin = StatePlugin::named("state-plugin");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let task = tokio::spawn(hubkit::run_with_shutdown(
        plugin.clone(),
        Config {
            register_retry_interval: Duration::from_secs(2),
            ..config_for(&mock, "it-state-cooldown")
        },
        async move {
            let _ = stop_rx.await;
        },
    ));

    let state = plugin.wait_for_state(Duration::from_secs(5)).await;
    mock.revoke_state();

    // 窗口内连着撞 401：每一次都该如实失败，但都不该触发重注册
    let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
    let mut denials = 0;
    while tokio::time::Instant::now() < deadline {
        let err = state.get(NS, "k").await.expect_err("应当失败");
        assert!(err.is_unauthenticated(), "{err:?}");
        denials += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(denials >= 3, "这段窗口里应当撞上不止一次 401");

    assert_eq!(
        mock.registration_count(),
        1,
        "冷却窗口内的 401 不该触发重新注册——否则持续被拒时注册会打成自旋"
    );

    stop_tx.send(()).ok();
    task.await.expect("插件应当正常退出").expect("退出");
    mock.stop();
}

#[tokio::test]
async fn 不用状态的插件完全不受影响() {
    // 老插件一行不改也要能跑：默认实现是空钩子，`run` 不做任何额外要求。
    // 这条测试的价值在于「加了钩子之后 DemoPlugin 还能注册、还能被调用」。
    let mut mock = start_mock(MockConfig::with_interval(1)).await;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let task = tokio::spawn(hubkit::run_with_shutdown(
        DemoPlugin,
        config_for(&mock, "it-state-plain"),
        async move {
            let _ = stop_rx.await;
        },
    ));

    assert!(
        mock.wait_for_registrations(1, Duration::from_secs(5)).await,
        "{}",
        msg("不实现 set_state 的插件照样注册")
    );

    // 状态面上一次调用都不该发生
    assert_eq!(mock.state_calls(), 0, "没用到状态的插件不该碰状态面");

    stop_tx.send(()).ok();
    task.await.expect("插件应当正常退出").expect("退出");
    mock.stop();
}

#[tokio::test]
async fn 中台没下发凭证时_插件照常起_调用如实报错() {
    // 中台可能还没配状态面。这时插件不该起不来——它只是用不了状态，
    // 用的时候会拿到 401，而那是自愈路径的入口（骨架会去重注册要一张新的）。
    let mock = start_mock(MockConfig {
        heartbeat_interval_seconds: 1,
        omit_state_token: true,
        ..Default::default()
    })
    .await;

    let running = start_state_plugin(&mock, "state-plugin", "it-state-notoken").await;
    assert!(
        mock.wait_for_registrations(1, Duration::from_secs(5)).await,
        "{}",
        msg("注册")
    );
    // 注入照常发生：客户端本身是可用的，缺的只是凭证
    let state = running.plugin.wait_for_state(Duration::from_secs(5)).await;

    let err = state.get(NS, "k").await.expect_err("没有凭证时应当失败");
    assert!(err.is_unauthenticated(), "{err:?}");

    // 刻意**不**断言「一定会重新注册」：中台会不会在下次回执里补一张凭证
    // 是它的实现细节（mock 每注册一次都会重新下发），把它写进断言等于把
    // mock 的行为当成契约。这里只验「插件活着、错误如实上报」。

    running.stop.send(()).ok();
    running.task.await.expect("插件应当正常退出").expect("退出");
}

#[tokio::test]
async fn 本地拦下的错误好懂且不产生网络往返() {
    let mut mock = start_mock(MockConfig::with_interval(1)).await;
    let running = start_state_plugin(&mock, "state-plugin", "it-state-local").await;
    let state = running.plugin.wait_for_state(Duration::from_secs(5)).await;

    let before = mock.state_calls();

    // 键名规则：与中台共用一份判定（`hub-rules.json`），但错误里把「200 字节」也说了
    let err = state.get("bad:ns", "k").await.expect_err("应当本地被拦");
    assert!(matches!(err, StateError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("200 字节"), "{err}");

    // 单值上限 1 MiB：本地先拦，不必等中台那句 INVALID_ARGUMENT
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

    // 扫描上限 1000：中台收 0 或超限都拒，本地能说得更直接
    for limit in [0, hubkit::MAX_SCAN_LIMIT + 1] {
        let err = state
            .scan(NS, "", limit)
            .await
            .expect_err("limit 越界应当本地被拦");
        assert!(err.to_string().contains("1..=1000"), "{err}");
    }
    let err = state
        .scan(NS, "a:b", 10)
        .await
        .expect_err("前缀非法应当本地被拦");
    assert!(matches!(err, StateError::Invalid(_)), "{err:?}");

    // 关键断言：上面这些一次都没走到中台
    assert_eq!(
        mock.state_calls(),
        before,
        "本地就能判定的错误不该产生任何网络往返"
    );

    running.stop.send(()).ok();
    running.task.await.expect("插件应当正常退出").expect("退出");
    mock.stop();
}
