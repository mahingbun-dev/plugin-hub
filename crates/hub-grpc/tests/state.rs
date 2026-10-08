//! HubState 的集成测试：真 PostgreSQL + 真 Redis。
//!
//! 这一层的价值全在"真的连上去会怎样"——认证、前缀隔离、TTL 到期，任何一条
//! 用 mock 验都等于没验。
//!
//! 每个用例用 `#[sqlx::test]` 拿到**独立数据库**，Redis 走夹具里硬编码的 db 15——
//! 两者都是为了不碰到本机开发库、以及并发运行的中台实例所用的 Redis db。

use hub_proto::v1::{KvDeleteRequest, KvGetRequest, KvKey, KvPutRequest, KvScanRequest};
use redis::AsyncCommands;
use sqlx::PgPool;
use tonic::Request;

mod common;

/// 建一个已注册的实例，返回它的 plugin_name 与凭证。
async fn register_instance(ctx: &common::Ctx, plugin: &str) -> String {
    ctx.register_plugin(plugin, "1.0.0", "http://127.0.0.1:9000")
        .await
}

fn key(ns: &str, k: &str) -> KvKey {
    KvKey {
        namespace: ns.to_string(),
        key: k.to_string(),
    }
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 存取删往返(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token = register_instance(&ctx, "roundtrip").await;
    let mut client = ctx.state_client().await;

    // 不存在时 found=false
    let got = client
        .kv_get(common::with_token(
            Request::new(KvGetRequest {
                key: Some(key("s", "k1")),
            }),
            &token,
        ))
        .await
        .expect("查询不该失败")
        .into_inner();
    assert!(!got.found, "键不存在时 found 必须是 false");

    client
        .kv_put(common::with_token(
            Request::new(KvPutRequest {
                key: Some(key("s", "k1")),
                value: b"hello".to_vec(),
                ttl_seconds: 0,
            }),
            &token,
        ))
        .await
        .expect("写入不该失败");

    let got = client
        .kv_get(common::with_token(
            Request::new(KvGetRequest {
                key: Some(key("s", "k1")),
            }),
            &token,
        ))
        .await
        .expect("查询不该失败")
        .into_inner();
    assert!(got.found, "写入后 found 必须是 true");
    assert_eq!(got.value, b"hello", "值必须原样取回");

    let deleted = client
        .kv_delete(common::with_token(
            Request::new(KvDeleteRequest {
                key: Some(key("s", "k1")),
            }),
            &token,
        ))
        .await
        .expect("删除不该失败")
        .into_inner();
    assert!(deleted.deleted, "删除已存在的键应返回 deleted=true");

    // 删不存在的键不是错误
    let again = client
        .kv_delete(common::with_token(
            Request::new(KvDeleteRequest {
                key: Some(key("s", "k1")),
            }),
            &token,
        ))
        .await
        .expect("删不存在的键不该报错")
        .into_inner();
    assert!(!again.deleted, "删不存在的键应返回 deleted=false");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 空字节的值与不存在的键要能区分(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token = register_instance(&ctx, "emptyval").await;
    let mut client = ctx.state_client().await;

    client
        .kv_put(common::with_token(
            Request::new(KvPutRequest {
                key: Some(key("s", "empty")),
                value: Vec::new(),
                ttl_seconds: 0,
            }),
            &token,
        ))
        .await
        .expect("写入空值不该失败");

    let got = client
        .kv_get(common::with_token(
            Request::new(KvGetRequest {
                key: Some(key("s", "empty")),
            }),
            &token,
        ))
        .await
        .expect("查询不该失败")
        .into_inner();
    assert!(
        got.found,
        "空字节的值是存在的值，found 必须为 true——不能和不存在混为一谈"
    );
    assert!(got.value.is_empty());
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 无凭证与伪造凭证都被拒(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let mut client = ctx.state_client().await;

    // 完全不带 token
    let err = client
        .kv_get(Request::new(KvGetRequest {
            key: Some(key("s", "k")),
        }))
        .await
        .expect_err("无凭证必须被拒");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // 伪造的 token。
    //
    // 必须是可见 ASCII：gRPC metadata 的值只接受这部分字节，用中文会在构造
    // header 时就 panic，验不到认证逻辑本身。
    let err = client
        .kv_get(common::with_token(
            Request::new(KvGetRequest {
                key: Some(key("s", "k")),
            }),
            "forged-token-never-registered",
        ))
        .await
        .expect_err("伪造凭证必须被拒");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 插件读不到别人的键(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token_a = register_instance(&ctx, "plugin-a").await;
    let token_b = register_instance(&ctx, "plugin-b").await;
    let mut client = ctx.state_client().await;

    client
        .kv_put(common::with_token(
            Request::new(KvPutRequest {
                key: Some(key("s", "secret")),
                // 值用中文，顺带验证 bytes 字段上的非 ASCII 原样往返
                value: "A 的数据".as_bytes().to_vec(),
                ttl_seconds: 0,
            }),
            &token_a,
        ))
        .await
        .expect("A 写入不该失败");

    let got = client
        .kv_get(common::with_token(
            Request::new(KvGetRequest {
                key: Some(key("s", "secret")),
            }),
            &token_b,
        ))
        .await
        .expect("B 查询不该失败")
        .into_inner();
    assert!(
        !got.found,
        "B 绝不能读到 A 的数据——前缀必须由中台强制，不能靠插件自觉"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 通配符命名空间被拒(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token = register_instance(&ctx, "wildcard").await;
    let mut client = ctx.state_client().await;

    for bad in ["*", "a*", "a:b", "带中文", ""] {
        let err = client
            .kv_scan(common::with_token(
                Request::new(KvScanRequest {
                    namespace: bad.to_string(),
                    prefix: String::new(),
                    limit: 10,
                }),
                &token,
            ))
            .await
            .expect_err(&format!("namespace={bad:?} 必须被拒"));
        assert_eq!(
            err.code(),
            tonic::Code::InvalidArgument,
            "namespace={bad:?} 应是参数错误而非内部错误"
        );
    }
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 扫描只看到自己的命名空间(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token = register_instance(&ctx, "scanner").await;
    let mut client = ctx.state_client().await;

    for k in ["a-1", "a-2", "b-1"] {
        client
            .kv_put(common::with_token(
                Request::new(KvPutRequest {
                    key: Some(key("s", k)),
                    value: k.as_bytes().to_vec(),
                    ttl_seconds: 0,
                }),
                &token,
            ))
            .await
            .expect("写入不该失败");
    }

    let scanned = client
        .kv_scan(common::with_token(
            Request::new(KvScanRequest {
                namespace: "s".to_string(),
                prefix: "a-".to_string(),
                limit: 100,
            }),
            &token,
        ))
        .await
        .expect("扫描不该失败")
        .into_inner();
    let mut keys: Vec<String> = scanned.entries.iter().map(|e| e.key.clone()).collect();
    keys.sort();
    assert_eq!(keys, vec!["a-1".to_string(), "a-2".to_string()]);
}

/// `KvScan` 是全文件唯一把模式拼成 glob 的地方，也是隔离最容易被后续重构悄悄打破的
/// 地方（比如有人把模式写成 `hub:state:*:{ns}:*`）。所以单独守住它的跨插件隔离。
///
/// 刻意让 B **也往同名 namespace 里写自己的键**：这样 B 的扫描必然有结果，
/// 断言就不是"因为扫描坏了所以扫不到"这种空心通过。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 扫描看不到别人的键(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token_a = register_instance(&ctx, "scanner-a").await;
    let token_b = register_instance(&ctx, "scanner-b").await;
    let mut client = ctx.state_client().await;

    // A 往 namespace "s" 写两个键
    for k in ["x-1", "x-2"] {
        client
            .kv_put(common::with_token(
                Request::new(KvPutRequest {
                    key: Some(key("s", k)),
                    value: k.as_bytes().to_vec(),
                    ttl_seconds: 0,
                }),
                &token_a,
            ))
            .await
            .expect("A 写入不该失败");
    }

    // B 往**同一个** namespace "s" 写自己的键
    client
        .kv_put(common::with_token(
            Request::new(KvPutRequest {
                key: Some(key("s", "y-1")),
                value: b"y-1".to_vec(),
                ttl_seconds: 0,
            }),
            &token_b,
        ))
        .await
        .expect("B 写入不该失败");

    // B 用与 A 完全相同的 namespace 与空前缀扫：只能看到自己的
    let scanned = client
        .kv_scan(common::with_token(
            Request::new(KvScanRequest {
                namespace: "s".to_string(),
                prefix: String::new(),
                limit: 100,
            }),
            &token_b,
        ))
        .await
        .expect("B 扫描不该失败")
        .into_inner();
    let mut keys: Vec<String> = scanned.entries.iter().map(|e| e.key.clone()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["y-1".to_string()],
        "B 只能扫到自己的键——模式里含插件名，glob 匹配必须被锁在自己那一段"
    );

    // prefix 上的通配符同样是绕过手段，必须与 namespace 一视同仁地拒掉
    let err = client
        .kv_scan(common::with_token(
            Request::new(KvScanRequest {
                namespace: "s".to_string(),
                prefix: "*".to_string(),
                limit: 100,
            }),
            &token_b,
        ))
        .await
        .expect_err("prefix 里的通配符必须被拒");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// 前缀必须是**中台拼的**，不是插件传进来的——绕开 API 直接查 Redis 里的字面键名。
///
/// 这是本任务的核心要求，只验 API 语义是验不到的：API 完全可能"看起来隔离"
/// （`KvGet` 到的值对得上），而库里其实躺着一枚按插件自报字段拼出来的键。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 键前缀由中台强制拼出(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token = register_instance(&ctx, "prefixcheck").await;
    let mut client = ctx.state_client().await;

    client
        .kv_put(common::with_token(
            Request::new(KvPutRequest {
                key: Some(key("ns1", "the-key")),
                value: b"v".to_vec(),
                ttl_seconds: 0,
            }),
            &token,
        ))
        .await
        .expect("写入不该失败");

    // 直连 Redis（绕过状态面），键名必须是 hub:state:{反查插件名}:{namespace}:{key}
    let mut conn = ctx.redis.clone();
    let raw: Option<Vec<u8>> = conn
        .get("hub:state:prefixcheck:ns1:the-key")
        .await
        .expect("直连 Redis 取键失败");
    assert_eq!(
        raw.as_deref(),
        Some(b"v".as_slice()),
        "Redis 里的字面键名必须是 hub:state:{{插件名}}:{{namespace}}:{{key}}"
    );

    // 反向确认：没有"插件自报什么就存什么"的裸键——前缀真的被拼上了，不是同名巧合
    let bare: Option<Vec<u8>> = conn.get("ns1:the-key").await.expect("直连 Redis 取键失败");
    assert!(bare.is_none(), "不该存在不带中台前缀的裸键：{bare:?}");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn ttl_到期后取不到(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token = register_instance(&ctx, "ttl").await;
    let mut client = ctx.state_client().await;

    client
        .kv_put(common::with_token(
            Request::new(KvPutRequest {
                key: Some(key("s", "short")),
                value: b"v".to_vec(),
                ttl_seconds: 1,
            }),
            &token,
        ))
        .await
        .expect("写入不该失败");

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let got = client
        .kv_get(common::with_token(
            Request::new(KvGetRequest {
                key: Some(key("s", "short")),
            }),
            &token,
        ))
        .await
        .expect("查询不该失败")
        .into_inner();
    assert!(!got.found, "TTL 到期后必须取不到");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 超过上限的值与扫描条数被拒(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token = register_instance(&ctx, "limits").await;
    let mut client = ctx.state_client().await;

    let err = client
        .kv_put(common::with_token(
            Request::new(KvPutRequest {
                key: Some(key("s", "huge")),
                value: vec![0u8; 1024 * 1024 + 1],
                ttl_seconds: 0,
            }),
            &token,
        ))
        .await
        .expect_err("超过 1MB 的值必须被拒");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    let err = client
        .kv_scan(common::with_token(
            Request::new(KvScanRequest {
                namespace: "s".to_string(),
                prefix: String::new(),
                limit: 1001,
            }),
            &token,
        ))
        .await
        .expect_err("limit 超过硬上限必须被拒");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

// `with_token` 搬到了夹具（`common::with_token`）：它对每个调状态面的用例都一样，
// 而「凭证挂在哪个 metadata 键上」是这一层的契约，散在多处迟早写错一处。
