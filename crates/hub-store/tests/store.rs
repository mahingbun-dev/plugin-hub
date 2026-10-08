//! hub-store 的集成测试：跑在真实 PostgreSQL 上。
//!
//! 用 `#[sqlx::test]` 为每个用例创建独立数据库并自动跑迁移——测试之间零干扰，
//! 也不需要手工清表。代价是要求 `DATABASE_URL` 指向一个可 `CREATE DATABASE` 的实例；
//! 用 `bash scripts/test.sh` 运行会自动载入本地 `.env`。

use chrono::{Duration, Utc};
use hub_store::Store;
use hub_store::instances::{self, InstanceView};
use hub_store::model::{ContractRow, PluginInstanceRow};
use hub_store::plugins::{self, NewTool, NewVersion};
use sqlx::PgPool;

const MANIFEST: &[u8] = b"manifest-bytes";
const DESCRIPTOR: &[u8] = b"descriptor-bytes";

fn new_version<'a>(name: &'a str, version: &'a str, produces: &'a [String]) -> NewVersion<'a> {
    NewVersion {
        plugin_name: name,
        description: "测试插件",
        owner: "qa",
        version,
        manifest: MANIFEST,
        descriptor: DESCRIPTOR,
        produces,
        consumes: &[],
        tools: &[],
    }
}

async fn store(pool: PgPool) -> Store {
    Store::from_pool(pool)
}

#[sqlx::test]
async fn 迁移建出全部表(pool: PgPool) {
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    .fetch_all(&pool)
    .await
    .expect("查询表失败");

    let names: Vec<&str> = tables.iter().map(|(t,)| t.as_str()).collect();
    for expected in [
        "plugin_contracts",
        "plugin_instances",
        "plugin_tools",
        "plugin_versions",
        "plugins",
    ] {
        assert!(
            names.contains(&expected),
            "缺少表 {expected}，实际有 {names:?}"
        );
    }
}

#[sqlx::test]
async fn 写入版本会同时落契约与工具(pool: PgPool) {
    let store = store(pool).await;
    let produces = vec!["wms.v1.OrderCreated".to_string()];
    let consumes = vec!["wms.v1.OrderPicked".to_string()];
    let tools = vec![NewTool {
        name: "query_order",
        description: "查单",
        input_schema_json: r#"{"type":"object"}"#,
        requires_approval: false,
    }];

    let result = plugins::upsert_version(
        store.pool(),
        &NewVersion {
            produces: &produces,
            consumes: &consumes,
            tools: &tools,
            ..new_version("wms-reader", "1.0.0", &produces)
        },
    )
    .await
    .expect("写入失败");

    assert!(result.created, "首次写入应视为新建");

    let contracts = plugins::contracts_of(store.pool(), result.row.id)
        .await
        .expect("查契约失败");
    assert_eq!(contracts.len(), 2);
    assert!(
        contracts
            .iter()
            .any(|c| c.direction == ContractRow::PRODUCES && c.fq_name == "wms.v1.OrderCreated")
    );
    assert!(
        contracts
            .iter()
            .any(|c| c.direction == ContractRow::CONSUMES && c.fq_name == "wms.v1.OrderPicked")
    );

    let stored_tools = plugins::tools_of(store.pool(), result.row.id)
        .await
        .expect("查工具失败");
    assert_eq!(stored_tools.len(), 1);
    assert_eq!(stored_tools[0].name, "query_order");
    assert_eq!(stored_tools[0].input_schema_json, r#"{"type":"object"}"#);
}

#[sqlx::test]
async fn 同一版本重复写入不新建也不重复插契约(pool: PgPool) {
    let store = store(pool).await;
    let produces = vec!["wms.v1.OrderCreated".to_string()];

    let first = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &produces))
        .await
        .expect("首次写入失败");
    let second = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &produces))
        .await
        .expect("二次写入失败");

    assert!(first.created);
    assert!(!second.created, "已存在的版本不应被当作新建");
    assert_eq!(first.row.id, second.row.id, "不应产生第二个版本行");

    let contracts = plugins::contracts_of(store.pool(), first.row.id)
        .await
        .expect("查契约失败");
    assert_eq!(contracts.len(), 1, "契约不应被重复插入");
}

