//! 插件注册面的集成测试：**经真实 gRPC** 调，不是直接调 `Registry`。
//!
//! 这个文件存在的唯一理由是 wire 层：`UnregisterRequest.state_token` 是本次新增的
//! 字段，而它的整条链路是「tonic 解码 → handler 透传 → registry 判定」。只调
//! `Registry::unregister` 的测试（`hub-registry`、`hub-mcp` 里那些）**绕过了前两段**
//! ——把 `crates/hub-grpc/src/lib.rs` 里那句 `&req.state_token` 换成 `""`，它们照样
//! 全绿（变异验证过）。于是「新字段到底有没有真的过线」只有编译器在守。
//!
//! 用真 PostgreSQL；Redis 与别的用例同源，走夹具硬编码的 db 15（见 `common`）。

// 夹具是注册面/状态面几个测试文件**共享**的，每个文件按需取用其中一部分。本文件只要
// `setup` 与 `Ctx.addr`，用不到 `state_client` / `with_token`——它们的 dead_code 判定
// 是按测试目标算的，所以会在这里报出来。放行在 mod 这一层，而不是去公共夹具里逐个
// 加 allow：那会把「谁用得到什么」散进公共文件，每加一个测试文件都得回去改它。
//
// 用 `expect` 而不是 `allow`：本文件哪天用上了这两项，这条放行就成了多余的，
// `expect` 会报 unfulfilled 而不是继续沉在那里——CI 的 `-D warnings` 正好逼着清理。
#[expect(dead_code)]
mod common;

use hub_proto::v1::plugin_registry_client::PluginRegistryClient;
use hub_proto::v1::{HeartbeatRequest, UnregisterRequest};
use sqlx::PgPool;
use tonic::transport::Channel;

async fn registry_client(ctx: &common::Ctx) -> PluginRegistryClient<Channel> {
    PluginRegistryClient::connect(ctx.addr.clone())
        .await
        .expect("连注册面失败")
}

/// 实例还在不在：经 gRPC 问心跳，**不绕回 store 直查**——直查就跳出了要验的那一段线。
///
/// 心跳 `accepted=false` 正是「实例不在注册表里」的中台语义（同
/// `hub_registry` 的 `心跳对未注册实例要求重新注册`）。
async fn alive(client: &mut PluginRegistryClient<Channel>, instance_id: &str) -> bool {
    client
        .heartbeat(HeartbeatRequest {
            instance_id: instance_id.to_string(),
        })
        .await
        .expect("心跳调用失败")
        .into_inner()
        .accepted
}

/// 带着注册时下发的凭证注销，实例应被摘掉。
///
/// 这条同时守着「字段过线」：把 handler 里透传的 `req.state_token` 丢掉，中台收到
/// 空凭证会拒绝，`alive` 就是 `true`，断言立刻失败。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 注销带对凭证能摘掉实例(pool: PgPool) {
    let ctx = common::setup(pool).await;
    let token = ctx
        .register_plugin("unreg-ok", "1.0.0", "http://127.0.0.1:9000")
        .await;
    assert!(
        !token.is_empty(),
        "注册成功必须下发凭证，否则这个用例什么也证明不了"
    );
    let mut client = registry_client(&ctx).await;
    assert!(alive(&mut client, "unreg-ok-i1").await, "注册后应在线");

    client
        .unregister(UnregisterRequest {
            instance_id: "unreg-ok-i1".to_string(),
            reason: "测试优雅退出".to_string(),
            state_token: token,
        })
        .await
        .expect("注销调用本身不该失败");

    assert!(
        !alive(&mut client, "unreg-ok-i1").await,
        "凭证正确时应当摘掉实例"
    );
}

/// 不带凭证的注销（旧版插件的行为）必须什么都不删。
///
/// 这是线上那条事故路径：撞了 `instance_id` 的插件从没注册成功、手里没有凭证，
/// 退出时照旧发一发注销，而中台按 `instance_id` 删掉了**属主**那一行。
///
/// 拒绝是**业务上的**，不是 RPC 错误——插件那边不该因为注销被拒而报错退出。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 注销没带凭证摘不掉实例(pool: PgPool) {
    let ctx = common::setup(pool).await;
    ctx.register_plugin("unreg-noauth", "1.0.0", "http://127.0.0.1:9000")
        .await;
    let mut client = registry_client(&ctx).await;

    let resp = client
        .unregister(UnregisterRequest {
            instance_id: "unreg-noauth-i1".to_string(),
            reason: "旧版插件优雅退出".to_string(),
            state_token: String::new(),
        })
        .await;

    resp.expect("注销调用不该以 RPC 错误失败——拒绝是业务上的");

    assert!(
        alive(&mut client, "unreg-noauth-i1").await,
        "没带凭证的注销必须什么都不删——正是这条路径删掉了别的插件的行"
    );
}
