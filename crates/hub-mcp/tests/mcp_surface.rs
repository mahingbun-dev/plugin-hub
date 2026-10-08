//! MCP 面：工具逻辑 + 工具注册 + 端点接线。
//!
//! 工具都是 `pub async fn`，直接调用即可覆盖逻辑；但**直接调方法覆盖不到「它有没有
//! 注册进工具面」**——`#[tool]` 属性漏了，方法照样好调、测试照样全绿，agent 却在
//! `tools/list` 里看不见它。所以「注册」单独锁一条（`工具面注册齐了全部工具`），
//! 端点接线再发真实的 JSON-RPC 验证——不引 rmcp 的客户端传输（它会拉 reqwest 与
//! TLS provider，为一个测试把离线构建镜像压重不划算），握手与 session 头自己来。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use hub_engine::Invoker;
use hub_mcp::HubMcp;
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_proto::v1::{RegisterRequest, RegisterResponse};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use hub_testkit as kit;
// get_tool 是 ServerHandler 的方法，`#[tool_handler]` 会把它接到 tool_router 上
use rmcp::ServerHandler as _;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

struct Harness {
    mcp: HubMcp,
    app: Router,
    registry: Registry,
}

fn harness(store: Store) -> Harness {
    harness_with_hosts(store, None)
}

/// 与 [`harness`] 相同，但能配 MCP 面的 Host 白名单。`None` = 沿用 rmcp 默认。
fn harness_with_hosts(store: Store, hosts: Option<Vec<String>>) -> Harness {
    harness_full(store, hosts, false)
}

/// 与 [`harness`] 相同，但能开关登录闸门（默认关——历史行为）。
fn harness_with_gate(store: Store, gate: bool) -> Harness {
    harness_full(store, None, gate)
}

fn harness_full(store: Store, hosts: Option<Vec<String>>, login_gate: bool) -> Harness {
    let client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*client).clone());
    let flows = hub_engine::FlowService::new(
        store.clone(),
        hub_engine::FlowExecutor::new(registry.clone(), invoker.clone()),
    );
    let mcp = HubMcp::new(store, invoker, flows)
        .with_allowed_hosts(hosts)
        .with_login_gate(login_gate);
    let app = mcp.clone().router(CancellationToken::new());
    Harness { mcp, app, registry }
}

/// 取工具返回的第一段文本并解析成 JSON。
fn text_json(result: &rmcp::model::CallToolResult) -> Value {
    let text = result
        .content
        .iter()
        .find_map(|block| block.as_text().map(|t| t.text.clone()))
        .expect("工具应返回文本内容");
    serde_json::from_str(&text).expect("工具应返回 JSON 文本")
}

async fn register(fixture: &kit::Fixture, registry: &Registry) -> bool {
    registry
        .register(&fixture.register_request(), None)
        .await
        .accepted
}

// ---------------------------------------------------------------- 工具逻辑

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 列出插件工具返回已注册插件(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(register(&fixture, &h.registry).await);

    let result = h.mcp.list_plugins().await.expect("工具调用失败");
    let value = text_json(&result);

    let plugins = value.as_array().expect("应返回数组");
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["name"], "echo-plugin");
    assert_eq!(plugins[0]["instance_count"], 1);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件详情工具给出契约与实例(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(register(&fixture, &h.registry).await);

    let result = h
        .mcp
        .get_plugin(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::GetPluginArgs {
                name: "echo-plugin".to_string(),
            },
        ))
        .await
        .expect("工具调用失败");
    let value = text_json(&result);

    assert_eq!(value["name"], "echo-plugin");
    let version = &value["versions"][0];
    assert_eq!(version["version"], "1.0.0");

    // 契约按 (direction, fq_name) 排序，不要依赖下标——夹具同时声明了消费与生产
    let contracts = version["contracts"].as_array().expect("应有契约");
    assert!(
        contracts
            .iter()
            .any(|c| c["direction"] == "produces" && c["fq_name"] == kit::MESSAGE_FQ),
        "应能看到它生产什么：{contracts:?}"
    );
    assert!(
        contracts
            .iter()
            .any(|c| c["direction"] == "consumes" && c["fq_name"] == kit::MESSAGE_FQ),
        "也应能看到它消费什么：{contracts:?}"
    );

    assert_eq!(
        version["instances"][0]["advertise_addr"],
        fixture.base_url()
    );
}