#[sqlx::test]
async fn 多版本共存且最新版本可定位(pool: PgPool) {
    let store = store(pool).await;
    let produces = vec!["wms.v1.OrderCreated".to_string()];

    plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &produces))
        .await
        .expect("写入 1.0.0 失败");
    let v2 = plugins::upsert_version(store.pool(), &new_version("p", "2.0.0", &produces))
        .await
        .expect("写入 2.0.0 失败");

    let detail = plugins::describe_plugin(store.pool(), "p")
        .await
        .expect("查询失败")
        .expect("插件应存在");
    assert_eq!(detail.versions.len(), 2, "两个版本应共存");

    let latest = plugins::latest_version(store.pool(), detail.plugin.id)
        .await
        .expect("查最新版本失败")
        .expect("应有最新版本");
    assert_eq!(latest.id, v2.row.id);
    assert_eq!(latest.version, "2.0.0");
}

#[sqlx::test]
async fn 重复注册同一实例不产生重复行并刷新地址(pool: PgPool) {
    let store = store(pool).await;
    let produces = vec![];
    let version = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &produces))
        .await
        .expect("写入失败");

    let first = instances::upsert_instance(
        store.pool(),
        version.row.id,
        "inst-1",
        "http://10.0.0.1:9000",
        Some("10.0.0.1"),
    )
    .await
    .expect("注册失败");

    let second = instances::upsert_instance(
        store.pool(),
        version.row.id,
        "inst-1",
        "http://10.0.0.2:9000",
        Some("10.0.0.2"),
    )
    .await
    .expect("重注册失败");

    assert_eq!(first.id, second.id, "同 instance_id 应更新而非新增");
    assert_eq!(second.advertise_addr, "http://10.0.0.2:9000");
    assert_eq!(second.source_ip.as_deref(), Some("10.0.0.2"));

    let all = instances::list_instances(store.pool())
        .await
        .expect("列表失败");
    assert_eq!(all.len(), 1);
}

#[sqlx::test]
async fn 心跳对不存在的实例返回假(pool: PgPool) {
    let store = store(pool).await;
    let hit = instances::touch_heartbeat(store.pool(), "不存在的实例", Utc::now())
        .await
        .expect("心跳失败");
    assert!(!hit, "实例不在注册表里时必须返回 false，插件据此重新注册");
}

#[sqlx::test]
async fn 心跳续期后不会被摘除(pool: PgPool) {
    let store = store(pool).await;
    let version = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &[]))
        .await
        .expect("写入失败");
    instances::upsert_instance(store.pool(), version.row.id, "inst-1", "addr", None)
        .await
        .expect("注册失败");

    let now = Utc::now();
    // 先把心跳回拨到过去，模拟它一度掉线
    instances::touch_heartbeat(store.pool(), "inst-1", now - Duration::seconds(30))
        .await
        .expect("回拨失败");
    // 再续期到当前时刻
    assert!(
        instances::touch_heartbeat(store.pool(), "inst-1", now)
            .await
            .expect("心跳失败")
    );

    // 截止时间落在「回拨点」与「续期点」之间：续期过的实例应存活
    let removed = instances::sweep_stale(store.pool(), now - Duration::seconds(10))
        .await
        .expect("摘除失败");
    assert!(
        removed.is_empty(),
        "续期过的不该被摘除，实际摘除 {removed:?}"
    );
}

