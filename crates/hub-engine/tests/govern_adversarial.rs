//! 实例级治理的对抗性测试（独立验证 agent 编写，不属于原作者）。
//!
//! 全部只用 `hub-engine` 的**公开 API**：`Governor` / `GovernorConfig` / `GovernError` / `Permit`。
//! 目的不是复跑作者已有的测试，而是从「取消 / 并发 / 边界 / 竞态」这些角度找它的破绽。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hub_engine::Permit;
use hub_engine::govern::{GovernError, Governor, GovernorConfig};

/// 起一个治理器。
fn gov(
    max_concurrency: usize,
    queue_timeout_ms: u64,
    threshold: u32,
    cooldown_ms: u64,
) -> Governor {
    Governor::new(GovernorConfig {
        max_concurrency,
        queue_timeout: Duration::from_millis(queue_timeout_ms),
        failure_threshold: threshold,
        open_cooldown: Duration::from_millis(cooldown_ms),
    })
}

/// 把某个实例打到跳闸（阈值必须为 1，或循环失败够阈值次）。
async fn trip(g: &Governor, plugin: &str, instance: &str, failures: u32) {
    for _ in 0..failures {
        let permit = g
            .acquire(plugin, instance, None)
            .await
            .expect("跳闸前应能拿到许可");
        permit.record(false);
    }
}

// ---------------------------------------------------------------------------
// 1. 回归防守：acquire 在「等并发名额」时被取消（调用超时 / 客户端断开）
// ---------------------------------------------------------------------------

/// 【回归防守】`acquire` 在 `admit` 里把状态切成 `Probing` 之后才去 await 信号量。
/// 如果这个 await 被取消（外层 timeout / 客户端断开），future 直接被丢掉，**后续一行
/// 代码都不会执行**——只有 `Drop` 还会跑。
///
/// **曾是缺陷，现已修复**：起初没有任何东西把 `Probing` 退回去，实例卡在这个状态，
/// 期间所有调用都被误报成 `CircuitOpen`（503）而不是 `Overloaded`（429），也不再尝试
/// 探测，直到别的在途许可被释放才顺带自愈。现在由 `ProbeGuard` 承担——它是一个跨越
/// await 的局部变量，future 被丢弃时它随之析构，把名额退回「立即可再探测」。
///
/// 本用例守着它不再退化。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 探测等待期间被取消不会泄漏半开名额() {
    let g = gov(2, 5_000, 1, 40); // 名额 2、排队 5s、阈值 1、冷却 40ms

    // 两个正常调用占满名额
    let h1 = g.acquire("p", "i1", None).await.expect("h1");
    let h2 = g.acquire("p", "i1", None).await.expect("h2");

    // 第三个调用已经在排队（它通过熔断判定时熔断还是 Closed）
    let g2 = g.clone();
    let waiter = tokio::spawn(async move { g2.acquire("p", "i1", None).await });
    tokio::time::sleep(Duration::from_millis(30)).await;

    // h2 回报失败 → 阈值 1 立刻跳闸，同时把名额让给排队者
    h2.record(false);
    let w = waiter
        .await
        .expect("排队任务不该 panic")
        .expect("排队者应拿到刚释放的名额");
    // 名额被 h1 / w 占满，熔断处于 Open（40ms 冷却）
    assert!(
        matches!(
            g.acquire("p", "i1", Some(Duration::ZERO)).await,
            Err(GovernError::CircuitOpen { .. })
        ),
        "此刻应处于跳闸中"
    );

    tokio::time::sleep(Duration::from_millis(100)).await; // 冷却结束

    // 探测者：进入 Probing 后卡在并发名额上（名额被占满），被外层 100ms 超时取消
    let cancelled =
        tokio::time::timeout(Duration::from_millis(100), g.acquire("p", "i1", None)).await;
    assert!(cancelled.is_err(), "这次 acquire 应被外层超时取消");

    // 判别调用（budget=0，不会真的排队）：
    //   - 状态若正确退回 Open{until: now} → 立刻转 Probing → 名额满 → Overloaded
    //   - 状态若泄漏成 Probing（取消者留下的）→ CircuitOpen
    let probe_d = g.acquire("p", "i1", Some(Duration::ZERO)).await;
    println!("【取消后：判别调用】{probe_d:?}");
    println!(
        "【取消后：探测名额是否泄漏】{}",
        matches!(probe_d, Err(GovernError::CircuitOpen { .. }))
    );
    assert!(
        matches!(probe_d, Err(GovernError::Overloaded { .. })),
        "本断言写的是**正确行为**：取消后状态应退回「立刻可再探测」，名额满即报 Overloaded。\
         实际报 CircuitOpen（且 retry_after 是冷却常量）就说明状态仍停在 Probing；实际 {probe_d:?}"
    );

    // 自愈路径：别的在途许可一释放，泄漏的 Probing 就被顺带退回去
    drop(h1);
    let heal = g.acquire("p", "i1", Some(Duration::ZERO)).await;
    println!("【别的许可释放之后】{:?}", heal.as_ref().map(|_| "Ok"));
    assert!(
        heal.is_ok(),
        "泄漏会被别的许可释放顺带治愈（这也是为什么它没造成永久故障）：{:?}",
        heal.err()
    );
    if let Ok(p) = heal {
        p.record(true);
    }
    drop(w);
}

