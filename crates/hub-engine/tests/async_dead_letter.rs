//! 重投耗尽的出口。
//!
//! `BUS_MAX_DELIVERY` 曾经是个不生效的配置：`run_once` 的注释写着「重投次数用完时
//! 由留存巡检送进死信」，而死信表那边的注释也预期「巡检和消费循环同时判定它已耗尽」
//! ——但留存巡检只按**时间**判定（默认 24 小时），根本读不到次数。
//! 于是唯一那条出路是「在总线上躺满足留期」，期间这条消息每一轮被接管都要完整重跑
//! 一遍注定失败的逻辑。
//!
//! 这条用例用一条**有环的编排**让它必然失败（`plan()` 解不出执行计划），
//! 一轮一轮消费到上限，断言它进了死信表。

use std::sync::Arc;
use std::time::Duration;

use hub_bus::{Bus, BusConfig};
use hub_engine::{AsyncExecutor, FlowExecutor, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_proto::{BusMessage, Envelope};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_store::flows::{self, DraftInput};
use serde_json::json;
use sqlx::PgPool;
use ulid::Ulid;

/// 测试用的 Redis。
///
/// 默认硬编码到本地 db 9，**刻意不读 `REDIS_URL`**：`.env` 里的那个指向应用的
/// db 2，测试键混进去会污染正在跑的中台。
/// `TEST_REDIS_URL` 只用来在 CI 里指向另一个端口——CI 自己起一个 Redis，
/// 不假定 runner 上正好有一个（跟测试 PG 用非默认端口是同一个理由）。
fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/9".to_string())
}

/// 重投上限。取 2 而不是默认的 5：这条用例要真走完「试满 → 进死信」，
/// 每多一次就多一个「等接管 → 再失败」的回合。
const MAX_DELIVERY: i64 = 2;

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 重投耗尽的异步消息进死信(pool: PgPool) {
    let store = Store::from_pool(pool);

    // 有环 → `plan()` 必失败 → `handle` 每轮都返回 Err。
    // 保存时校验会报 Cycle 但**不会拒绝**（「保存不被拒」是刻意的，编排是一步步改出来的），
    // 所以它存得下来——这正是这条用例能成立的前提。
    flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name: "cyclic",
            description: "有环，计划排不出来",
            definition: &json!({
                "name": "cyclic",
                "nodes": [
                    {"id": "a", "plugin": "whatever"},
                    {"id": "b", "plugin": "whatever"}
                ],
                "edges": [{"from": "a", "to": "b"}, {"from": "b", "to": "a"}]
            }),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("存草稿应成功");
    flows::publish_draft(store.pool(), "cyclic")
        .await
        .expect("发布应成功");

    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());

    let bus = Bus::connect(
        &redis_url(),
        BusConfig {
            stream: format!("test:dead-letter:{}", Ulid::generate()),
            group: "test-workers".to_string(),
            consumer: "test-consumer".to_string(),
            block: Duration::from_millis(50),
            // 接管阈值调到 1ms：这条用例要连着触发好几轮 receive，
            // 默认的 60 秒会让它永远跑不完
            claim_min_idle: Duration::from_millis(1),
            max_delivery: MAX_DELIVERY,
            ..BusConfig::default()
        },
    )
    .await
    .expect("连接 Redis 失败——本机 6379 上应有 Redis");

    let exec = AsyncExecutor::new(
        store.clone(),
        bus.clone(),
        FlowExecutor::new(registry, invoker),
    );

    let run_id = Ulid::generate().to_string();
    bus.publish(&BusMessage {
        kind: hub_proto::v1::bus_message::Kind::Node as i32,
        run_id: run_id.clone(),
        flow_name: "cyclic".to_string(),
        flow_revision: 1,
        node_id: "a".to_string(),
        envelope: Some(Envelope::default()),
        ..Default::default()
    })
    .await
    .expect("投递应成功");

    // 一轮一轮消费到它进死信。
    // 判定在「失败之后」，所以要到第 MAX_DELIVERY 轮才会写死信。
    let mut rounds = 0;
    loop {
        rounds += 1;
        exec.run_once()
            .await
            .expect("消费一轮本身不该失败——失败的是那条消息，不是基础设施");

        let letters = hub_store::dead_letters::list(store.pool(), true, 10)
            .await
            .expect("查死信应成功");

        if !letters.is_empty() {
            assert_eq!(letters.len(), 1, "同一条消息只该有一条死信");
            let letter = &letters[0];
            assert_eq!(letter.run_id.as_deref(), Some(run_id.as_str()));
            assert_eq!(letter.flow_name.as_deref(), Some("cyclic"));
            assert_eq!(letter.node_id.as_deref(), Some("a"));
            assert_eq!(
                letter.attempts as i64, MAX_DELIVERY,
                "记的应当是「试满的那一次」的次数"
            );
            assert!(
                !letter.stream_id.is_empty(),
                "流 id 要留着——重放与排查都靠它"
            );
            assert!(
                letter.payload_summary.is_some(),
                "信封在，摘要就该有——重放的人要先看它判断这份数据长什么样"
            );
            return;
        }

        assert!(rounds < 20, "试了 {rounds} 轮还没进死信——出口没接上");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