#[sqlx::test]
async fn 心跳超时的实例被摘除但不影响插件定义(pool: PgPool) {
    let store = store(pool).await;
    let version = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &[]))
        .await
        .expect("写入失败");
    instances::upsert_instance(store.pool(), version.row.id, "stale-1", "addr", None)
        .await
        .expect("注册失败");
    instances::upsert_instance(store.pool(), version.row.id, "fresh-1", "addr", None)
        .await
        .expect("注册失败");

    let now = Utc::now();
    // 插入时的心跳由数据库 now() 写入，两个实例几乎同一时刻，必须显式回拨才能区分
    instances::touch_heartbeat(store.pool(), "stale-1", now - Duration::seconds(30))
        .await
        .expect("回拨失败");
    instances::touch_heartbeat(store.pool(), "fresh-1", now)
        .await
        .expect("心跳失败");

    // 截止时间落在两者之间：只有 stale-1 过期
    let removed = instances::sweep_stale(store.pool(), now - Duration::seconds(10))
        .await
        .expect("摘除失败");
    assert_eq!(removed, vec!["stale-1".to_string()]);

    let remaining: Vec<InstanceView> = instances::list_instances(store.pool())
        .await
        .expect("列表失败");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].instance_id, "fresh-1");

    // 插件与版本定义必须保留：已编排的 flow 不该因为插件掉线就查不到它
    let detail = plugins::describe_plugin(store.pool(), "p")
        .await
        .expect("查询失败")
        .expect("插件定义应保留");
    assert_eq!(detail.versions.len(), 1);
}

#[sqlx::test]
async fn 可按消息类型反查生产方与消费方(pool: PgPool) {
    let store = store(pool).await;
    let produces = vec!["wms.v1.OrderCreated".to_string()];
    let consumes = vec!["wms.v1.OrderCreated".to_string()];

    plugins::upsert_version(store.pool(), &new_version("producer", "1.0.0", &produces))
        .await
        .expect("写入生产方失败");
    plugins::upsert_version(
        store.pool(),
        &NewVersion {
            consumes: &consumes,
            produces: &[],
            ..new_version("consumer", "1.0.0", &[])
        },
    )
    .await
    .expect("写入消费方失败");

    let producers = plugins::versions_by_fq_name(store.pool(), "wms.v1.OrderCreated", "produces")
        .await
        .expect("查生产方失败");
    assert_eq!(
        producers,
        vec![("producer".to_string(), "1.0.0".to_string())]
    );

    let consumers = plugins::versions_by_fq_name(store.pool(), "wms.v1.OrderCreated", "consumes")
        .await
        .expect("查消费方失败");
    assert_eq!(
        consumers,
        vec![("consumer".to_string(), "1.0.0".to_string())]
    );
}

#[sqlx::test]
async fn 插件总览统计版本数与在线实例数(pool: PgPool) {
    let store = store(pool).await;
    let v1 = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &[]))
        .await
        .expect("写入失败");
    plugins::upsert_version(store.pool(), &new_version("p", "2.0.0", &[]))
        .await
        .expect("写入失败");

    instances::upsert_instance(store.pool(), v1.row.id, "i1", "a", None)
        .await
        .expect("注册失败");
    instances::upsert_instance(store.pool(), v1.row.id, "i2", "a", None)
        .await
        .expect("注册失败");

    let overview = plugins::list_plugins(store.pool()).await.expect("总览失败");
    assert_eq!(overview.len(), 1);
    assert_eq!(overview[0].name, "p");
    assert_eq!(overview[0].version_count, 2);
    assert_eq!(overview[0].instance_count, 2);

    // 摘除一个实例后在线数应下降，版本数不变
    instances::delete_instance(store.pool(), "i1")
        .await
        .expect("删除失败");
    let overview = plugins::list_plugins(store.pool()).await.expect("总览失败");
    assert_eq!(overview[0].instance_count, 1);
    assert_eq!(overview[0].version_count, 2);
}

