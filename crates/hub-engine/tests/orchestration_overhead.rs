//! 编排开销基线：P99 目标 20ms。
//!
//! 「编排开销」是 [`FlowRun::elapsed_ms`] 减去各节点实际调用耗时之和——也就是**中台自己**
//! 花在解析计划、装配信封、调度、记账上的时间。把它跟插件耗时分开是刻意的：插件慢是
//! 插件的问题，而这条基线要守的是「中台不该成为瓶颈」。
//!
//! 所以这里的插件是**故意做到极快**的（本机 gRPC、无业务逻辑）。插件一慢，节点耗时就
//! 盖住了编排开销，这条基线也就测不出东西了。
//!
//! 目标值取自 `docs/design.md` 的容量目标（编排开销 P99 < 20ms）。20ms 对一段纯内存
//! 的调度逻辑是很宽的线——留这么宽是因为 CI 机器可能被别的任务压着，而这条线要守的是
//! **数量级**，不是微秒。

use std::sync::Arc;
use std::time::Instant;

use hub_engine::{FlowExecutor, Invoker};
use hub_flow::FlowDefinition;
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_testkit as kit;
use serde_json::json;
use sqlx::PgPool;

/// 跑多少次。够算 P99 又不至于让用例跑太久。
const ROUNDS: usize = 300;

/// 并发度：同时压多少条链。容量目标里有「500 并发 flow」一条，这里取小一档，
/// 目的是看编排层在争用下的表现，而不是压满机器。
const CONCURRENCY: usize = 32;

/// 编排开销的 P99 目标（毫秒）。
const P99_BUDGET_MS: f64 = 20.0;

/// 一条 3 节点的链：a → b → c。
fn chain() -> FlowDefinition {
    serde_json::from_value(json!({
        "name": "bench",
        "nodes": [
            {"id": "a", "plugin": "bench-a"},
            {"id": "b", "plugin": "bench-b"},
            {"id": "c", "plugin": "bench-c"}
        ],
        "edges": [{"from": "a", "to": "b"}, {"from": "b", "to": "c"}]
    }))
    .expect("链定义应能解析")
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    // 最近秩法：小样本下比插值更保守（不会把一次真实的长尾抹平）
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 三节点链的编排开销_p99_在预算内(pool: PgPool) {
    let store = Store::from_pool(pool);
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );

    // 三个极快的插件：让编排开销成为可测的那一部分。
    // 夹具收进 Vec 而不是就地丢弃——它们要活到压测结束，插件进程一停，
    // 后面测到的就全是「连不上」的失败路径耗时了。
    let mut fixtures = Vec::new();
    for name in ["bench-a", "bench-b", "bench-c"] {
        let fixture = kit::start(kit::Behavior::named(name, "1.0.0")).await;
        let response = registry.register(&fixture.register_request(), None).await;
        assert!(
            response.accepted,
            "注册 {name} 应通过：{:?}",
            response.rejections
        );
        fixtures.push(fixture);
    }

    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let executor = FlowExecutor::new(registry, invoker);
    let definition = chain();

    // 先热身：第一次调用要建 gRPC 连接、解析实例，那部分不算稳态开销
    for _ in 0..20 {
        let run = executor
            .run(&definition, hub_proto::Envelope::default())
            .await;
        assert!(run.succeeded(), "热身执行应成功：{:?}", run.error);
    }

    let started = Instant::now();
    let mut overheads = Vec::with_capacity(ROUNDS);

    for _ in 0..ROUNDS.div_ceil(CONCURRENCY) {
        let mut batch = Vec::with_capacity(CONCURRENCY);
        for _ in 0..CONCURRENCY {
            let executor = executor.clone();
            let definition = definition.clone();
            batch.push(tokio::spawn(async move {
                let run = executor
                    .run(&definition, hub_proto::Envelope::default())
                    .await;
                assert!(run.succeeded(), "压测中的执行不该失败：{:?}", run.error);
                // 编排开销 = 总耗时 − 各节点实际调用耗时之和
                run.elapsed_ms as f64 - run.node_time_ms() as f64
            }));
        }
        for handle in batch {
            overheads.push(handle.await.expect("压测任务不应 panic"));
        }
    }

    let wall = started.elapsed();
    let mut sorted = overheads.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let p50 = percentile(&sorted, 50.0);
    let p95 = percentile(&sorted, 95.0);
    let p99 = percentile(&sorted, 99.0);
    let max = sorted.last().copied().unwrap_or(0.0);

    println!(
        "【编排开销】样本 {} 条 3 节点链，并发 {CONCURRENCY}，墙钟 {:.2}s\n\
         \x20 P50 {p50:.3}ms  P95 {p95:.3}ms  P99 {p99:.3}ms  MAX {max:.3}ms",
        overheads.len(),
        wall.as_secs_f64()
    );
    println!(
        "【吞吐】{:.0} 次执行/秒（每次 3 个节点调用）",
        overheads.len() as f64 / wall.as_secs_f64()
    );

    assert!(
        p99 < P99_BUDGET_MS,
        "编排开销 P99 超出预算：实测 {p99:.3}ms，预算 {P99_BUDGET_MS}ms\
         （P50 {p50:.3}ms / P95 {p95:.3}ms / MAX {max:.3}ms）"
    );
    assert!(
        overheads.iter().all(|o| *o >= 0.0),
        "编排开销为负说明记账错了：节点耗时之和不可能超过总耗时"
    );

    drop(fixtures);
}