/// get_plugin 要透出 manifest 声明的 invokes：互调授权名单只存在 manifest 字节里，
/// 解析丢字段不会报错、只会静默少一项，所以必须专门锁住「声明了什么就透出什么」。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件详情工具透出_manifest_声明的_invokes(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));

    let mut request = fixture.register_request();
    if let Some(manifest) = request.manifest.as_mut() {
        // invokes 的目标校验发生在调用期（不在注册期），声明一个没注册过的名字
        // 不影响注册被接受——注册期只管 manifest 自洽。
        manifest.invokes = vec!["ping-callee".to_string()];
    }
    assert!(h.registry.register(&request, None).await.accepted);

    let result = h
        .mcp
        .get_plugin(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::GetPluginArgs {
                name: "echo-plugin".to_string(),
            },
        ))
        .await
        .expect("工具调用失败");
    let value = text_json(&result);

    let version = &value["versions"][0];
    assert_eq!(
        version["invokes"],
        json!(["ping-callee"]),
        "manifest 里声明的互调名单必须原样透出"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 拒绝留痕工具给出拒绝原因与次数(pool: PgPool) {
    let h = harness(Store::from_pool(pool));

    // 造一条 VERSION_CONFLICT 留痕：来源是谁不重要（registry 侧的落库
    // 链路已由它的集成测试覆盖），这里锁的是 MCP 面的视图形状
    hub_store::rejections::record(
        // harness 吃掉了 store，直接从 registry 取同一个池
        h.registry.store().pool(),
        &hub_store::rejections::NewRejection {
            plugin_name: "sql-executor",
            instance_id: "sql-executor-1",
            code: 5,
            version: "0.2.0",
            message: "版本 0.2.0 已存在且契约或 manifest 不一致",
            detail: "同一版本号不可改变契约；请升版本号后重新注册",
            source_ip: "127.0.0.1",
        },
    )
    .await
    .expect("造拒绝记录失败");

    let result = h
        .mcp
        .list_register_rejections(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::ListRejectionsArgs {
                plugin: None,
                limit: None,
            },
        ))
        .await
        .expect("工具调用失败");
    let value = text_json(&result);

    let rows = value.as_array().expect("应返回数组");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["plugin_name"], "sql-executor");
    assert_eq!(rows[0]["code_name"], "VERSION_CONFLICT");
    assert_eq!(rows[0]["count"], 1);
    assert!(
        rows[0]["detail"]
            .as_str()
            .unwrap_or("")
            .contains("升版本号"),
        "detail 是给插件方的下一步动作，必须原样透出"
    );

    // 按插件过滤
    let result = h
        .mcp
        .list_register_rejections(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::ListRejectionsArgs {
                plugin: Some("别的插件".to_string()),
                limit: None,
            },
        ))
        .await
        .expect("工具调用失败");
    assert_eq!(text_json(&result).as_array().map(Vec::len), Some(0));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 查询未注册插件返回参数错误(pool: PgPool) {
    let h = harness(Store::from_pool(pool));

    let err = h
        .mcp
        .get_plugin(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::GetPluginArgs {
                name: "不存在".to_string(),
            },
        ))
        .await
        .expect_err("应报错");
    assert!(err.message.contains("未注册"), "实际 {}", err.message);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 影响面工具能反查到生产方(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(register(&fixture, &h.registry).await);

    let result = h
        .mcp
        .describe_message(rmcp::handler::server::wrapper::Parameters(
            hub_mcp::DescribeMessageArgs {
                fq_name: kit::MESSAGE_FQ.to_string(),
            },
        ))
        .await
        .expect("工具调用失败");
    let value = text_json(&result);

    assert_eq!(value["fq_name"], kit::MESSAGE_FQ);
    assert_eq!(value["producers"][0][0], "echo-plugin");
    assert_eq!(value["producers"][0][1], "1.0.0");
    // 夹具同时消费同一个类型（链路上的节点就是这样），所以两侧都该有它
    assert_eq!(
        value["consumers"].as_array().map(Vec::len),
        Some(1),
        "消费方也应能查到：{value}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 调用工具把载荷送进插件并回传结果(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(register(&fixture, &h.registry).await);

    let payload = json!({"orderId": "SO-9", "qty": 2});
    let result = h
        .mcp
        .invoke_plugin_as(
            None,
            None,
            hub_mcp::InvokePluginArgs {
                plugin: "echo-plugin".to_string(),
                payload: payload.clone(),
                version: None,
                message_id: Some("msg-42".to_string()),
                timeout_ms: None,
            },
        )
        .await
        .expect("工具调用失败");
    let value = text_json(&result);

    assert_eq!(value["status"], "handled");
    assert_eq!(value["plugin"], "echo-plugin");
    assert_eq!(value["payload"], payload);
    assert_eq!(fixture.handled_count(), 1);
    assert_eq!(
        fixture.last_envelope().expect("插件应收到信封").message_id,
        "msg-42"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 校验拒绝时工具返回结构化问题而非报错(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(register(&fixture, &h.registry).await);

    let result = h
        .mcp
        .invoke_plugin_as(
            None,
            None,
            hub_mcp::InvokePluginArgs {
                plugin: "echo-plugin".to_string(),
                payload: json!({"a": 1}),
                version: None,
                message_id: Some("bad-1".to_string()),
                timeout_ms: None,
            },
        )
        .await
        .expect("校验拒绝是业务结果，不该当成工具错误");
    let value = text_json(&result);

    assert_eq!(value["status"], "rejected");
    assert_eq!(value["issues"][0]["path"], "message_id");
    assert_eq!(fixture.handled_count(), 0, "校验不通过时插件体绝不能被执行");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 调用工具拒绝非对象载荷(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(register(&fixture, &h.registry).await);

    let err = h
        .mcp
        .invoke_plugin_as(
            None,
            None,
            hub_mcp::InvokePluginArgs {
                plugin: "echo-plugin".to_string(),
                payload: json!([1, 2]),
                version: None,
                message_id: None,
                timeout_ms: None,
            },
        )
        .await
        .expect_err("应报错");
    assert!(err.message.contains("对象"), "实际 {}", err.message);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 未指定_message_id_时自动生成(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(register(&fixture, &h.registry).await);

    h.mcp
        .invoke_plugin_as(
            None,
            None,
            hub_mcp::InvokePluginArgs {
                plugin: "echo-plugin".to_string(),
                payload: json!({"a": 1}),
                version: None,
                message_id: None,
                timeout_ms: None,
            },
        )
        .await
        .expect("工具调用失败");

    let seen = fixture.last_envelope().expect("插件应收到信封");
    assert!(
        seen.message_id.starts_with("echo-plugin-"),
        "自动生成的 id 应可追溯到插件，实际 {}",
        seen.message_id
    );
}

// ---------------------------------------------------------------- 工具注册

/// 每个工具都得**注册进工具面**，不只是有一个能调用的方法。
///
/// 上面那些用例直接调 `h.mcp.invoke_plugin(...)`，走的是方法本身：`#[tool]` 属性漏了
/// 它们一样全绿。`invoke_plugin` 就这么漏过一回——实现与测试都齐，唯独 `tools/list`
/// 里没有它，agent 一条路都调不到插件。`get_tool` 读的是 `#[tool_router]` 生成的注册
/// 表，属性一漏这里就是 None，锁的正是「注册」这一步。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 工具面注册齐了全部工具(pool: PgPool) {
    // 与 README「MCP 工具面」那张表一一对应；增删工具时两处一起改
    const EXPECTED: [&str; 19] = [
        "list_plugins",
        "get_plugin",
        "list_instances",
        "list_register_rejections",
        "describe_message",
        "invoke_plugin",
        "list_flows",
        "get_flow",
        "save_flow_draft",
        "trigger_flow",
        "trigger_flow_async",
        "list_runs",
        "get_run",
        "list_traces",
        "get_trace",
        "list_dead_letters",
        "replay_dead_letter",
        "list_triggers",
        "save_trigger",
    ];

    let h = harness(Store::from_pool(pool));

    let missing: Vec<&str> = EXPECTED
        .iter()
        .copied()
        .filter(|name| h.mcp.get_tool(name).is_none())
        .collect();

    assert!(
        missing.is_empty(),
        "这些工具没注册进工具面（多半是漏了 #[tool] 属性）：{missing:?}"
    );
}

/// **已认证的身份要跟着信封一起到插件**（MCP 这条路）。
///
/// 与 ingress 那条同源：身份由鉴权中间件放进请求扩展，rmcp 把 HTTP 的
/// `request::Parts`（含那份扩展）注入给工具处理函数，这里再把它填进信封。
///
/// 这条测的是**下半段**（拿到身份之后怎么装进信封）；上半段（身份怎么从凭证来、
/// 中间件有没有真的挂在 `/mcp` 上）在 `hub-server/tests/http_app.rs` 里验——
/// 那一段必须走真实 HTTP，构造不出来。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 调用插件时把身份放进信封(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(register(&fixture, &h.registry).await);

    let subject = hub_proto::v1::Subject {
        kind: hub_proto::v1::SubjectKind::Human as i32,
        id: "u-1".to_string(),
        scopes: vec!["hub:invoke".to_string()],
        ..Default::default()
    };

    h.mcp
        .invoke_plugin_as(
            Some(subject.clone()),
            None,
            hub_mcp::InvokePluginArgs {
                plugin: "echo-plugin".to_string(),
                payload: json!({"a": 1}),
                version: None,
                message_id: Some("with-subject".to_string()),
                timeout_ms: None,
            },
        )
        .await
        .expect("工具调用失败");

    let seen = fixture.last_envelope().expect("插件应收到信封");
    let got = seen.subject.expect("信封里应带上调用主体");
    assert_eq!(got.id, "u-1", "subject.id 要一路传到插件");
    assert_eq!(got.kind, subject.kind, "主体类型要一致");
    assert_eq!(got.scopes, subject.scopes, "权限位要一并带上");
}

// ---------------------------------------------------------------- 工具聚合

/// 一份声明了 MCP 工具的夹具行为。
fn with_one_tool() -> kit::Behavior {
    kit::Behavior {
        tools: vec![hub_proto::v1::ToolDecl {
            name: "echo".to_string(),
            description: "把载荷原样回显".to_string(),
            input_schema_json: r#"{"type":"object","properties":{"a":{"type":"integer"}}}"#
                .to_string(),
            requires_approval: false,
        }],
        ..kit::Behavior::default()
    }
}

/// 拉一次工具列表的名字。
///
/// 走 `mcp_request` 而不是自己解析报文：它顺带断言了状态码与报文可解析性，
/// 这里只想拿到名字。
async fn tool_names(h: &Harness, session: &str) -> Vec<String> {
    let (_, value) = mcp_request(
        &h.app,
        Some(session),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;

    value["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("应返回 tools 数组，实际：{value}"))
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// **插件声明的工具要聚合进工具面**。
///
/// 这是这个面的设计目的：「插件注册即用、agent 立即可见」。宏生成的 `list_tools`
/// 只列中台自己的静态工具，看不到任何插件能力——所以这里手写了它。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件声明的工具会出现在工具列表里(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;

    // 注册前：没有插件工具
    let before = tool_names(&h, &session).await;
    assert!(
        !before.iter().any(|n| n.starts_with("echo-plugin__")),
        "插件还没注册，不该有它的工具：{before:?}"
    );

    let fixture = kit::start(with_one_tool()).await;
    assert!(register(&fixture, &h.registry).await);

    let after = tool_names(&h, &session).await;
    assert!(
        after.contains(&"echo-plugin__echo".to_string()),
        "插件声明的工具应聚合进来（前缀 `插件名__`）：{after:?}"
    );
}

/// **人工确认闸门（P2）的回退路径：客户端不会弹窗时，确认载荷原样放行。**
///
/// 闸门只在客户端声明了 elicitation 能力时强制（CLI 会弹原生确认框）；
/// 没有能力的客户端（简单脚本、旧 agent）拦下来等于永远做不了写入。
/// 放行时的约束仍在别处：卡片自带的 `human_approval_required` 字段与
/// 工具描述里的强制指令。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 客户端不会弹窗时确认载荷原样放行(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;

    let fixture = kit::start(with_one_tool()).await;
    assert!(register(&fixture, &h.registry).await);

    // 载荷形状照搬 sql-executor 的 confirm 卡（带令牌与人工确认标记）。
    // 回显夹具会把它原样吐回来，正好模拟「插件要求人工确认」的结果。
    let approval_payload = json!({
        "status": "confirm",
        "next_action": {
            "tool": "sql_execute",
            "confirmation_token": "ct_TESTTESTTESTTESTTESTTEST",
            "human_approval_required": true
        }
    });

    // 聚合工具路径（sql-executor__sql_submit 走的就是这条）
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "echo-plugin__echo",
                "arguments": approval_payload.clone()
            }
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let delivered: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(
        delivered["payload"]["next_action"]["confirmation_token"], "ct_TESTTESTTESTTESTTESTTEST",
        "客户端不支持确认时按回退放行，令牌必须原样下发：{delivered}"
    );
    // **放行必须让调用方知道闸门没过**：静默放行会让 agent 以为确认是走流程的，
    // 用户也看不到「这次写入没经过人工确认」。
    assert_eq!(
        delivered["payload"]["approval_bypassed"], true,
        "回退放行的卡上必须带 approval_bypassed 标注：{delivered}"
    );
    assert!(
        delivered["payload"]["approval_note"]
            .as_str()
            .unwrap_or_default()
            .contains("人工确认闸门未生效"),
        "approval_note 要把闸门未生效说明白：{delivered}"
    );

    // invoke_plugin 通用入口路径（两条路都要过同一个闸门）
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "invoke_plugin",
                "arguments": {"plugin": "echo-plugin", "payload": approval_payload}
            }
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let delivered: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(
        delivered["payload"]["next_action"]["human_approval_required"], true,
        "通用入口同样按回退放行：{delivered}"
    );
}

/// **tools/list 必须带 ttlMs 与 cacheScope。**
///
/// 协商到 2025-11-25 的客户端把这两个字段当必填：Claude CLI 实测缺失时整个
/// tools/list 校验失败，服务器被整个丢弃——表现为「连接成功但一个工具都
/// 看不见」。ttl=0 + private = 结果不许缓存，与 list_tools「每次现查」的
/// 既有语义一致。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 工具列表携带缓存语义字段(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;

    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;

    let result = &value["result"];
    assert_eq!(
        result["ttlMs"], 0,
        "ttl=0：结果不许缓存，客户端每次都该现查"
    );
    assert_eq!(
        result["cacheScope"], "private",
        "private：只允许同一调用方的客户端缓存"
    );
}

/// **摘除后工具要消失**：留着它，agent 会调到一个已经不在的工具。
///
/// 「在线」的判据是有没有实例行，而摘除是 `DELETE`（见 `instances::sweep_stale`），
/// 所以插件停掉之后它的工具自然就查不出来了。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件下线后它的工具会消失(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;

    let fixture = kit::start(with_one_tool()).await;
    // 这里不走 `register` helper：注销要凭注册时下发的状态凭证，得把响应留下来
    let registered = h.registry.register(&fixture.register_request(), None).await;
    assert!(registered.accepted);
    let state_token = registered.state_token;
    assert!(
        tool_names(&h, &session)
            .await
            .contains(&"echo-plugin__echo".to_string()),
        "注册后应先看得见"
    );

    h.registry
        .unregister(
            &fixture.register_request().instance_id,
            &state_token,
            "测试主动下线",
        )
        .await
        .expect("注销应成功");

    assert!(
        !tool_names(&h, &session)
            .await
            .contains(&"echo-plugin__echo".to_string()),
        "下线后不该再列出它的工具"
    );
}