#[sqlx::test]
async fn 删除实例返回是否命中(pool: PgPool) {
    let store = store(pool).await;
    let version = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &[]))
        .await
        .expect("写入失败");
    instances::upsert_instance(store.pool(), version.row.id, "i1", "a", None)
        .await
        .expect("注册失败");

    assert!(
        instances::delete_instance(store.pool(), "i1")
            .await
            .expect("删除失败")
    );
    assert!(
        !instances::delete_instance(store.pool(), "i1")
            .await
            .expect("删除失败"),
        "重复删除应返回 false"
    );
}

/// 带凭证的注销：**只有凭证与这一行对得上才删**。
///
/// 这是「注销删错行」那条线上最底下的一道。中台侧还有一层（`Registry::unregister`
/// 的空凭证早退），但 store 是删除动作真正发生的地方——它自己认凭证，才不至于
/// 换个调用方就漏。
#[sqlx::test]
async fn 带凭证注销只删得掉凭证相符的那一行(pool: PgPool) {
    let store = store(pool).await;
    let version = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &[]))
        .await
        .expect("写入失败");
    instances::upsert_instance(store.pool(), version.row.id, "i1", "a", None)
        .await
        .expect("注册失败");
    let token = instances::find_state_token(store.pool(), "i1")
        .await
        .expect("查询失败")
        .expect("注册后应当下发凭证");

    // 空凭证：不删。这里钉的是**行为**——函数开头的判空与 SQL 的 `state_token <> ''`
    // 互为冗余，单独去掉任何一道行为都不变，所以不区分是哪一道拦下的。
    assert!(
        !instances::delete_instance_with_token(store.pool(), "i1", "")
            .await
            .expect("注销失败"),
        "空凭证不该删掉任何东西"
    );
    // 凭证不符：不删
    assert!(
        !instances::delete_instance_with_token(store.pool(), "i1", "别的凭证")
            .await
            .expect("注销失败"),
        "凭证不符不该删掉任何东西"
    );
    assert!(
        instances::find_state_token(store.pool(), "i1")
            .await
            .expect("查询失败")
            .is_some(),
        "两次失败的注销之后，那一行必须还在"
    );

    // 凭证相符：删掉
    assert!(
        instances::delete_instance_with_token(store.pool(), "i1", &token)
            .await
            .expect("注销失败"),
        "凭证相符时应删掉"
    );
    assert!(
        instances::find_state_token(store.pool(), "i1")
            .await
            .expect("查询失败")
            .is_none(),
        "删掉之后应当查不到"
    );

    // 行已经不在了：返回 false，不报错
    assert!(
        !instances::delete_instance_with_token(store.pool(), "i1", &token)
            .await
            .expect("注销失败"),
        "行不存在时应返回 false"
    );
}

/// 重新注册轮换凭证之后，**旧凭证立刻删不动了**。
///
/// 这条守着「进程重启视为同一实例、旧凭证作废」的语义：否则一个已经退出的旧进程
/// 还能拿它当年那份凭证，把新进程刚注册好的行删掉。
#[sqlx::test]
async fn 重新注册轮换后旧凭证注销不动(pool: PgPool) {
    let store = store(pool).await;
    let version = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &[]))
        .await
        .expect("写入失败");
    instances::upsert_instance(store.pool(), version.row.id, "i1", "a", None)
        .await
        .expect("注册失败");
    let old_token = instances::find_state_token(store.pool(), "i1")
        .await
        .expect("查询失败")
        .expect("注册后应当下发凭证");

    // 同一个 instance_id 再注册一次 = 进程重启，凭证轮换
    instances::upsert_instance(store.pool(), version.row.id, "i1", "a", None)
        .await
        .expect("重复注册失败");
    let new_token = instances::find_state_token(store.pool(), "i1")
        .await
        .expect("查询失败")
        .expect("重新注册后应当有凭证");
    assert_ne!(old_token, new_token, "重复注册应当轮换凭证");

    assert!(
        !instances::delete_instance_with_token(store.pool(), "i1", &old_token)
            .await
            .expect("注销失败"),
        "旧凭证不该还能删掉这一行"
    );
    assert!(
        instances::find_state_token(store.pool(), "i1")
            .await
            .expect("查询失败")
            .is_some(),
        "用旧凭证试过之后，那一行必须还在"
    );

    assert!(
        instances::delete_instance_with_token(store.pool(), "i1", &new_token)
            .await
            .expect("注销失败"),
        "新凭证应当能删掉"
    );
}

