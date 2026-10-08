//! hub-registry 的集成测试：注册流程跑在真实 PostgreSQL 上。
//!
//! 用 `#[sqlx::test]` 为每个用例独立建库；可达性探测用假的 `PluginProbe`，
//! 这样「地址不可达时不落库」这类断言不需要真的起一个插件进程。
//!
//! 迁移文件归 hub-store 管，这里的测试宏显式指向那份路径——全系统只有一个库，
//! 迁移不该按 crate 分散存放。

use async_trait::async_trait;
use hub_proto::v1::{MessageContract, PluginManifest, RegisterRequest, RejectCode, ToolDecl};
use hub_registry::{PluginProbe, ProbeOutcome, Registry, RegistryConfig};
use hub_store::Store;
use hub_store::plugins;
use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet};
use sqlx::PgPool;

// ---------------------------------------------------------------- 测试夹具

/// 固定结果的探测：单测不关心真实连通性，只关心注册流程对探测结果的反应。
struct FakeProbe(ProbeOutcome);

#[async_trait]
impl PluginProbe for FakeProbe {
    async fn health(&self, _addr: &str) -> ProbeOutcome {
        self.0.clone()
    }
}

fn healthy() -> FakeProbe {
    FakeProbe(ProbeOutcome::Healthy {
        message: "ok".to_string(),
    })
}

fn unreachable() -> FakeProbe {
    FakeProbe(ProbeOutcome::Unreachable {
        message: "connection refused".to_string(),
    })
}

fn registry(store: Store, probe: FakeProbe) -> Registry {
    Registry::new(store, std::sync::Arc::new(probe), RegistryConfig::default())
}

fn str_field(name: &str, number: i32) -> FieldDescriptorProto {
    FieldDescriptorProto {
        name: Some(name.to_string()),
        number: Some(number),
        label: Some(Label::Optional as i32),
        r#type: Some(Type::String as i32),
        ..Default::default()
    }
}

fn int_field(name: &str, number: i32) -> FieldDescriptorProto {
    FieldDescriptorProto {
        r#type: Some(Type::Int32 as i32),
        ..str_field(name, number)
    }
}

fn msg(name: &str, fields: Vec<FieldDescriptorProto>) -> DescriptorProto {
    DescriptorProto {
        name: Some(name.to_string()),
        field: fields,
        ..Default::default()
    }
}