/// 对照：同样场景，但探测那次**没有被取消**（让它拿到名额），实例应能自行恢复。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 对照_探测未被取消时冷却结束后能恢复() {
    let g = gov(2, 5_000, 1, 40);

    let h1 = g.acquire("p", "i1", None).await.expect("h1");
    let h2 = g.acquire("p", "i1", None).await.expect("h2");
    let g2 = g.clone();
    let waiter = tokio::spawn(async move { g2.acquire("p", "i1", None).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    h2.record(false);
    let w = waiter.await.expect("no panic").expect("排队者应拿到名额");

    drop(h1);
    drop(w);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 探测没被取消：拿到许可并回报成功
    let probe = g.acquire("p", "i1", None).await.expect("冷却后应放探测");
    probe.record(true);

    g.acquire("p", "i1", None)
        .await
        .expect("探测成功后应完全恢复")
        .record(true);
}

// ---------------------------------------------------------------------------
// 2. 回归防守：半开探测的名额不该被「无关调用」的排队超时清掉
// ---------------------------------------------------------------------------

/// 【回归防守】超时分支里的「把 Probing 退回去」曾经是**无条件**的：它不检查这个
/// Probing 是不是自己造成的。一个早就过了熔断判定、正在排队等名额的调用一旦排队超时，
/// 就会把**别人**正在飞的探测名额抹掉，随后新的调用又能被当成探测放行——半开窗口里
/// 于是放出了第二个探测。
///
/// **曾是真缺陷，现已修复**：现在只有探测名额的持有者（`probe: Some(_)`）才有资格
/// 退回它。前提是冷却时间 < 排队上限（默认 10s vs 100ms 撞不上，但 config 明确允许
/// `BREAKER_COOLDOWN_SECS=0` 这类激进档位，本用例就构造了这个组合）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 无关调用排队超时不会抹掉别人的探测名额() {
    // 关键：冷却(30ms) 远小于排队上限(500ms)，排队者能活到冷却结束之后
    let g = gov(2, 500, 1, 30);

    let h1 = g.acquire("p", "i1", None).await.expect("h1");
    let h2 = g.acquire("p", "i1", None).await.expect("h2");

    // 两个排队者：都在熔断还是 Closed 时通过了判定；预算 200ms 让它们早于探测者超时
    let budget = Some(Duration::from_millis(200));
    let g2 = g.clone();
    let w1 = tokio::spawn(async move { g2.acquire("p", "i1", budget).await });
    let g3 = g.clone();
    let w2 = tokio::spawn(async move { g3.acquire("p", "i1", budget).await });
    tokio::time::sleep(Duration::from_millis(30)).await;

    // 跳闸：释放出来的名额给到其中一个排队者，另一个继续排（不假设是哪一个）
    h2.record(false);
    tokio::time::sleep(Duration::from_millis(80)).await; // 冷却 30ms 已过
    let mut holders = Vec::new();
    if w1.is_finished()
        && let Ok(Ok(p)) = w1.await
    {
        holders.push(p);
    }
    if w2.is_finished()
        && let Ok(Ok(p)) = w2.await
    {
        holders.push(p);
    }
    println!("【跳闸时抢到名额的排队者数】{}", holders.len());
    assert_eq!(holders.len(), 1, "应只有一个排队者拿到名额，另一个还在排");

    // 探测者 A：排队上限 500ms（比排队者的 200ms 长），会一直卡到判定之后
    let ga = g.clone();
    let probe_a = tokio::spawn(async move { ga.acquire("p", "i1", None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!probe_a.is_finished(), "A 此时应还卡在并发名额上");

    // 剩下的那个排队者在 t≈200ms 超时（A 是 t≈110ms 起算，要 t≈610ms 才超时）
    tokio::time::sleep(Duration::from_millis(180)).await;

    // 判别调用（budget=0，不排队）：
    //   - 若 A 的探测还在（Probing）→ CircuitOpen（正确）
    //   - 若被抹成 Open{until: now} → 立刻又能转 Probing → 名额满、排队 0ms → Overloaded
    let d = g.acquire("p", "i1", Some(Duration::ZERO)).await;
    println!("【A 是否还在等名额】{}", !probe_a.is_finished());
    println!("【判别调用 D 的结果】{d:?}");

    assert!(
        matches!(d, Err(GovernError::CircuitOpen { .. })),
        "A 的探测还在飞，D 不该再被当作探测放行；实际 {d:?}"
    );

    probe_a.abort();
    let _ = h1;
    drop(holders);
}

// ---------------------------------------------------------------------------
// 3. 半开窗口：真实并发下是否只放一个探测过去
// ---------------------------------------------------------------------------

/// 作者的同名测试是**顺序**调用的。这里用真实并发（多线程运行时 + 同时起跑）验证。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn 半开窗口_真实并发下只放一个探测() {
    const N: usize = 64;
    let g = gov(8, 100, 1, 30);
    trip(&g, "p", "i1", 1).await;
    tokio::time::sleep(Duration::from_millis(60)).await; // 冷却结束

    let go = Arc::new(AtomicBool::new(false));
    let mut tasks = Vec::new();
    for _ in 0..N {
        let g = g.clone();
        let go = Arc::clone(&go);
        tasks.push(tokio::spawn(async move {
            while !go.load(Ordering::Relaxed) {
                tokio::task::yield_now().await;
            }
            g.acquire("p", "i1", None).await.map(|p| {
                std::mem::forget(p); // 探测不许回报，否则会立刻关闸
            })
        }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    go.store(true, Ordering::Relaxed);

    let mut admitted = 0;
    let mut rejected = 0;
    for t in tasks {
        match t.await.expect("任务不该 panic") {
            Ok(()) => admitted += 1,
            Err(_) => rejected += 1,
        }
    }
    println!("【并发 {N} 个探测：放行 {admitted}，拒绝 {rejected}】");
    assert_eq!(admitted, 1, "半开只能放一个探测，实际放行了 {admitted} 个");
    assert_eq!(rejected, N - 1);
}

// ---------------------------------------------------------------------------
// 4. 高并发抢名额是否超发
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn 高并发抢名额不超发() {
    const MAX: usize = 4;
    let g = gov(MAX, 50, 99, 1_000);
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let ok = Arc::new(AtomicUsize::new(0));

    let mut tasks = Vec::new();
    for _ in 0..400 {
        let g = g.clone();
        let inflight = Arc::clone(&inflight);
        let peak = Arc::clone(&peak);
        let ok = Arc::clone(&ok);
        tasks.push(tokio::spawn(async move {
            if let Ok(permit) = g.acquire("p", "i1", None).await {
                ok.fetch_add(1, Ordering::Relaxed);
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(3)).await;
                inflight.fetch_sub(1, Ordering::SeqCst);
                permit.record(true);
            }
        }));
    }
    for t in tasks {
        t.await.expect("任务不该 panic");
    }
    println!(
        "【拿到许可 {} 次，同时在途峰值 {}，上限 {MAX}】",
        ok.load(Ordering::Relaxed),
        peak.load(Ordering::SeqCst)
    );
    assert!(
        peak.load(Ordering::SeqCst) <= MAX,
        "同时在途数不该超过上限：峰值 {} > {MAX}",
        peak.load(Ordering::SeqCst)
    );
}

/// 【回归防守】`prune` 的清理条件曾是「熔断已关 + 所有许可都空着」，而 `acquire` 是
/// 「先取 cell（拿 Arc），后拿许可」——取到 cell 到拿到许可之间有一个同步窗口，此刻
/// 许可还没被占，prune 会把这条表项删掉，调用方继续用这条**被摘掉的** cell 发许可，
/// 新调用方则新建一条。同一个实例于是有了两套信号量与两套失败计数：**并发上限翻倍，
/// 而熔断的失败计数被分裂、永远攒不够阈值**。
///
/// **曾是真缺陷，现已修复**：`Cell` 增加了 `checked_out` 计数，在 `cells` 锁内与
/// 「取到 cell」一起完成，`prune` 持同一把锁读它。
///
/// 判据（无需猜测）：持有许可期间 `tracked_instances() == 0` 只可能意味着
/// 「我手上这张许可来自一条已经不在治理表里的表项」——因为持有许可时该表项不可能被清。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn prune_与取许可并发时名额不会超发() {
    const MAX: usize = 1;
    let g = gov(MAX, 5, 99, 1_000);
    let alive: HashSet<String> = HashSet::new(); // 实例不在该集合里 → 可被清
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let orphan = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let pruner = {
        let g = g.clone();
        let alive = alive.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                n += g.prune(&alive) as u64;
            }
            n
        })
    };

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let g = g.clone();
        let inflight = Arc::clone(&inflight);
        let peak = Arc::clone(&peak);
        let attempts = Arc::clone(&attempts);
        let orphan = Arc::clone(&orphan);
        let stop = Arc::clone(&stop);
        tasks.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                attempts.fetch_add(1, Ordering::Relaxed);
                if let Ok(permit) = g.acquire("p", "i1", None).await {
                    // 持有许可时表项却不在表里 → 这张许可来自被 prune 摘掉的表项
                    if g.tracked_instances() == 0 {
                        orphan.fetch_add(1, Ordering::SeqCst);
                    }
                    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_micros(50)).await;
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    permit.record(true);
                }
                // 让名额空出来，好让 prune 有机会清理这条表项
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }));
    }

    tokio::time::sleep(Duration::from_secs(8)).await;
    stop.store(true, Ordering::Relaxed);
    for t in tasks {
        t.await.expect("任务不该 panic");
    }
    let pruned = pruner.join().expect("prune 线程不该 panic");
    println!(
        "【8 秒内：取许可 {} 次；prune 清理 {} 次；同时在途峰值 {}（上限 {MAX}）；\
         观察到「持有许可但表项已被清」{} 次】",
        attempts.load(Ordering::Relaxed),
        pruned,
        peak.load(Ordering::SeqCst),
        orphan.load(Ordering::SeqCst)
    );
    assert!(
        peak.load(Ordering::SeqCst) <= MAX,
        "并发上限被 prune 竞态击穿：峰值 {} > {MAX}",
        peak.load(Ordering::SeqCst)
    );
    assert_eq!(
        orphan.load(Ordering::SeqCst),
        0,
        "同一实例出现了两套并发的治理状态（许可来自被摘掉的表项）"
    );
}