#[sqlx::test]
async fn 查询不存在的插件返回空(pool: PgPool) {
    let store = store(pool).await;
    assert!(
        plugins::find_plugin(store.pool(), "不存在")
            .await
            .expect("查询失败")
            .is_none()
    );
    assert!(
        plugins::describe_plugin(store.pool(), "不存在")
            .await
            .expect("查询失败")
            .is_none()
    );
}

#[sqlx::test]
async fn 同一实例换版本注册会归到新版本下(pool: PgPool) {
    let store = store(pool).await;
    let v1 = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &[]))
        .await
        .expect("写入失败");
    let v2 = plugins::upsert_version(store.pool(), &new_version("p", "2.0.0", &[]))
        .await
        .expect("写入失败");

    instances::upsert_instance(store.pool(), v1.row.id, "i1", "a", None)
        .await
        .expect("注册失败");
    // 插件升级后以同一 instance_id 重新注册
    instances::upsert_instance(store.pool(), v2.row.id, "i1", "a", None)
        .await
        .expect("重注册失败");

    let v2_instances: Vec<PluginInstanceRow> =
        instances::instances_of_version(store.pool(), v2.row.id)
            .await
            .expect("查询失败");
    assert_eq!(v2_instances.len(), 1);
    assert_eq!(v2_instances[0].instance_id, "i1");

    let v1_instances = instances::instances_of_version(store.pool(), v1.row.id)
        .await
        .expect("查询失败");
    assert!(v1_instances.is_empty(), "实例应已迁到新版本下");
}

#[sqlx::test]
async fn 注册时下发凭证且重复注册会轮换(pool: PgPool) {
    let store = store(pool).await;
    let version = plugins::upsert_version(store.pool(), &new_version("auth", "1.0.0", &[]))
        .await
        .expect("建版本应成功");

    let first = instances::upsert_instance(store.pool(), version.row.id, "i-1", "http://a:1", None)
        .await
        .expect("注册应成功");
    let token_a = instances::find_state_token(store.pool(), &first.instance_id)
        .await
        .expect("查凭证应成功")
        .expect("新注册的实例必须有凭证");
    assert!(!token_a.is_empty(), "凭证不能是空串");

    // 重复注册 = 进程重启：凭证必须换掉，旧凭证立即失效
    instances::upsert_instance(store.pool(), version.row.id, "i-1", "http://a:2", None)
        .await
        .expect("重复注册应成功");
    let token_b = instances::find_state_token(store.pool(), "i-1")
        .await
        .expect("查凭证应成功")
        .expect("凭证应仍在");

    assert_ne!(token_a, token_b, "重复注册必须轮换凭证");
    assert_eq!(
        instances::plugin_of_state_token(store.pool(), &token_a)
            .await
            .expect("反查应成功"),
        None,
        "旧凭证必须立即失效"
    );
    assert_eq!(
        instances::plugin_of_state_token(store.pool(), &token_b)
            .await
            .expect("反查应成功")
            .as_deref(),
        Some("auth"),
        "新凭证应能反查到插件名"
    );
}

#[sqlx::test]
async fn 不存在的实例与空凭证都反查不到插件(pool: PgPool) {
    let store = store(pool).await;
    assert_eq!(
        instances::plugin_of_state_token(store.pool(), "从未存在过的凭证")
            .await
            .expect("反查应成功"),
        None
    );
    assert_eq!(
        instances::plugin_of_state_token(store.pool(), "")
            .await
            .expect("空凭证应直接返回而非查库"),
        None
    );
    assert_eq!(
        instances::find_state_token(store.pool(), "不存在的实例")
            .await
            .expect("查询应成功"),
        None
    );
}

