//! MCP 面的总线工具：死信、重放、触发器、异步触发。
//!
//! 这一层是**给 agent 用的**，所以用例里除了「功能对不对」，还在意「工具描述有没有
//! 把话说清楚」——agent 只看 tools/list 里的那段说明，描述里漏掉一个前提（比如重放
//! 必须自带载荷），它就只能靠猜。有几个断言是专门守这个的。

use std::sync::Arc;
use std::time::Duration;

use hub_bus::{Bus, BusConfig};
use hub_engine::{AsyncExecutor, FlowExecutor, Invoker};
use hub_mcp::HubMcp;
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_store::dead_letters;
use hub_store::flows::{self, DraftInput};
use hub_store::triggers;
use serde_json::{Value, json};
use sqlx::PgPool;
use ulid::Ulid;

/// 测试用的 Redis。
///
/// 默认硬编码到本地 db 9，**刻意不读 REDIS_URL**：.env 里的那个指向应用的
/// db 2，测试键混进去会污染正在跑的中台。
/// TEST_REDIS_URL 只用来在 CI 里指向另一个端口——CI 自己起一个 Redis，
/// 不假定 runner 上正好有一个（跟测试 PG 用非默认端口是同一个理由）。
fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/9".to_string())
}

struct Harness {
    mcp: HubMcp,
    store: Store,
    bus: Bus,
}

async fn harness(pool: PgPool, tag: &str, with_async: bool) -> Harness {
    let store = Store::from_pool(pool);
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let executor = FlowExecutor::new(registry.clone(), invoker.clone());
    let flows = hub_engine::FlowService::new(store.clone(), executor.clone());

    let bus = Bus::connect(
        &redis_url(),
        BusConfig {
            stream: format!("test:mcp:{tag}:{}", Ulid::generate()),
            group: "test-workers".to_string(),
            consumer: "test-consumer".to_string(),
            block: Duration::from_millis(50),
            ..BusConfig::default()
        },
    )
    .await
    .expect("连接 Redis 失败——本机 6379 上应有 Redis");

    let mut mcp = HubMcp::new(store.clone(), invoker, flows);
    if with_async {
        mcp = mcp.with_async(AsyncExecutor::new(store.clone(), bus.clone(), executor));
    }

    Harness { mcp, store, bus }
}

fn text_json(result: &rmcp::model::CallToolResult) -> Value {
    let text = result
        .content
        .iter()
        .find_map(|block| block.as_text().map(|t| t.text.clone()))
        .expect("工具应返回文本内容");
    serde_json::from_str(&text).expect("工具应返回 JSON 文本")
}

/// 一条 a → b 的链，已发布。
async fn publish_chain(store: &Store, name: &str) {
    flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name,
            description: "MCP 总线工具测试",
            definition: &json!({
                "name": name,
                "nodes": [
                    {"id": "a", "plugin": "flow-first"},
                    {"id": "b", "plugin": "flow-second"}
                ],
                "edges": [{"from": "a", "to": "b"}]
            }),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("存草稿应成功");
    flows::publish_draft(store.pool(), name)
        .await
        .expect("发布应成功");
}