// ---------------------------------------------------------------------------
// 5. 熔断状态被非探测调用「顺带」关闭
// ---------------------------------------------------------------------------

/// 【回归防守】`Permit::record(true)` 曾无条件 `breaker = Closed; consecutive_failures = 0`，
/// 不管这次调用是不是半开探测。一个在跳闸**之前**就起飞、慢吞吞跑完的调用回报成功，
/// 会把刚跳的闸直接关掉、失败计数清零，冷却期形同虚设——而它的成功是**过期消息**：
/// 这段时间里实例确实一直在失败。
///
/// **曾是真缺陷，现已修复**：`record` 现在看 `admit` 的放行方式——只有 `Admission::Probe`
/// 才有资格关闸；跳闸状态下回报的普通成功一律不理会。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 跳闸前起飞的慢调用成功不会关掉熔断() {
    let g = gov(2, 100, 1, 60_000); // 冷却 60 秒：跳闸后理应长时间拒绝

    // 一个在跳闸前就通过判定的在途调用
    let slow = g.acquire("p", "i1", None).await.expect("slow");
    let bad = g.acquire("p", "i1", None).await.expect("bad");
    bad.record(false); // 阈值 1 → 立刻跳闸，冷却 60 秒

    assert!(
        matches!(
            g.acquire("p", "i1", Some(Duration::ZERO)).await,
            Err(GovernError::CircuitOpen { .. })
        ),
        "此时应在冷却中"
    );

    slow.record(true); // 跳闸前起飞的慢调用回来了

    let after = g.acquire("p", "i1", Some(Duration::ZERO)).await;
    println!("【慢调用成功回报之后】{after:?}");
    assert!(
        matches!(after, Err(GovernError::CircuitOpen { .. })),
        "非探测调用的成功不该关掉熔断（冷却还有 60 秒）；实际 {after:?}"
    );
    if let Ok(p) = after {
        p.record(true);
    }
}