/// caller_of_state_token 是 plugin_of_state_token 的加宽版：互调权限校验要拿
/// caller **注册版本**的 manifest 去解析 invokes 声明，所以除了名字还必须带出
/// 版本 id。这条锁住「带出的是实例当前挂在的那个版本」。
#[sqlx::test]
async fn 凭证反查调用方能带出其注册版本的_id(pool: PgPool) {
    let store = store(pool).await;
    let v1 = plugins::upsert_version(store.pool(), &new_version("caller", "1.0.0", &[]))
        .await
        .expect("写入失败");
    let v2 = plugins::upsert_version(store.pool(), &new_version("caller", "2.0.0", &[]))
        .await
        .expect("写入失败");

    instances::upsert_instance(store.pool(), v1.row.id, "i1", "a", None)
        .await
        .expect("注册失败");
    let token = instances::find_state_token(store.pool(), "i1")
        .await
        .expect("查凭证应成功")
        .expect("新注册的实例必须有凭证");

    assert_eq!(
        instances::caller_of_state_token(store.pool(), &token)
            .await
            .expect("反查应成功"),
        Some(("caller".to_string(), v1.row.id)),
        "应反查到插件名与实例注册版本 id"
    );

    // 插件升级重注册：凭证随之轮换，反查结果迁到新版本——与实例归属同进退
    instances::upsert_instance(store.pool(), v2.row.id, "i1", "a", None)
        .await
        .expect("重注册失败");
    let new_token = instances::find_state_token(store.pool(), "i1")
        .await
        .expect("查凭证应成功")
        .expect("凭证应仍在");

    assert_eq!(
        instances::caller_of_state_token(store.pool(), &new_token)
            .await
            .expect("反查应成功"),
        Some(("caller".to_string(), v2.row.id)),
    );
    assert_eq!(
        instances::caller_of_state_token(store.pool(), &token)
            .await
            .expect("反查应成功"),
        None,
        "旧凭证必须与 plugin_of_state_token 同口径立即失效"
    );

    // 边界与 plugin_of_state_token 一致：空凭证直接不查
    assert_eq!(
        instances::caller_of_state_token(store.pool(), "")
            .await
            .expect("空凭证应直接返回而非查库"),
        None
    );
}

/// manifest_of_version 服务互调权限校验：invokes 声明只存在 manifest 字节里。
/// 原样存、原样取，不做任何转码——解码是调用方（拿着 hub-proto）的事。
#[sqlx::test]
async fn 按版本_id_能取回注册时的_manifest_字节(pool: PgPool) {
    let store = store(pool).await;
    let manifest: &[u8] = b"\x0a\x06caller\x12\x051.0.0";
    let version = plugins::upsert_version(
        store.pool(),
        &NewVersion {
            manifest,
            ..new_version("caller", "1.0.0", &[])
        },
    )
    .await
    .expect("写入失败");

    assert_eq!(
        plugins::manifest_of_version(store.pool(), version.row.id)
            .await
            .expect("查询应成功"),
        Some(manifest.to_vec()),
        "取回的必须是注册时提交的原始字节"
    );
    assert_eq!(
        plugins::manifest_of_version(store.pool(), version.row.id + 999)
            .await
            .expect("查询应成功"),
        None,
        "不存在的版本应返回 None"
    );
}