/// 一份可换版本号的夹具行为：`echo` 各版本都有（description 带版本号，可辨来源），
/// `legacy` 只有 0.1.0 有——用来区分「同名取最新」与「旧版独有工具不下发」。
fn with_echo_and_legacy(version: &str) -> kit::Behavior {
    let echo = hub_proto::v1::ToolDecl {
        name: "echo".to_string(),
        description: format!("echo 来自 {version}"),
        input_schema_json: r#"{"type":"object","properties":{"a":{"type":"integer"}}}"#.to_string(),
        requires_approval: false,
    };
    let mut behavior = kit::Behavior {
        tools: vec![echo],
        ..kit::Behavior::named("dup-plugin", version)
    };
    if version == "0.1.0" {
        behavior.tools.push(hub_proto::v1::ToolDecl {
            name: "legacy".to_string(),
            description: "只有 0.1.0 才有的工具".to_string(),
            input_schema_json: "{}".to_string(),
            requires_approval: false,
        });
    }
    behavior
}

/// 注册一个 0.1.0 在线 + 0.2.0 在线的同名双版本场景，返回 0.2.0 的注册响应。
///
/// `register_request` 的 instance_id 按插件名生成，两份夹具会撞行——第二份得换个
/// instance_id，否则注册会把实例行顶给新版本，0.1.0 就没有在线实例了。
async fn 注册同名双版本都在线(
    h: &Harness,
) -> (
    kit::Fixture,
    kit::Fixture,
    RegisterRequest,
    RegisterResponse,
) {
    let old = kit::start(with_echo_and_legacy("0.1.0")).await;
    let new = kit::start(with_echo_and_legacy("0.2.0")).await;
    assert!(register(&old, &h.registry).await);

    let mut new_request = new.register_request();
    new_request.instance_id = "instance-dup-plugin-v2".to_string();
    let registered = h.registry.register(&new_request, None).await;
    assert!(registered.accepted, "0.2.0 注册应通过：{registered:?}");
    (old, new, new_request, registered)
}

