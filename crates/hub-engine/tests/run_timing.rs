//! 执行记录的起止时间。
//!
//! 这条性质看起来琐碎，但它是控制台「这次执行花了多久」的唯一来源：
//! `runs` 表没有耗时列，耗时是拿 `finished_at - started_at` 算的。
//! 一旦两列记的是同一个时刻（例如都写成结算那一刻），算出来恒为 0——
//! 而 0ms 是个**看起来很正常**的值，不会有人怀疑它，只会以为执行很快。
//! 所以这里用真实链路把它钉住。

use std::sync::Arc;
use std::time::Duration;

use hub_engine::{FlowExecutor, FlowService, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_store::flows::{self, DraftInput};
use hub_testkit as kit;
use serde_json::json;
use sqlx::PgPool;

/// 故意让插件慢一点：瞬间完成的执行会让「起止时间相同」与「执行真的很快」
/// 这两件事难以区分，测试也就钉不住任何东西。
const PLUGIN_DELAY: Duration = Duration::from_millis(25);

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 执行记录的起止时间能算出真实耗时(pool: PgPool) {
    let store = Store::from_pool(pool);
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );

    let mut behavior = kit::Behavior::named("timing-plugin", "1.0.0");
    behavior.delay = Some(PLUGIN_DELAY);
    let fixture = kit::start(behavior).await;
    let response = registry.register(&fixture.register_request(), None).await;
    assert!(response.accepted, "注册应通过：{:?}", response.rejections);

    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let service = FlowService::new(store.clone(), FlowExecutor::new(registry, invoker));

    flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name: "timing",
            description: "起止时间",
            definition: &json!({
                "name": "timing",
                "nodes": [{"id": "only", "plugin": "timing-plugin"}],
                "edges": []
            }),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("存草稿应成功");
    flows::publish_draft(store.pool(), "timing")
        .await
        .expect("发布应成功");

    let outcome = service
        .trigger("timing", hub_proto::Envelope::default(), None)
        .await
        .expect("触发应成功");
    assert!(
        outcome.run.succeeded(),
        "执行应成功：{:?}",
        outcome.run.error
    );

    let detail = hub_store::runs::find_run_detail(store.pool(), &outcome.run.run_id)
        .await
        .expect("查执行详情应成功")
        .expect("执行记录应已落库");

    let earliest_node = detail
        .nodes
        .iter()
        .map(|n| n.started_at)
        .min()
        .expect("应有节点记录");

    // 核心断言：run 的起点是「这次执行何时开始」，不是「何时结算完」。
    // 拿结算时刻当起点的话它会等于 finished_at（下面那条断言也会跟着失败）。
    assert_eq!(
        detail.run.started_at, earliest_node,
        "run 的起点应当取最早那个节点的开始时刻"
    );

    let finished_at = detail.run.finished_at.expect("执行已结束，应当有结束时间");
    assert!(
        finished_at > detail.run.started_at,
        "结束时间必须晚于开始时间，否则控制台按起止时间算耗时恒为 0\
         （起点 {}，终点 {}）",
        detail.run.started_at,
        finished_at
    );

    // 插件的延迟是 25ms，真实耗时不应当离它太远——这条用来挡住
    // 「时间戳是有了但记的是别的什么东西」这种形态的问题。
    let elapsed_ms = (finished_at - detail.run.started_at).num_milliseconds();
    assert!(
        elapsed_ms >= PLUGIN_DELAY.as_millis() as i64,
        "耗时 {elapsed_ms}ms 小于插件的固定延迟 {}ms，说明起止时间没有覆盖真实执行",
        PLUGIN_DELAY.as_millis()
    );

    drop(fixture);
}