#[sqlx::test]
async fn 按_instance_id_能查到属主(pool: PgPool) {
    let store = store(pool).await;
    let v1 = plugins::upsert_version(store.pool(), &new_version("p", "1.0.0", &[]))
        .await
        .expect("写入失败");
    let v2 = plugins::upsert_version(store.pool(), &new_version("p", "2.0.0", &[]))
        .await
        .expect("写入失败");

    assert_eq!(
        instances::instance_owner(store.pool(), "i1")
            .await
            .expect("查询应成功"),
        None,
        "还没注册过的 instance_id 没有属主"
    );

    instances::upsert_instance(store.pool(), v1.row.id, "i1", "a", None)
        .await
        .expect("注册失败");
    assert_eq!(
        instances::instance_owner(store.pool(), "i1")
            .await
            .expect("查询应成功"),
        Some(("p".to_string(), "1.0.0".to_string()))
    );

    // 同一插件换版本注册：属主随之迁到新版本，插件名不变——这正是注册流程要放行的升级
    instances::upsert_instance(store.pool(), v2.row.id, "i1", "a", None)
        .await
        .expect("重注册失败");
    assert_eq!(
        instances::instance_owner(store.pool(), "i1")
            .await
            .expect("查询应成功"),
        Some(("p".to_string(), "2.0.0".to_string()))
    );
}

// ---------------------------------------------------------------- 插件调用审计

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件调用审计按条件过滤(pool: PgPool) -> Result<(), hub_store::StoreError> {
    let cases: [(&str, &str, &str, &str, serde_json::Value); 4] = [
        (
            "t-1",
            "echo-plugin",
            "ok",
            "sql-executor",
            serde_json::json!({"kind": "plugin-invocation", "plugin": "sql-executor",
                            "tool": "echo", "caller": "maqb11"}),
        ),
        (
            "t-2",
            "echo-plugin",
            "rejected",
            "sql-executor",
            serde_json::json!({"kind": "plugin-invocation", "plugin": "sql-executor",
                            "tool": "echo", "caller": "other"}),
        ),
        (
            "t-3",
            "echo-plugin",
            "ok",
            "auth",
            serde_json::json!({"kind": "plugin-invocation", "plugin": "auth",
                            "tool": "login", "caller": "maqb11"}),
        ),
        (
            "t-flow",
            "flow-node",
            "ok",
            "none",
            serde_json::json!({"flow": "f"}),
        ),
    ];
    let mut spans: Vec<hub_store::spans::NewSpan<'_>> = Vec::new();
    for (trace, name, status, _plugin, attrs) in &cases {
        spans.push(hub_store::spans::NewSpan {
            trace_id: trace,
            span_id: trace,
            parent_span_id: None,
            run_id: None,
            node_id: None,
            name,
            started_at: Utc::now(),
            duration_ms: 12,
            status,
            attributes: Some(attrs),
        });
    }
    for span in &spans {
        hub_store::spans::insert_span(&pool, span).await?;
    }

    fn filter<'a>(
        plugin: Option<&'a str>,
        caller: Option<&'a str>,
        status: Option<&'a str>,
    ) -> hub_store::spans::InvocationAuditFilter<'a> {
        hub_store::spans::InvocationAuditFilter {
            plugin,
            caller,
            status,
            limit: 50,
            offset: 0,
        }
    }

    // 无过滤：只看得到 3 条插件调用（编排 span 没有 kind 标记，不混入）
    let (_, total) =
        hub_store::spans::list_invocation_audits(&pool, &filter(None, None, None)).await?;
    assert_eq!(total, 3);

    // 插件 + 调用者组合过滤
    let (rows, total) = hub_store::spans::list_invocation_audits(
        &pool,
        &filter(Some("sql-executor"), Some("maqb11"), None),
    )
    .await?;
    assert_eq!(total, 1);
    assert_eq!(rows[0].plugin, "sql-executor");
    assert_eq!(rows[0].caller, "maqb11");
    assert_eq!(rows[0].trace_id, "t-1");

    // 状态过滤
    let (_, total) =
        hub_store::spans::list_invocation_audits(&pool, &filter(None, None, Some("rejected")))
            .await?;
    assert_eq!(total, 1);

    Ok(())
}