/// **只暴露最新登记版本的工具：旧版本即使实例还在线，它独有的工具也不下发。**
///
/// 调用路由不指定版本时永远打最新登记版本（`Registry::select_instance`），旧版本
/// 独有的工具路由送不到——暴露了只会「看得见调不着」。面上口径与路由口径必须
/// 一致，这是 `online_tools` 新语义（见 `hub_store::plugins`）的锁行为测试。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 只暴露最新登记版本的工具(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;
    let (_old, _new, _, _) = 注册同名双版本都在线(&h).await;

    let names = tool_names(&h, &session).await;
    assert!(
        names.contains(&"dup-plugin__echo".to_string()),
        "最新版本的共名工具应在面上：{names:?}"
    );
    assert!(
        !names.contains(&"dup-plugin__legacy".to_string()),
        "旧版本独有的工具不该下发：{names:?}"
    );
}

/// **最新版本下线后，整个插件的工具从面上消失——即使旧版本实例还在线也不回退。**
///
/// 与调用路由一致：不指定版本的调用永远找最新登记版本，它没了实例就是
/// `NoHealthyInstance`，面上留着一堆调不通的工具只会误导 agent。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 最新版本下线后不回退到旧版本的工具(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;
    let (_old, new, new_request, registered) = 注册同名双版本都在线(&h).await;

    h.registry
        .unregister(
            &new_request.instance_id,
            &registered.state_token,
            "测试主动下线",
        )
        .await
        .expect("注销应成功");
    drop(new);

    let names = tool_names(&h, &session).await;
    assert!(
        !names.iter().any(|n| n.starts_with("dup-plugin__")),
        "最新版本下线后不该回退到旧版本的工具：{names:?}"
    );
}

