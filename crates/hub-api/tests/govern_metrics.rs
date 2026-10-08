//! 治理的打点口径验证（独立验证 agent 起头，修复后改写）。
//!
//! 用一个真实 Prometheus recorder 把 `hub_govern_*` 渲染出来看两件事：
//!
//! 1. `hub_govern_inflight` 的加减是否配平（有没有可能递减多于递增）；
//! 2. 熔断相关的指标**口径是否成立**。
//!
//! 第 2 点是本文件存在的主要理由。原先有一个 `hub_govern_breaker_open{plugin}` 的
//! gauge，但熔断状态是**按实例**的——同一个插件下 instA 还在熔断、instB 已经恢复时，
//! 这个 gauge 会被 instB 置 0，看板显示「一切正常」而实际上有实例在拒流。这就是
//! 「指标口径与状态口径不一致」的典型后果：它不会报错，只会让人看错。
//!
//! 修复方向选了**换掉指标**而不是给 gauge 加 `instance_id` 标签：实例 id 每次进程重启
//! 都会变，在 Prometheus 里是一串永远不再更新、也永远不会消失的时间线，基数只增不减。
//! 现在用两个单调计数器（跳闸 / 恢复），配合已有的
//! `hub_govern_rejected_total{plugin,reason}`：「此刻有几个实例开着」由
//! `tripped - recovered` 推出，标签基数只随插件数增长。
//!
//! 而「此刻到底哪些实例在熔断」这个问题，正确的来源是治理器自身而不是指标——
//! M4 的治理面板会直接读它。
//!
//! 注意：recorder 是**进程级**的，同一个测试二进制里的用例共享它，且用默认并发跑。
//! 所以每个用例用**自己的插件名**打点，断言也按插件名取值——否则用例之间会互相看见
//! 对方的计数，失败变成偶发。

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use hub_engine::{GovernError, Governor, GovernorConfig};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// 进程级只装一次的 recorder。第二次 `install_recorder` 会直接报错。
fn recorder() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("安装 prometheus recorder")
    })
}

/// 某个插件在这条指标上的当前值。找不到就算 0。
fn value(name: &str, plugin: &str) -> f64 {
    let text = recorder().render();
    let needle = format!("plugin=\"{plugin}\"");
    text.lines()
        .find(|l| l.starts_with(name) && !l.starts_with('#') && l.contains(&needle))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// 某个插件在这条指标上是否出现过（用于「不该出现」这类断言）。
fn appears(name: &str, plugin: &str) -> bool {
    let text = recorder().render();
    let needle = format!("plugin=\"{plugin}\"");
    text.lines()
        .any(|l| l.starts_with(name) && !l.starts_with('#') && l.contains(&needle))
}

fn gov(max_concurrency: usize, threshold: u32, cooldown_ms: u64) -> Governor {
    Governor::new(GovernorConfig {
        max_concurrency,
        queue_timeout: Duration::from_millis(20),
        failure_threshold: threshold,
        open_cooldown: Duration::from_millis(cooldown_ms),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inflight_加减配平() {
    let _ = recorder(); // 必须先装 recorder，晚装会丢掉之前打过的点
    let plugin = "metrics-inflight";
    let g = gov(8, 1, 50);

    let a = g.acquire(plugin, "instA", None).await.expect("a");
    let b = g.acquire(plugin, "instB", None).await.expect("b");
    let c = g.acquire(plugin, "instC", None).await.expect("c");
    assert_eq!(value("hub_govern_inflight", plugin), 3.0);

    a.record(true);
    b.record(false); // 跳闸（阈值 1）
    drop(c); // 取消：不记录

    assert_eq!(
        value("hub_govern_inflight", plugin),
        0.0,
        "inflight 必须回到 0，不能多减也不能少减"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 熔断指标能推出此刻开着几个() {
    let _ = recorder();
    let plugin = "metrics-breaker";
    let g = gov(8, 1, 50);

    // instA 先在 t=0 跳闸
    let a = g.acquire(plugin, "instA", None).await.expect("a");
    a.record(false);
    assert_eq!(value("hub_govern_breaker_tripped_total", plugin), 1.0);
    assert_eq!(value("hub_govern_breaker_recovered_total", plugin), 0.0);

    // 冷却 50ms 后 instA 探测成功 → 恢复
    tokio::time::sleep(Duration::from_millis(70)).await;
    let probe = g.acquire(plugin, "instA", None).await.expect("应放探测");
    probe.record(true);
    assert_eq!(value("hub_govern_breaker_recovered_total", plugin), 1.0);
    assert_eq!(
        value("hub_govern_breaker_tripped_total", plugin)
            - value("hub_govern_breaker_recovered_total", plugin),
        0.0,
        "两个计数器之差就是「此刻有几个实例开着」"
    );

    // 换另一个实例跳闸：差值变成 1，看板不会说「一切正常」
    let b = g.acquire(plugin, "instB", None).await.expect("b");
    b.record(false);
    assert_eq!(
        value("hub_govern_breaker_tripped_total", plugin)
            - value("hub_govern_breaker_recovered_total", plugin),
        1.0,
    );

    // 同一时刻真实状态：instB 被拦下
    let blocked = g.acquire(plugin, "instB", Some(Duration::ZERO)).await;
    assert!(
        matches!(blocked, Err(GovernError::CircuitOpen { .. })),
        "前提：instB 冷却未结束，应被拦下；实际 {blocked:?}"
    );

    // 而且「被拦下」这件事本身也有计数，不必靠推断
    assert!(
        value("hub_govern_rejected_total", plugin) > 0.0,
        "熔断拦下的次数要单独计数"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 背压与熔断在指标上分开表达() {
    let _ = recorder();
    let plugin = "metrics-backpressure";
    let g = Governor::new(GovernorConfig {
        max_concurrency: 1,
        queue_timeout: Duration::ZERO,
        failure_threshold: 99,
        open_cooldown: Duration::from_millis(50),
    });

    let _held = g.acquire(plugin, "i1", None).await.expect("第一个");
    let err = g
        .acquire(plugin, "i1", None)
        .await
        .expect_err("第二个应背压");
    assert!(matches!(err, GovernError::Overloaded { .. }), "{err}");

    // 背压与熔断是两种完全不同的处置（429 vs 503），指标上必须分得开
    assert!(
        value("hub_govern_rejected_total", plugin) > 0.0,
        "背压该有计数"
    );
    assert!(
        !appears("hub_govern_breaker_tripped_total", plugin),
        "这次没有跳闸，不该出现熔断的计数"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 已跳闸的实例不会被_prune_清掉() {
    let g = gov(4, 1, 60_000);

    let a = g.acquire("p", "tripped", None).await.expect("应拿到许可");
    a.record(false);
    drop(g.acquire("p", "idle", None).await);

    let alive: HashSet<String> = HashSet::new();
    assert_eq!(g.prune(&alive), 1, "只该清掉空闲的那个");
    assert_eq!(g.tracked_instances(), 1);

    // 冷却长达 60 秒：清掉它会把它刚攒下的失败计数凭空归零
    let blocked = g.acquire("p", "tripped", Some(Duration::ZERO)).await;
    assert!(matches!(blocked, Err(GovernError::CircuitOpen { .. })));
}