// ---------------------------------------------------------------------------
// 6. 取消 / panic：permit 未 record 就消失
// ---------------------------------------------------------------------------

/// 已经拿到 permit 的调用被打断（panic 展开 / 直接 drop / 取消），
/// 应当既不算失败，也能让实例重新可探测。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 拿到许可后未回报_既不算失败也能重新探测() {
    let g = gov(4, 100, 1, 30);
    trip(&g, "p", "i1", 1).await;
    tokio::time::sleep(Duration::from_millis(60)).await;

    // 探测许可在 panic 展开里被丢弃
    let probe = g.acquire("p", "i1", None).await.expect("应放探测");
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _hold = probe;
        panic!("调用被打断");
    }));
    assert!(unwound.is_err(), "应发生 panic 展开");

    // 不能卡在半开，也不能被记成失败（记成失败会重新跳闸 30ms）
    let again = g.acquire("p", "i1", Some(Duration::ZERO)).await;
    println!(
        "【未回报后的下一次取许可】{:?}",
        again.as_ref().map(|_| "Ok")
    );
    assert!(
        again.is_ok(),
        "未回报的探测应立刻退回可探测状态（不计失败），下次调用应能拿到许可；实际 {:?}",
        again.err()
    );
    if let Ok(p) = again {
        p.record(true);
    }
    // 失败计数没有被这次「未回报」污染：记录一次失败后仍不该跳闸（阈值 1 时除外）
    let g2 = gov(4, 20, 2, 30);
    trip(&g2, "p", "i2", 1).await;
    let p = g2.acquire("p", "i2", None).await.expect("应能拿到许可");
    drop(p); // 未回报
    let p = g2.acquire("p", "i2", None).await.expect("应仍能拿到许可");
    println!("【未回报不计失败：第二次取许可成功】");
    p.record(true);
}