/// **经聚合工具调用要真的打到插件，并带上工具名**。
///
/// 工具名走 `meta["hub.tool"]`：插件靠它区分自己被调的是哪个工具——同一个插件
/// 声明多个工具时，光看载荷分不出来。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 经聚合工具调用会打到插件并带上工具名(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;

    let fixture = kit::start(with_one_tool()).await;
    assert!(register(&fixture, &h.registry).await);

    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "echo-plugin__echo",
                "arguments": { "a": 7 }
            }
        }),
    )
    .await;
    assert!(
        value.get("error").is_none(),
        "聚合工具的调用不该回 JSON-RPC 错误——HTTP 200 不代表工具跑通了：{value}"
    );

    let seen = fixture.last_envelope().expect("插件应收到信封");
    assert_eq!(
        seen.meta.get("hub.tool").map(String::as_str),
        Some("echo"),
        "工具短名要走 meta 带过去，插件才知道被调的是哪个工具"
    );
    // 闸门关、无登录态：绝不注入空壳键——插件以「缺键」识别匿名调用
    assert!(
        seen.meta.get("hub.mas_token").is_none(),
        "无登录态时不得注入 hub.mas_token：{:?}",
        seen.meta
    );

    // 载荷就是 arguments 本身——不掺中台的约定字段，两种调用方式的载荷形状一致
    let payload =
        hub_proto::decode_payload(seen.payload.as_ref().expect("应有载荷")).expect("载荷应是 JSON");
    assert_eq!(payload["a"], json!(7), "参数应原样传给插件");
}

// ---------------------------------------------------------------- 端点接线

/// 往 `/mcp` 发一条报文。`session` 为空时不带会话头——握手的第一发就是这样。
async fn mcp_post(app: &Router, session: Option<&str>, body: Value) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        // 服务端要求 `Host` 头，oneshot 没有真实连接，得显式带上。真实客户端（含经 nginx
        // 转发的）都会带，nginx 侧我们配了 proxy_set_header Host $host。
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(session) = session {
        builder = builder.header("mcp-session-id", session);
    }

    let request = builder
        .body(Body::from(serde_json::to_vec(&body).expect("序列化失败")))
        .expect("构造请求失败");

    app.clone().oneshot(request).await.expect("请求失败")
}

/// 发一条 MCP **请求**，返回 (会话 id, JSON-RPC 报文)。
///
/// 客户端这一侧自己写，不引 rmcp 的客户端传输（见文件头注释）。协议上有几处要自己照应：
/// 响应可能是 SSE（`data:` 行）或裸 JSON，两种都收；会话 id 在 `initialize` 的**响应头**
/// 里（`mcp-session-id`），后续请求必须原样带上。
async fn mcp_request(app: &Router, session: Option<&str>, body: Value) -> (Option<String>, Value) {
    let response = mcp_post(app, session, body).await;
    let status = response.status();
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应失败")
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();

    assert_eq!(status, StatusCode::OK, "MCP 端点应可应答，实际响应: {text}");

    let payload = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .find(|chunk| chunk.starts_with('{'))
        .unwrap_or(&text);
    let value = serde_json::from_str(payload)
        .unwrap_or_else(|err| panic!("响应不是可解析的 JSON-RPC 报文（{err}），原始响应: {text}"));

    (session_id, value)
}

/// 发一条 MCP **通知**：服务端收下即回 202，没有报文可解。
async fn mcp_notify(app: &Router, session: &str, body: Value) {
    let status = mcp_post(app, Some(session), body).await.status();
    assert!(status.is_success(), "通知应被收下，实际状态码: {status}");
}

/// 走完一次握手（initialize → notifications/initialized），返回后续请求要带的会话 id。
///
/// agent 真实接入就是这个顺序：先握手拿会话，再列工具、调工具。
async fn mcp_handshake(app: &Router) -> String {
    let (session, value) = mcp_request(
        app,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1.0" }
            }
        }),
    )
    .await;
    assert!(
        value["result"]["serverInfo"]["name"].is_string(),
        "握手应返回 serverInfo，实际响应: {value}"
    );

    let session = session.expect("initialize 的响应头里应带 mcp-session-id，后续请求靠它续会话");

    mcp_notify(
        app,
        &session,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    session
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn mcp_端点响应_initialize(pool: PgPool) {
    let h = harness(Store::from_pool(pool));

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0.1.0" }
        }
    });

    let (session, value) = mcp_request(&h.app, None, body).await;

    assert_eq!(
        value["result"]["serverInfo"]["name"], "plugin-hub",
        "agent 要能认出是中台在应答，而不是 rmcp 的默认库名"
    );
    assert!(
        session.is_some(),
        "initialize 的响应头里应带 mcp-session-id，后续请求靠它续会话"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn mcp_端点列出_invoke_plugin(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;

    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;

    let tools = value["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list 应返回工具数组，实际: {value}"));
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();

    // agent 就是靠这份清单发现能力的：不在清单里，等于没有这个能力
    assert!(
        names.contains(&"invoke_plugin"),
        "tools/list 里应有 invoke_plugin（agent 调插件的唯一入口），实际: {names:?}"
    );

    // 光有名字不够：参数 schema 得能让 agent 拼出调用
    let invoke = tools
        .iter()
        .find(|tool| tool["name"] == "invoke_plugin")
        .expect("上面断言过存在");
    let required = invoke["inputSchema"]["required"]
        .as_array()
        .unwrap_or_else(|| panic!("invoke_plugin 应声明必填参数，实际: {invoke}"));
    for field in ["plugin", "payload"] {
        assert!(
            required.iter().any(|name| name == field),
            "invoke_plugin 的 schema 应把 {field} 列为必填，实际: {required:?}"
        );
    }
}