async fn a_dead_letter(store: &Store, stream_id: &str, flow: Option<&str>) -> i64 {
    dead_letters::insert(
        store.pool(),
        &dead_letters::NewDeadLetter {
            stream: "hub:flows",
            stream_id,
            run_id: Some("run-x"),
            flow_name: flow,
            node_id: Some("b"),
            attempts: 5,
            error: "插件不可达",
            payload_summary: Some(r#"{"payload_bytes":42}"#),
        },
    )
    .await
    .expect("写死信应成功")
}

// ---------------------------------------------------------------- 死信

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 列出死信给出来龙去脉(pool: PgPool) {
    let h = harness(pool, "list", true).await;
    a_dead_letter(&h.store, "1-1", Some("intake")).await;

    let result = h
        .mcp
        .list_dead_letters(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::ListDeadLettersArgs {
                pending: None,
                limit: None,
            },
        ))
        .await
        .expect("工具调用失败");
    let value = text_json(&result);

    let rows = value.as_array().expect("应返回数组");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["flow_name"], "intake");
    assert_eq!(rows[0]["node_id"], "b");
    assert_eq!(rows[0]["attempts"], 5);
    assert_eq!(
        rows[0]["error"], "插件不可达",
        "agent 要靠这条错误判断该不该重放"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 重放死信产生新执行(pool: PgPool) {
    let h = harness(pool, "replay", true).await;
    publish_chain(&h.store, "intake").await;
    let id = a_dead_letter(&h.store, "2-1", Some("intake")).await;

    let result = h
        .mcp
        .replay_dead_letter(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::ReplayDeadLetterArgs {
                id,
                payload: json!({"orderId": "SO-9"}),
                timeout_ms: None,
            },
        ))
        .await
        .expect("重放应成功");
    let value = text_json(&result);

    assert_eq!(value["status"], "replayed");
    let run_id = value["run_id"].as_str().expect("应返回 run_id");

    let letter = dead_letters::find(h.store.pool(), id)
        .await
        .unwrap()
        .expect("应查得到");
    assert_eq!(
        letter.replayed_run_id.as_deref(),
        Some(run_id),
        "要留下「重放成了哪次新执行」，否则 agent 放完就追踪不下去了"
    );

    let deliveries = h.bus.receive().await.expect("应取到消息");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].message.run_id, run_id);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 重复重放被拒且不产生第二次执行(pool: PgPool) {
    let h = harness(pool, "twice", true).await;
    publish_chain(&h.store, "intake").await;
    let id = a_dead_letter(&h.store, "3-1", Some("intake")).await;

    let args = || hub_mcp::ReplayDeadLetterArgs {
        id,
        payload: json!({"a": 1}),
        timeout_ms: None,
    };

    h.mcp
        .replay_dead_letter(rmcp::handler::server::wrapper::Parameters(args()))
        .await
        .expect("第一次应成功");

    let err = h
        .mcp
        .replay_dead_letter(rmcp::handler::server::wrapper::Parameters(args()))
        .await
        .expect_err("第二次该被拒");
    assert!(
        err.message.contains("已经重放"),
        "要说清为什么拒，agent 才知道不用再试：{}",
        err.message
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 没有编排归属的死信重放被拒(pool: PgPool) {
    let h = harness(pool, "noflow", true).await;
    let id = a_dead_letter(&h.store, "4-1", None).await;

    let err = h
        .mcp
        .replay_dead_letter(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::ReplayDeadLetterArgs {
                id,
                payload: json!({"a": 1}),
                timeout_ms: None,
            },
        ))
        .await
        .expect_err("该被拒");
    assert!(err.message.contains("编排"), "{}", err.message);

    // 抢占要撤掉：抢占了却没真的重放，那条死信会显示「已重放」而实际什么都没发生
    let letter = dead_letters::find(h.store.pool(), id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        letter.replayed_at.is_none(),
        "失败的这次不该把死信标成已重放"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 未装配总线时死信工具明确报错(pool: PgPool) {
    let h = harness(pool, "noasync", false).await;
    let id = a_dead_letter(&h.store, "5-1", Some("intake")).await;

    let err = h
        .mcp
        .replay_dead_letter(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::ReplayDeadLetterArgs {
                id,
                payload: json!({"a": 1}),
                timeout_ms: None,
            },
        ))
        .await
        .expect_err("该报错");

    assert!(
        err.message.contains("异步链"),
        "要说清是「这个能力没开」而不是笼统的失败，否则 agent 会一直重试：{}",
        err.message
    );
    let _ = h.bus;
}

// ---------------------------------------------------------------- 触发器

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 登记并列出触发器(pool: PgPool) {
    let h = harness(pool, "trigger", true).await;
    publish_chain(&h.store, "intake").await;

    let saved = h
        .mcp
        .save_trigger(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::SaveTriggerArgs {
                flow: "intake".to_string(),
                kind: triggers::KIND_CRON.to_string(),
                name: None,
                config: json!({"expr": "0 2 * * *"}),
            },
        ))
        .await
        .expect("登记应成功");
    let row = text_json(&saved);
    assert_eq!(row["kind"], "cron");
    assert_eq!(row["name"], "default", "不传名字时默认 default");
    assert_eq!(row["enabled"], true);
    assert_eq!(row["fired_count"], 0);

    let listed = h
        .mcp
        .list_triggers(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::ListTriggersArgs {
                flow: Some("intake".to_string()),
            },
        ))
        .await
        .expect("列出应成功");
    assert_eq!(text_json(&listed).as_array().map(Vec::len), Some(1));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 挂在不存在的_flow_上被拒(pool: PgPool) {
    let h = harness(pool, "badflow", true).await;

    let err = h
        .mcp
        .save_trigger(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::SaveTriggerArgs {
                flow: "从来没有过".to_string(),
                kind: triggers::KIND_CRON.to_string(),
                name: None,
                config: json!({"expr": "0 2 * * *"}),
            },
        ))
        .await
        .expect_err("该被拒");

    assert!(
        err.message.contains("从来没有过"),
        "要说清是哪条 flow，而不是抛一句外键约束错误：{}",
        err.message
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 非法触发器类型被拒(pool: PgPool) {
    let h = harness(pool, "badkind", true).await;
    publish_chain(&h.store, "intake").await;

    let err = h
        .mcp
        .save_trigger(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::SaveTriggerArgs {
                flow: "intake".to_string(),
                kind: "webhook".to_string(),
                name: None,
                config: json!({}),
            },
        ))
        .await
        .expect_err("该被拒");

    assert!(err.message.contains("webhook"), "{}", err.message);
}

// ---------------------------------------------------------------- 异步触发

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 异步触发入队(pool: PgPool) {
    let h = harness(pool, "async", true).await;
    publish_chain(&h.store, "intake").await;

    let result = h
        .mcp
        .trigger_flow_async(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::TriggerFlowArgs {
                flow: "intake".to_string(),
                payload: json!({"orderId": "SO-1"}),
                message_id: Some("msg-mcp".to_string()),
                timeout_ms: None,
            },
        ))
        .await
        .expect("入队应成功");
    let value = text_json(&result);

    assert_eq!(value["status"], "queued");
    let run_id = value["run_id"].as_str().expect("应有 run_id");

    let run = hub_store::runs::find_run(h.store.pool(), run_id)
        .await
        .unwrap()
        .expect("入队后就该有 run 行");
    assert_eq!(run.status, "queued");

    let deliveries = h.bus.receive().await.expect("应取到消息");
    assert_eq!(deliveries[0].message.node_id, "a");
    assert_eq!(
        deliveries[0].message.envelope.as_ref().unwrap().message_id,
        "msg-mcp",
        "agent 指定的幂等键应被沿用"
    );
}