// ---------------------------------------------------------------------------
// 7. 代码层（绕过 config 校验）直接构造的边界值
// ---------------------------------------------------------------------------

/// `hub-core` 拦住了环境变量路径上的 `NODE_MAX_CONCURRENCY=0`，但 `Governor::new`
/// 是公开 API（`Invoker::with_governor` 也公开）。直接构造会发生什么？
///
/// **本用例的断言在修复后改过**：原本断言「上限 0 时每次调用都被背压拦下」（当时
/// 属实），这被判定为缺陷——中台会静默地变成对每一个请求都返回 429。现改为断言
/// `Governor::new` 把上限夹到至少 1，中台照常工作。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 直接构造并发上限为零的治理器会被夹到至少一() {
    let g = Governor::new(GovernorConfig {
        max_concurrency: 0,
        queue_timeout: Duration::from_millis(20),
        failure_threshold: 1,
        open_cooldown: Duration::from_millis(10),
    });
    let alive: HashSet<String> = HashSet::new();

    let started = Instant::now();
    let first = g.acquire("p", "i1", None).await;
    println!(
        "【上限 0：取许可结果】{first:?}，耗时 {:?}",
        started.elapsed()
    );
    let first = match first {
        Ok(permit) => permit,
        Err(err) => panic!(
            "上限 0 会让每一次调用都被背压拦下——中台等于死了，而且死得没有任何报错；\
             构造时就该夹到 1；实际 {err}"
        ),
    };

    // 夹到 1 之后，名额仍然真的只有 1 个：第二个就过不去了
    let err = g
        .acquire("p", "i1", None)
        .await
        .expect_err("夹到 1 不等于放开：上限仍然生效");
    assert!(
        matches!(err, GovernError::Overloaded { limit: 1, .. }),
        "{err}"
    );
    drop(first);

    // 空闲且已关闸 → 可清。这里顺带确认「上限被夹过」不会让 prune 把它当成永不清
    let idle = g.acquire("p", "i2", None).await.expect("应拿到许可");
    drop(idle);
    println!("【上限 0 时 prune 清掉】{}", g.prune(&alive));
    let _ = &alive;
}