/// 经 MCP 协议真调一次插件——agent 的真实路径。
///
/// 这条覆盖的是「工具名能不能被路由到实现、参数能不能解出来、结果能不能回给 caller」，
/// 也就是直接调方法永远走不到的那一段。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn mcp_端点经协议把载荷送进插件(pool: PgPool) {
    let fixture = kit::start_default().await;
    let h = harness(Store::from_pool(pool));
    assert!(register(&fixture, &h.registry).await);
    let session = mcp_handshake(&h.app).await;

    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "invoke_plugin",
                "arguments": {
                    "plugin": "echo-plugin",
                    "payload": { "orderId": "SO-9", "qty": 2 },
                    "message_id": "msg-42"
                }
            }
        }),
    )
    .await;

    assert!(
        value["error"].is_null(),
        "调用不该走成协议错误，实际响应: {value}"
    );
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("工具应返回文本内容，实际: {value}"));
    let out: Value = serde_json::from_str(text).expect("工具应返回 JSON 文本");

    assert_eq!(out["status"], "handled");
    assert_eq!(out["plugin"], "echo-plugin");
    assert_eq!(out["payload"], json!({"orderId": "SO-9", "qty": 2}));
    assert_eq!(fixture.handled_count(), 1, "插件体应真被执行过一次");
    assert_eq!(
        fixture.last_envelope().expect("插件应收到信封").message_id,
        "msg-42",
        "经协议传下来的 message_id 应原样到插件"
    );
}

// ---------------------------------------------------------------- Host 白名单

/// `initialize` 的请求体。Host 校验发生在协议层之前，用哪个体都行。
fn init_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0.1.0" }
        }
    })
}

/// 与 `mcp_request` 相同，但**能指定 Host 头**，且把状态码一并交回。
///
/// 两处不同各有理由：Host 是因为真实部署里它不是 `localhost` 而是对外域名，
/// 而白名单恰恰只看这个头；状态码是因为这几条用例要断言的正是**状态码本身**
/// （403 还是 200），`mcp_request` 内部那句断言 200 在这里恰好会挡路。
async fn mcp_request_with_host(
    app: &Router,
    session: Option<&str>,
    host: &str,
    body: Value,
) -> (Option<String>, Value, StatusCode) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", host)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(session) = session {
        builder = builder.header("mcp-session-id", session);
    }

    let request = builder
        .body(Body::from(serde_json::to_vec(&body).expect("序列化失败")))
        .expect("构造请求失败");

    let response = app.clone().oneshot(request).await.expect("请求失败");
    let status = response.status();
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应失败")
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();

    let payload = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .find(|chunk| chunk.starts_with('{'))
        .unwrap_or(&text);
    let value = serde_json::from_str(payload).unwrap_or(Value::Null);

    (session_id, value, status)
}

/// **反向代理传的是原始 Host，而默认白名单只认本机**——于是外部 agent 的每一个
/// `/mcp` 请求都被判成 DNS rebinding，回 403。
///
/// 这正是 UAT 上 2026-09-18 撞到的那条：`/hub-api/…` 一切正常，只有 `/mcp` 403；
/// 而在容器里 `curl 127.0.0.1:8095/mcp` 完全正常。也就是说**直连、同机测试、vite
/// 代理都天然通过，这个洞只在跨机部署的形态下露出来**。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 默认白名单会拒掉外部_host(pool: PgPool) {
    let h = harness(Store::from_pool(pool));

    let (_, _, status) =
        mcp_request_with_host(&h.app, None, "hub.example.com", init_body()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "默认只认本机 Host——这是想要的安全默认，也正是线上 /mcp 403 的来源"
    );

    // 同一套服务、同一份请求体，只把 Host 换成本机就通：证明差别确实只在 Host，
    // 而不是端点本身坏了
    let (_, value, status) =
        mcp_request_with_host(&h.app, None, "127.0.0.1:8095", init_body()).await;
    assert_eq!(status, StatusCode::OK, "本机 Host 应放行：{value}");
}

/// 配上对外域名之后，同一个请求要能通。
///
/// 配不带端口的一项即可：nginx 传的是 `$host`（不带端口），而 rmcp 对不带端口的
/// 白名单项放行该域名的**任意端口**。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 配上对外域名后外部_host_可通过(pool: PgPool) {
    let h = harness_with_hosts(
        Store::from_pool(pool),
        Some(vec!["hub.example.com".to_string()]),
    );

    let (_, value, status) =
        mcp_request_with_host(&h.app, None, "hub.example.com", init_body()).await;
    assert_eq!(status, StatusCode::OK, "配置里的域名应放行：{value}");
    assert_eq!(
        value["result"]["serverInfo"]["name"], "plugin-hub",
        "要真的走到协议层，而不是碰巧回了个 200"
    );

    // 白名单不是「配了就全放」：没列出来的域名照样拒
    let (_, _, status) = mcp_request_with_host(&h.app, None, "evil.example.com", init_body()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "白名单之外仍然要拒");
}