/// 注意：messages 是 `(消息名, 字段)`，包名由参数给出——契约标识是全限定名。
fn descriptor(package: &str, messages: Vec<DescriptorProto>) -> Vec<u8> {
    FileDescriptorSet {
        file: vec![FileDescriptorProto {
            name: Some("plugin.proto".to_string()),
            package: Some(package.to_string()),
            message_type: messages,
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

fn manifest(name: &str, version: &str, produces: &[&str]) -> PluginManifest {
    PluginManifest {
        name: name.to_string(),
        version: version.to_string(),
        description: "测试插件".to_string(),
        owner: "qa".to_string(),
        produces: produces
            .iter()
            .map(|fq| MessageContract {
                fq_name: (*fq).to_string(),
                description: String::new(),
            })
            .collect(),
        ..Default::default()
    }
}

fn request(manifest: PluginManifest, descriptor: Vec<u8>) -> RegisterRequest {
    RegisterRequest {
        plugin_name: manifest.name.clone(),
        version: manifest.version.clone(),
        instance_id: "inst-1".to_string(),
        advertise_addr: "http://10.0.0.1:9000".to_string(),
        manifest: Some(manifest),
        descriptor_set: descriptor,
    }
}

fn rejection_codes(response: &hub_proto::v1::RegisterResponse) -> Vec<i32> {
    response.rejections.iter().map(|r| r.code).collect()
}

/// 一个最小的合法注册请求。
fn valid_request() -> RegisterRequest {
    request(
        manifest("wms-reader", "1.0.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg("OrderCreated", vec![str_field("sku", 1)])],
        ),
    )
}

// ---------------------------------------------------------------- 用例

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 合法插件注册成功并全部落库(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    let response = registry.register(&valid_request(), Some("10.0.0.1")).await;

    assert!(response.accepted, "应接受：{:?}", response.rejections);
    assert_eq!(response.instance_id, "inst-1");
    assert_eq!(response.heartbeat_interval_seconds, 10);
    assert!(response.rejections.is_empty());

    let detail = plugins::describe_plugin(&pool, "wms-reader")
        .await
        .expect("查询失败")
        .expect("插件应已落库");
    assert_eq!(detail.plugin.owner, "qa");
    assert_eq!(detail.versions.len(), 1);

    let contracts = plugins::contracts_of(&pool, detail.versions[0].id)
        .await
        .expect("查契约失败");
    assert_eq!(contracts.len(), 1);
    assert_eq!(contracts[0].fq_name, "wms.v1.OrderCreated");

    let instances = hub_store::instances::list_instances(&pool)
        .await
        .expect("查实例失败");
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0].source_ip.as_deref(), Some("10.0.0.1"));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 地址不可达时拒绝且不落库(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), unreachable());

    let response = registry.register(&valid_request(), None).await;

    assert!(!response.accepted);
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::Unreachable as i32]
    );
    // 关键：不可达时绝不能留下半截数据，否则控制台上会出现一个永远调不通的插件
    assert!(
        plugins::find_plugin(&pool, "wms-reader")
            .await
            .expect("查询失败")
            .is_none()
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn descriptor_无法解析时拒绝(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    let mut req = valid_request();
    req.descriptor_set = vec![0xFF, 0xFF, 0xFF, 0xFF];

    let response = registry.register(&req, None).await;

    assert!(!response.accepted);
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::DescriptorInvalid as i32]
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 未提交_descriptor_且声明了自有类型时被拒(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    let mut req = valid_request();
    req.descriptor_set = Vec::new();

    // 空 descriptor 本身合法（只用 Struct 的插件就是这样），但声明了自有类型就必须提供
    let response = registry.register(&req, None).await;
    assert!(!response.accepted);
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::ManifestInvalid as i32],
        "声明 wms.v1.OrderCreated 却没有 descriptor 提供它"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 只用_struct_载荷的插件没有_descriptor_也能注册(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    // 直接响应 agent 调用的插件就是这样：没有自己的 proto
    let mut m = manifest("struct-only", "1.0.0", &[]);
    m.consumes = vec![MessageContract {
        fq_name: "google.protobuf.Struct".to_string(),
        description: String::new(),
    }];

    let mut req = request(m, Vec::new());
    req.instance_id = "struct-only-1".to_string();

    let response = registry.register(&req, None).await;
    assert!(
        response.accepted,
        "不该因为 descriptor 为空而拒绝：{:?}",
        response.rejections
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 声明的消息类型不存在于_descriptor_时拒绝(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    let req = request(
        manifest("p", "1.0.0", &["wms.v1.编造的"]),
        descriptor(
            "wms.v1",
            vec![msg("OrderCreated", vec![str_field("sku", 1)])],
        ),
    );

    let response = registry.register(&req, None).await;
    assert!(!response.accepted);
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::ManifestInvalid as i32]
    );
    assert!(response.rejections[0].message.contains("编造的"));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件名非法时拒绝(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    let req = request(
        manifest("非法 名字", "1.0.0", &[]),
        descriptor("p", vec![msg("M", vec![])]),
    );

    let response = registry.register(&req, None).await;
    assert!(!response.accepted);
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::ManifestInvalid as i32]
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 新版本新增可选字段放行(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    registry.register(&valid_request(), None).await;

    let mut next = request(
        manifest("wms-reader", "1.1.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg(
                "OrderCreated",
                vec![str_field("sku", 1), str_field("note", 2)],
            )],
        ),
    );
    next.instance_id = "inst-2".to_string();

    let response = registry.register(&next, None).await;
    assert!(
        response.accepted,
        "新增可选字段不该被拦：{:?}",
        response.rejections
    );
    assert!(response.warnings.is_empty(), "纯新增不该产生警告");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 新版本删除字段被拒(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    registry.register(&valid_request(), None).await;

    let mut next = request(
        manifest("wms-reader", "1.1.0", &["wms.v1.OrderCreated"]),
        // 把 sku 删掉，换成另一个字段
        descriptor(
            "wms.v1",
            vec![msg("OrderCreated", vec![str_field("other", 2)])],
        ),
    );
    next.instance_id = "inst-2".to_string();

    let response = registry.register(&next, None).await;
    assert!(!response.accepted);
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::BreakingChange as i32]
    );
    let detail = &response.rejections[0].detail;
    assert!(detail.contains("sku"), "拒绝原因要指出具体字段：{detail}");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 新版本字段改类型被拒(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    registry.register(&valid_request(), None).await;

    let mut next = request(
        manifest("wms-reader", "1.1.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg("OrderCreated", vec![int_field("sku", 1)])],
        ),
    );
    next.instance_id = "inst-2".to_string();

    let response = registry.register(&next, None).await;
    assert!(!response.accepted);
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::BreakingChange as i32]
    );
    assert!(response.rejections[0].detail.contains("string"));
    assert!(response.rejections[0].detail.contains("int32"));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 字段改名放行但返回警告(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    registry.register(&valid_request(), None).await;

    let mut next = request(
        manifest("wms-reader", "1.1.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg("OrderCreated", vec![str_field("itemSku", 1)])],
        ),
    );
    next.instance_id = "inst-2".to_string();

    let response = registry.register(&next, None).await;
    assert!(response.accepted, "编号未变，二进制仍兼容");
    assert_eq!(response.warnings.len(), 1);
    assert!(response.warnings[0].contains("itemSku"));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 同版本重复注册幂等(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    let first = registry.register(&valid_request(), None).await;
    let second = registry.register(&valid_request(), None).await;

    assert!(first.accepted);
    assert!(
        second.accepted,
        "同样的注册应幂等接受：{:?}",
        second.rejections
    );

    let detail = plugins::describe_plugin(&pool, "wms-reader")
        .await
        .expect("查询失败")
        .expect("插件应存在");
    assert_eq!(detail.versions.len(), 1, "不应产生第二个版本");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 同版本但契约变化被拒(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    registry.register(&valid_request(), None).await;

    // 版本号一字不改，但契约里把字段类型换了
    let changed = request(
        manifest("wms-reader", "1.0.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg("OrderCreated", vec![int_field("sku", 1)])],
        ),
    );

    let response = registry.register(&changed, None).await;
    assert!(!response.accepted);
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::VersionConflict as i32]
    );
    assert!(response.rejections[0].detail.contains("升版本号"));

    // 被拒的事实要留在中台侧：控制台不该只看到「实例 0 个」
    // 而看不到「为什么被拒」——那正是本次新增留痕表的全部意义
    let rows = hub_store::rejections::list(&pool, Some("wms-reader"), 10)
        .await
        .expect("查拒绝记录失败");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].code, RejectCode::VersionConflict as i32);
    assert_eq!(rows[0].version, "1.0.0");
    assert_eq!(rows[0].instance_id, "inst-1");
    assert_eq!(rows[0].count, 1);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 被拒后的重试在留痕里累计而不是新增行(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    registry.register(&valid_request(), None).await;

    // 版本号不改、契约变了：插件方的典型反应是每 5 秒重试一次，无限循环
    let changed = request(
        manifest("wms-reader", "1.0.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg("OrderCreated", vec![int_field("sku", 1)])],
        ),
    );
    registry.register(&changed, None).await;
    registry.register(&changed, None).await;
    registry.register(&changed, None).await;

    let rows = hub_store::rejections::list(&pool, Some("wms-reader"), 10)
        .await
        .expect("查拒绝记录失败");
    assert_eq!(rows.len(), 1, "重试必须收敛在同一行里");
    assert_eq!(rows[0].count, 3, "count 应累计重试次数");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 注册成功后旧拒绝记录销案(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    registry.register(&valid_request(), None).await;

    // 先制造一条拒绝（同版本、契约变了），再让插件方「升版本号」重新注册
    let changed = request(
        manifest("wms-reader", "1.0.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg("OrderCreated", vec![int_field("sku", 1)])],
        ),
    );
    assert!(!registry.register(&changed, None).await.accepted);

    // 修复方式 = 升版本号 + 兼容基线的新契约（新增字段，不是改类型——改类型
    // 会被 BREAKING_CHANGE 拦下，那是另一条拒绝路径）
    let fixed = request(
        manifest("wms-reader", "1.0.1", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg(
                "OrderCreated",
                vec![str_field("sku", 1), str_field("note", 2)],
            )],
        ),
    );
    assert!(registry.register(&fixed, None).await.accepted);

    let rows = hub_store::rejections::list(&pool, Some("wms-reader"), 10)
        .await
        .expect("查拒绝记录失败");
    assert!(
        rows.is_empty(),
        "注册成功即销案，旧拒绝不该继续挂在控制台上：{rows:?}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 多版本可共存且能各自解析到实例(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    registry.register(&valid_request(), None).await;
    let mut v2 = request(
        manifest("wms-reader", "1.1.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg(
                "OrderCreated",
                vec![str_field("sku", 1), str_field("note", 2)],
            )],
        ),
    );
    v2.instance_id = "inst-v2".to_string();
    v2.advertise_addr = "http://10.0.0.9:9000".to_string();
    assert!(registry.register(&v2, None).await.accepted);

    let v1_target = registry
        .resolve("wms-reader", Some("1.0.0"))
        .await
        .expect("解析 1.0.0 失败");
    assert_eq!(v1_target.instance_id, "inst-1");
    assert_eq!(v1_target.advertise_addr, "http://10.0.0.1:9000");

    let v2_target = registry
        .resolve("wms-reader", Some("1.1.0"))
        .await
        .expect("解析 1.1.0 失败");
    assert_eq!(v2_target.instance_id, "inst-v2");

    let latest = registry
        .resolve("wms-reader", None)
        .await
        .expect("解析最新失败");
    assert_eq!(latest.version, "1.1.0");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 解析未注册的插件报错(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    assert!(registry.resolve("不存在", None).await.is_err());
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 心跳对已注册实例成功(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    registry.register(&valid_request(), None).await;

    let hb = registry.heartbeat("inst-1").await.expect("心跳失败");
    assert!(hb.accepted);
    assert!(!hb.reregister_required);
    assert_eq!(hb.heartbeat_interval_seconds, 10);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 心跳对未注册实例要求重新注册(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    let hb = registry.heartbeat("从未注册过").await.expect("心跳失败");
    assert!(!hb.accepted);
    assert!(
        hb.reregister_required,
        "实例不在注册表里时必须让插件重新注册，否则掉线后无法自愈"
    );
}

/// **注册被拒的一方退出时，删不掉属主那一行**——这是 `INSTANCE_CONFLICT` 的另一半。
///
/// 撞了 `instance_id` 的第二个插件在**注册**时会被拒（INSTANCE_CONFLICT），因此它手里
/// **没有凭证**；但它退出时照旧会发一次注销（旧版 SDK 就是这么做的）。注销若只按
/// `instance_id` 删行，删掉的就是属主那一行——属主的心跳仍按 `instance_id` 命中、返回
/// accepted，**完全察觉不到自己从注册表里消失了**（实测：auth 重启一次，sql-executor
/// 的工具从 MCP 工具面上全部消失，而它自己的日志停在「已注册到中台」之后再无输出）。
///
/// 拦下这一发的是 registry 里的**空凭证早退**；`delete_instance_with_token` 内部还有
/// 两道（函数开头的判空、SQL 里的 `state_token <> ''`），是防御性冗余——即使调用方忘了
/// 判空也不会误删。store 侧的行为由 `crates/hub-store/tests/store.rs` 直接钉住。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 注册被拒的一方退出时删不掉属主那一行(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    let owner = registry.register(&valid_request(), None).await;
    assert!(owner.accepted, "应接受：{:?}", owner.rejections);
    assert!(!owner.state_token.is_empty(), "注册成功必须下发状态凭证");

    // 另一个插件撞了同一个 instance_id（`request()` 固定用 inst-1）
    let intruder = registry
        .register(
            &request(
                manifest("order-writer", "1.0.0", &["wms.v1.OrderShipped"]),
                descriptor(
                    "wms.v1",
                    vec![msg("OrderShipped", vec![str_field("sku", 1)])],
                ),
            ),
            None,
        )
        .await;
    assert!(
        rejection_codes(&intruder).contains(&(RejectCode::InstanceConflict as i32)),
        "撞 id 的注册必须被拒：{:?}",
        intruder.rejections
    );
    assert!(
        intruder.state_token.is_empty(),
        "被拒的注册不该下发凭证——否则它照样注销得掉属主那一行"
    );

    // 它退出时照旧发注销：旧版插件不带凭证，正是当初删掉属主那一行的路径
    assert!(
        !registry
            .unregister("inst-1", "", "撞 id 的插件优雅退出")
            .await
            .expect("注销失败"),
        "空凭证的注销必须被拒"
    );

    // 属主毫发无损
    let hb = registry.heartbeat("inst-1").await.expect("心跳失败");
    assert!(hb.accepted, "属主的心跳必须仍被接受");
    assert!(!hb.reregister_required);
    assert_eq!(
        hub_store::instances::list_instances(&pool)
            .await
            .expect("查实例失败")
            .len(),
        1,
        "属主的实例行必须还在"
    );
}

/// 拿着**别的行**的凭证去注销，必须被拒。
///
/// 凭证证明的是「你对**这一行**的所有权」，不是「你是个已注册的实例」。少了这层比对，
/// 任何插件只要知道别人的 `instance_id` 就能把对方摘掉。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 凭证与本行不符时注销被拒(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    let first = registry.register(&valid_request(), None).await;
    assert!(first.accepted, "应接受：{:?}", first.rejections);

    let mut second_req = request(
        manifest("order-writer", "1.0.0", &["wms.v1.OrderShipped"]),
        descriptor(
            "wms.v1",
            vec![msg("OrderShipped", vec![str_field("sku", 1)])],
        ),
    );
    second_req.instance_id = "inst-2".to_string();
    let second = registry.register(&second_req, None).await;
    assert!(second.accepted, "应接受：{:?}", second.rejections);

    // 拿 inst-2 的凭证去注销 inst-1
    assert!(
        !registry
            .unregister("inst-1", &second.state_token, "拿错了凭证")
            .await
            .expect("注销失败"),
        "凭证与要注销的那一行不符时必须拒绝"
    );

    // 两行都还在
    assert!(
        registry
            .heartbeat("inst-1")
            .await
            .expect("心跳失败")
            .accepted
    );
    assert_eq!(
        hub_store::instances::list_instances(&pool)
            .await
            .expect("查实例失败")
            .len(),
        2
    );

    // 换回它自己的凭证才注销得掉
    assert!(
        registry
            .unregister("inst-1", &first.state_token, "带对了凭证")
            .await
            .expect("注销失败"),
        "凭证正确时应注销成功"
    );
    assert_eq!(
        hub_store::instances::list_instances(&pool)
            .await
            .expect("查实例失败")
            .len(),
        1
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 注销后心跳要求重新注册且解析不到实例(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    let token = registry.register(&valid_request(), None).await.state_token;

    assert!(
        registry
            .unregister("inst-1", &token, "优雅退出")
            .await
            .expect("注销失败")
    );
    assert!(
        !registry
            .unregister("inst-1", &token, "重复注销")
            .await
            .expect("注销失败"),
        "重复注销应返回 false"
    );

    let hb = registry.heartbeat("inst-1").await.expect("心跳失败");
    assert!(hb.reregister_required);

    // 版本定义仍在，但没有可用实例
    assert!(registry.resolve("wms-reader", None).await.is_err());
    assert!(
        plugins::describe_plugin(&pool, "wms-reader")
            .await
            .expect("查询失败")
            .is_some(),
        "插件定义必须保留"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 摘除心跳超时的实例(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    // 把超时阈值压到 0，注册后立刻就会被判定为过期
    let registry = Registry::new(
        store.clone(),
        std::sync::Arc::new(healthy()),
        RegistryConfig {
            heartbeat_interval_seconds: 1,
            stale_after: std::time::Duration::from_secs(0),
        },
    );

    registry.register(&valid_request(), None).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let removed = registry.sweep_stale().await.expect("摘除失败");
    assert_eq!(removed, vec!["inst-1".to_string()]);

    let instances = hub_store::instances::list_instances(&pool)
        .await
        .expect("查实例失败");
    assert!(instances.is_empty());
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 多个副本时选到健康实例(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    registry.register(&valid_request(), None).await;
    let mut second = valid_request();
    second.instance_id = "inst-2".to_string();
    second.advertise_addr = "http://10.0.0.2:9000".to_string();
    assert!(registry.register(&second, None).await.accepted);

    let target = registry
        .resolve("wms-reader", None)
        .await
        .expect("解析失败");
    assert!(
        target.instance_id == "inst-1" || target.instance_id == "inst-2",
        "应选到某个健康副本，实际 {}",
        target.instance_id
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 工具声明落库并可按版本查询(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    let mut req = valid_request();
    if let Some(m) = req.manifest.as_mut() {
        m.tools = vec![
            ToolDecl {
                name: "query_order".to_string(),
                description: "查单".to_string(),
                input_schema_json: r#"{"type":"object"}"#.to_string(),
                requires_approval: false,
            },
            ToolDecl {
                name: "publish_flow".to_string(),
                description: "发布编排".to_string(),
                input_schema_json: r#"{"type":"object"}"#.to_string(),
                requires_approval: true,
            },
        ];
    }

    assert!(registry.register(&req, None).await.accepted);

    let detail = plugins::describe_plugin(&pool, "wms-reader")
        .await
        .expect("查询失败")
        .expect("插件应存在");
    let tools = plugins::tools_of(&pool, detail.versions[0].id)
        .await
        .expect("查工具失败");

    assert_eq!(tools.len(), 2);
    assert!(
        tools
            .iter()
            .any(|t| t.name == "publish_flow" && t.requires_approval)
    );
    assert!(
        tools
            .iter()
            .any(|t| t.name == "query_order" && !t.requires_approval)
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 注册成功时下发状态凭证(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    let registry = registry(store.clone(), healthy());

    let response = registry.register(&valid_request(), None).await;

    assert!(response.accepted, "注册应被接受：{:?}", response.rejections);
    assert!(
        !response.state_token.is_empty(),
        "注册成功必须下发状态凭证，否则插件无法使用 HubState"
    );

    let stored = hub_store::instances::find_state_token(store.pool(), &response.instance_id)
        .await
        .expect("查凭证应成功")
        .expect("凭证应已落库");
    assert_eq!(
        stored, response.state_token,
        "响应里的凭证必须与落库的一致——插件拿它当唯一身份依据"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 注册被拒时不下发凭证(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    registry.register(&valid_request(), None).await;

    // 版本号一字不改、契约却换了字段类型——落到拒绝分支，不能碰 upsert_instance
    let changed = request(
        manifest("wms-reader", "1.0.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg("OrderCreated", vec![int_field("sku", 1)])],
        ),
    );

    let response = registry.register(&changed, None).await;

    assert!(!response.accepted, "契约不一致应被拒");
    assert!(
        response.state_token.is_empty(),
        "被拒的注册不能下发凭证——否则等于给未通过校验的实例发了身份"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn instance_id_被别的插件占用时拒绝且不动原实例(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    let registry = registry(store.clone(), healthy());

    let first = registry.register(&valid_request(), None).await;
    assert!(first.accepted, "首个注册应通过：{:?}", first.rejections);
    let token_before = first.state_token.clone();

    // 另一个插件冒用同一个 instance_id。SDK 的缺省值是「主机名-PID」，在 host 网络下
    // 多个容器会拿到同一个主机名与 PID 1，这种撞车是现实会发生的。
    let mut intruder = valid_request();
    intruder.plugin_name = "order-reader".to_string();
    if let Some(m) = intruder.manifest.as_mut() {
        m.name = "order-reader".to_string();
    }
    // instance_id 有意保持 "inst-1"（那是 wms-reader 的）

    let response = registry.register(&intruder, None).await;

    assert!(!response.accepted, "别的插件不能顶掉已有实例行");
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::InstanceConflict as i32]
    );
    assert!(response.state_token.is_empty(), "被拒的注册不能下发凭证");

    // 原实例必须毫发无损：行还在、属主没变、凭证没被轮换
    assert_eq!(
        hub_store::instances::instance_owner(store.pool(), "inst-1")
            .await
            .expect("查询属主应成功"),
        Some(("wms-reader".to_string(), "1.0.0".to_string())),
        "原实例的属主不能被改写"
    );
    let token_after = hub_store::instances::find_state_token(store.pool(), "inst-1")
        .await
        .expect("查凭证应成功")
        .expect("凭证应还在");
    assert_eq!(
        token_after, token_before,
        "原实例的凭证不能被顶掉者轮换——轮换会让原插件静默失去状态访问能力"
    );

    // 顶掉者不该留下任何痕迹
    assert!(
        plugins::find_plugin(store.pool(), "order-reader")
            .await
            .expect("查询应成功")
            .is_none(),
        "被拒的注册不该建出插件定义"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn instance_id_为空或超长时拒绝(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());

    let mut blank = valid_request();
    blank.instance_id = "   ".to_string();
    let response = registry.register(&blank, None).await;
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::InstanceConflict as i32],
        "空白 instance_id 必须被拒：{:?}",
        response.rejections
    );

    let mut huge = valid_request();
    huge.instance_id = "x".repeat(257);
    let response = registry.register(&huge, None).await;
    assert_eq!(
        rejection_codes(&response),
        vec![RejectCode::InstanceConflict as i32],
        "超长 instance_id 必须被拒：{:?}",
        response.rejections
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 同一插件换版本沿用同一_instance_id_放行(pool: PgPool) {
    let registry = registry(Store::from_pool(pool.clone()), healthy());
    registry.register(&valid_request(), None).await;

    // 进程原地升级：instance_id 不变（SDK 缺省是「主机名-PID」），只是版本号变了。
    // 这也正是上面那条「属主是别的插件才拒」必须按插件名而不是 (插件名, 版本) 判的原因
    // ——按版本判会把每一次原地升级都拦死。
    let mut upgrade = request(
        manifest("wms-reader", "1.1.0", &["wms.v1.OrderCreated"]),
        descriptor(
            "wms.v1",
            vec![msg(
                "OrderCreated",
                vec![str_field("sku", 1), str_field("note", 2)],
            )],
        ),
    );
    upgrade.instance_id = "inst-1".to_string();

    let response = registry.register(&upgrade, None).await;
    assert!(
        response.accepted,
        "同一插件换版本沿用同一 instance_id 是升级，必须放行：{:?}",
        response.rejections
    );
}