/// 预算为 0 / 恰好等于 queue_timeout 时，等待时间怎么取。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 预算与排队上限的边界() {
    // 名额被占满 + 排队上限 200ms
    let g = gov(1, 200, 99, 1_000);
    let held = g.acquire("p", "i1", None).await.expect("held");

    // 预算 0：不排队，立刻失败（但**仍然会先过熔断判定**）
    let t0 = Instant::now();
    let r0 = g.acquire("p", "i1", Some(Duration::ZERO)).await;
    let e0 = t0.elapsed();
    println!("【预算 0】{r0:?} 耗时 {e0:?}");

    // 预算恰好等于排队上限：等满 200ms 才失败
    let t1 = Instant::now();
    let r1 = g.acquire("p", "i1", Some(Duration::from_millis(200))).await;
    let e1 = t1.elapsed();
    println!("【预算 = 排队上限】{r1:?} 耗时 {e1:?}");

    // 预算大于排队上限：仍只等排队上限
    let t2 = Instant::now();
    let r2 = g.acquire("p", "i1", Some(Duration::from_secs(30))).await;
    let e2 = t2.elapsed();
    println!("【预算 30s】{r2:?} 耗时 {e2:?}");

    assert!(matches!(r0, Err(GovernError::Overloaded { .. })));
    assert!(e0 < Duration::from_millis(20), "预算 0 不该排队：{e0:?}");
    assert!(
        e1 >= Duration::from_millis(180) && e1 < Duration::from_millis(400),
        "预算=排队上限时应等满约 200ms：{e1:?}"
    );
    assert!(
        e2 >= Duration::from_millis(180) && e2 < Duration::from_millis(400),
        "预算远大于排队上限时也只等排队上限：{e2:?}"
    );

    // 名额空出来后，budget=0 是「拿到」还是「拿不到」？
    drop(held);
    let r3 = g.acquire("p", "i1", Some(Duration::ZERO)).await;
    println!("【名额空着 + 预算 0】{:?}", r3.as_ref().map(|_| "Ok"));
    assert!(
        r3.is_ok(),
        "预算 0 时若名额本来就空着仍会放行（这是后续「预算 0 也算实例失败」的入口）"
    );
    if let Ok(p) = r3 {
        p.record(true);
    }
}

// ---------------------------------------------------------------------------
// 8. 探测成功之后，permit 的生命周期
// ---------------------------------------------------------------------------

/// 拿到许可后连续记录两次是不可能的（`record` 消费 self），
/// 这里验证「record 之后再 drop」不会破坏状态。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 记录之后正常归还名额() {
    let g = gov(1, 20, 1, 30);
    g.acquire("p", "i1", None)
        .await
        .expect("应拿到许可")
        .record(true);
    g.acquire("p", "i1", None)
        .await
        .expect("record 之后名额应已归还")
        .record(true);
    assert_eq!(g.tracked_instances(), 1);
}

/// `prune` 对「已跳闸但实例仍在注册表」的处理。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_不会清掉正在冷却的实例() {
    let g = gov(2, 20, 1, 60_000);
    trip(&g, "p", "i1", 1).await;
    let alive: HashSet<String> = HashSet::new();
    assert_eq!(g.prune(&alive), 0, "正在冷却的实例不该被清");
    assert!(matches!(
        g.acquire("p", "i1", Some(Duration::ZERO)).await,
        Err(GovernError::CircuitOpen { .. })
    ));
}

/// 让 `Permit` 在析构顺序上走在 `Governor` 之后（Arc 生命周期），不应 panic。
#[test]
fn 治理器先于许可被丢弃也不该出问题() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("建运行时");
    let permit: Permit = rt.block_on(async {
        let g = gov(1, 20, 1, 30);
        g.acquire("p", "i1", None).await.expect("应拿到许可")
    });
    drop(permit); // Governor 已经 drop
}