/// **「没配」绝不能等于「不设防」**。
///
/// rmcp 把**空白名单**解释成「放行全部 Host」。我们配置面留空表达的是「我没说，
/// 用默认」，所以 `None` 与空 `Vec` 都必须退化成默认，不能让一个空配置把防
/// DNS rebinding 的那一层悄悄关掉。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 空白名单退化为默认而不是放行全部(pool: PgPool) {
    let cases: [Option<Vec<String>>; 2] = [None, Some(Vec::new())];
    for hosts in cases {
        let h = harness_with_hosts(Store::from_pool(pool.clone()), hosts.clone());
        let (_, _, status) =
            mcp_request_with_host(&h.app, None, "evil.example.com", init_body()).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "hosts={hosts:?} 不该被当成「放行全部」"
        );
    }
}

// ---------------------------------------------------------------- 登录闸门

/// **闸门开启而客户端不会弹窗：插件调用拿到拒绝卡，管理工具不受影响。**
///
/// 拒绝卡必须说清「为什么被拒」与「怎么解决」——把不会弹窗静默放行，
/// 闸门就成了摆设；直接 401 又会把问题藏进协议层。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 登录闸门开启时不能弹窗的客户端拿到拒卡(pool: PgPool) {
    let h = harness_with_gate(Store::from_pool(pool), true);
    let session = mcp_handshake(&h.app).await;

    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "invoke_plugin",
                "arguments": {"plugin": "echo-plugin", "payload": {"a": 1}}
            }
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let card: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(card["status"], "rejected", "{card}");
    assert_eq!(card["login"]["required"], true);
    assert_eq!(
        card["next_action"]["tool"], "login",
        "指引卡要指向 login 工具：{card}"
    );

    // 管理类工具不走闸门：读目录仍然可用
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
               "params": {"name": "list_plugins", "arguments": {}}}),
    )
    .await;
    assert!(
        value.get("error").is_none(),
        "管理工具不该被登录闸门拦：{value}"
    );
}

/// **闸门关闭（默认）：一切照旧。**这是给既有调用方的兼容承诺。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 登录闸门关闭时插件调用照常匿名(pool: PgPool) {
    let h = harness_with_gate(Store::from_pool(pool), false);
    let session = mcp_handshake(&h.app).await;

    // 匿名调用要真的走到插件：回显夹具证明整条链路通
    let fixture = kit::start(kit::Behavior::default()).await;
    assert!(register(&fixture, &h.registry).await);

    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "invoke_plugin",
                "arguments": {"plugin": "echo-plugin", "payload": {"a": 1}}
            }
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let delivered: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(delivered["status"], "handled", "{delivered}");
}

// ---------------------------------------------------------------- 插件调用审计

/// **每次插件调用都留痕**：结果带回 trace_id，`get_trace` 按它查到一条 span
/// （谁调的、调了什么、结果、耗时）——「MCP 直接调插件在中台侧无法审计」
/// 的缺口就在这里补上。trace_id 同时写进了信封：插件自己的审计引用同一个 id。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件调用留下审计span(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;

    let fixture = kit::start(kit::Behavior::default()).await;
    assert!(register(&fixture, &h.registry).await);

    // 成功路径
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "invoke_plugin",
                "arguments": {"plugin": "echo-plugin", "payload": {"a": 1}}
            }
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let delivered: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    let trace_id = delivered["trace_id"].as_str().expect("结果应带回 trace_id");

    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
               "params": {"name": "get_trace", "arguments": {"trace_id": trace_id}}}),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let trace: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    let spans = trace["spans"].as_array().expect("应有 spans 数组");
    assert_eq!(spans.len(), 1, "{trace}");
    assert_eq!(spans[0]["name"], "echo-plugin");
    assert_eq!(spans[0]["status"], "ok");
    assert_eq!(spans[0]["attributes"]["caller"], "anonymous", "{trace}");

    // 校验拒绝路径同样留痕（status=rejected）
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "invoke_plugin",
                "arguments": {"plugin": "echo-plugin", "payload": {"a": 1},
                              "message_id": "bad-1"}
            }
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let rejected: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(rejected["status"], "rejected");
    let rejected_trace = rejected["trace_id"]
        .as_str()
        .expect("拒绝卡也应带回 trace_id");

    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
               "params": {"name": "get_trace", "arguments": {"trace_id": rejected_trace}}}),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let trace: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(trace["spans"][0]["status"], "rejected", "{trace}");
}

/// **万能登录路径：不支持弹窗的客户端经 login 工具建立身份。**
///
/// zcode / codex 这类客户端不会应答 elicitation——它们收到指引卡后由
/// agent 调 `login` 工具（账号密码由用户在对话里给），登录一次后全部
/// 插件调用共享身份。这条测试走的就是 zcode 式客户端的完整闭环。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 不支持弹窗的客户端经login工具建立身份后插件调用放行(pool: PgPool) {
    let h = harness_with_gate(Store::from_pool(pool), true);
    let session = mcp_handshake(&h.app).await;

    // auth 替身：以固定身份应答 login（模拟鉴权插件验票成功）
    let auth = kit::start(kit::Behavior {
        plugin_name: "auth".to_string(),
        reject_prefix: None,
        respond_payload: Some(serde_json::json!({
            "authenticated": true,
            "userCode": "u-1",
            "scopes": ["hub:read", "hub:invoke"],
            "expiresAt": 1758000000000_i64
        })),
        ..kit::Behavior::default()
    })
    .await;
    assert!(register(&auth, &h.registry).await);
    let echo = kit::start(kit::Behavior::default()).await;
    assert!(register(&echo, &h.registry).await);

    // 0. 鉴权插件自己不被闸门拦（否则鸡生蛋：登录工具都没法用）
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "invoke_plugin",
                       "arguments": {"plugin": "auth", "payload": {"op": "login"}}}
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let direct: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(
        direct["status"], "handled",
        "鉴权插件必须豁免于登录闸门：{direct}"
    );

    // 1. 首次插件调用：拿到「需要登录」指引卡
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "invoke_plugin",
                       "arguments": {"plugin": "echo-plugin", "payload": {"a": 1}}}
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let card: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(card["login"]["required"], true, "{card}");
    assert_eq!(card["next_action"]["tool"], "login");

    // 2. 调 login 工具建立身份（这就是 zcode 式客户端的路径）
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "login",
                       "arguments": {"account": "maqb11", "password": "secret"}}
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let login: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(login["status"], "handled", "{login}");
    assert_eq!(login["user"], "u-1");
    assert!(login.get("password").is_none(), "回执绝不回显密码");

    // 3. 原调用重试：身份已缓存，直接放行，且插件收到真实身份
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "invoke_plugin",
                       "arguments": {"plugin": "echo-plugin", "payload": {"a": 1}}}
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let delivered: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(delivered["status"], "handled", "{delivered}");
    let seen = echo.last_envelope().expect("插件应收到信封");
    let subject = seen.subject.as_ref().expect("登录后信封必须带身份");
    assert_eq!(subject.id, "u-1");
    assert_eq!(subject.scopes, vec!["hub:read", "hub:invoke"]);
}


/// **插件调用层故障（不可达 / 超时）返回结构化错误卡，而不是 JSON-RPC error。**
///
/// 之前这类失败走 `Err(internal)`，agent 看到的是一层客户端各自渲染的传输层
/// 错误；改成卡之后，它与业务结果同构、可读，中台本身照常服务。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件不可达时返回结构化错误卡(pool: PgPool) {
    let h = harness(Store::from_pool(pool));
    let session = mcp_handshake(&h.app).await;

    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "invoke_plugin",
                "arguments": {"plugin": "no-such-plugin", "payload": {"op": "echo"}}
            }
        }),
    )
    .await;

    // 关键：这是一个**正常完成的工具调用**（不是 JSON-RPC error），载荷是错误卡
    let rpc_error = value.get("error");
    assert!(
        rpc_error.is_none(),
        "调用层故障不该以 JSON-RPC error 逃逸：{value}"
    );
    let text = value["result"]["content"][0]["text"].as_str().expect("结果应为文本");
    let card: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(card["status"], "error", "卡上要说明这是调用层故障：{card}");
    assert_eq!(card["decision"], "reject");
    let message = card["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("插件调用失败") && message.contains("中台不受影响"),
        "卡要说明故障在哪一层、中台是否受影响：{card}"
    );
}

/// **登录态透传：login 换来的 masToken 随信封 meta 带给插件，且绝不进审计。**
///
/// hub 透传登录态、插件不持任何凭证：dc-dict 这类需要下游登录态的插件从
/// `meta["hub.mas_token"]` 取 token 去调 UAT 的 DC 接口——键名是 hub 与
/// 插件的契约。反向约束同样在这里锁定：masToken 只许活在发往插件的那份
/// 信封里，审计 span（PG / get_trace 出口）一律不得出现它。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 登录后插件调用在信封meta里带上mas_token且审计不含它(pool: PgPool) {
    let h = harness_with_gate(Store::from_pool(pool), true);
    let session = mcp_handshake(&h.app).await;

    // auth 替身：login 应答里带 masToken（模拟鉴权插件验票成功）
    let auth = kit::start(kit::Behavior {
        plugin_name: "auth".to_string(),
        reject_prefix: None,
        respond_payload: Some(serde_json::json!({
            "authenticated": true,
            "userCode": "u-1",
            "masToken": "mt_SECRET_TOKEN",
            "expiresAt": 1758000000000_i64
        })),
        ..kit::Behavior::default()
    })
    .await;
    assert!(register(&auth, &h.registry).await);
    let echo = kit::start(kit::Behavior::default()).await;
    assert!(register(&echo, &h.registry).await);

    // 1. 调 login 工具建立身份（login 工具入口；闸门 elicitation 入口汇入同一个缓存）
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "login",
                       "arguments": {"account": "maqb11", "password": "secret"}}
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let login: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(login["status"], "handled", "{login}");
    assert!(
        !text.contains("mt_SECRET_TOKEN"),
        "login 回执只回身份不回 token：{text}"
    );

    // 2. 重试插件调用：信封 meta 里必须带上 masToken（契约键）
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "invoke_plugin",
                       "arguments": {"plugin": "echo-plugin", "payload": {"a": 1}}}
        }),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let delivered: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    assert_eq!(delivered["status"], "handled", "{delivered}");
    let seen = echo.last_envelope().expect("插件应收到信封");
    assert_eq!(
        seen.meta.get("hub.mas_token").map(String::as_str),
        Some("mt_SECRET_TOKEN"),
        "登录态要随信封 meta 透传给插件：{:?}",
        seen.meta
    );

    // 3. 审计脱敏：这条调用的 span 里查不到 token（PG attributes 与 get_trace 出口）
    let trace_id = delivered["trace_id"].as_str().expect("结果应带回 trace_id");
    let (_, value) = mcp_request(
        &h.app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
               "params": {"name": "get_trace", "arguments": {"trace_id": trace_id}}}),
    )
    .await;
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("结果应为文本");
    let trace: serde_json::Value = serde_json::from_str(text).expect("结果应可解析");
    // 正向控制：先确认查到的确实是这条调用（span 名与调用主体都在），
    // 再锁脱敏——否则「span 缺失」会让脱敏断言空转变绿
    assert_eq!(
        trace["spans"][0]["name"], "echo-plugin",
        "应取到这次调用的 span：{trace}"
    );
    assert_eq!(trace["spans"][0]["attributes"]["caller"], "u-1");
    assert!(
        !text.contains("mt_SECRET_TOKEN"),
        "审计 span 不得包含 masToken：{text}"
    );
}

