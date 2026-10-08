//! `HubState.Publish` 的集成测试：真 PostgreSQL + 真 Redis + 真总线。
//!
//! 这一层验的全是「真的连上去会怎样」——防环看的是链上累积的名字，限额记在 Redis
//! 上，投递落在 Stream 里。任何一条用 mock 验都等于没验。
//!
//! 每个用例用 `#[sqlx::test]` 拿独立数据库，Redis 走夹具硬编码的 db 15
//! ——两者都是为了不碰到本机开发库与并发运行的中台实例。

use hub_proto::v1::{Envelope, PublishRequest};
use hub_store::flows::{self, DraftInput};
use sqlx::PgPool;
use tonic::Request;

mod common;

/// 建一个**已发布**的 flow 当投递目标。
///
/// `enqueue` 要求目标已发布（只存草稿不算），所以光 `upsert_draft` 不够——
/// 「未发布时拒绝」那条用例要的正是另一种状态。
async fn seed_published_flow(pool: &PgPool, name: &str) {
    flows::upsert_draft(
        pool,
        &DraftInput {
            name,
            description: "publish 用例的目标",
            definition: &serde_json::json!({
                "name": name,
                "nodes": [{ "id": "entry", "plugin": "whatever" }],
                "edges": []
            }),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("存草稿应成功");

    flows::publish_draft(pool, name).await.expect("发布应成功");
}

fn envelope() -> Envelope {
    Envelope {
        message_id: "publish-test-1".to_string(),
        ..Default::default()
    }
}

/// 投给一个已发布的编排：受理，并给出 run_id。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn publish_投递到已发布的编排(pool: PgPool) {
    let ctx = common::setup(pool.clone()).await;
    seed_published_flow(&pool, "downstream").await;

    let token = ctx
        .register_plugin("publisher", "1.0.0", "http://127.0.0.1:9000")
        .await;
    let mut client = ctx.state_client().await;

    let resp = client
        .publish(common::with_token(
            Request::new(PublishRequest {
                target: "downstream".to_string(),
                envelope: Some(envelope()),
            }),
            &token,
        ))
        .await
        .expect("publish 应可调用")
        .into_inner();

    assert!(resp.accepted, "应当受理，拒绝原因：{}", resp.reason);
    assert!(!resp.run_id.is_empty(), "受理后要给出 run_id 供追溯");
}

/// 目标不存在 / 未发布：**当业务结果回，不当 gRPC 错误**。
///
/// 这个区分是有用的：插件据此才知道该改配置还是该重试。混在同一个错误通道里，
/// 它只能看到一句「投递失败」，然后反复重试一个永远不会成立的目标。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn publish_目标不成立时拒绝而不报错(pool: PgPool) {
    let ctx = common::setup(pool.clone()).await;
    // 只存草稿，**不发布**——这正是「未发布不能触发」那条规则
    flows::upsert_draft(
        &pool,
        &DraftInput {
            name: "never-published",
            description: "",
            definition: &serde_json::json!({
                "name": "never-published",
                "nodes": [{ "id": "entry", "plugin": "whatever" }],
                "edges": []
            }),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("存草稿应成功");

    let token = ctx
        .register_plugin("publisher", "1.0.0", "http://127.0.0.1:9000")
        .await;
    let mut client = ctx.state_client().await;

    for target in ["never-published", "根本不存在的编排"] {
        let resp = client
            .publish(common::with_token(
                Request::new(PublishRequest {
                    target: target.to_string(),
                    envelope: Some(envelope()),
                }),
                &token,
            ))
            .await
            .unwrap_or_else(|err| panic!("{target} 不该是 gRPC 错误，实际：{err}"))
            .into_inner();

        assert!(!resp.accepted, "{target} 不该被受理");
        assert!(!resp.reason.is_empty(), "{target} 的拒绝要带上原因");
    }
}

/// **成环就拒**：链上已经有这个目标了。
///
/// 环检测靠的是随信封传下来的链，不是深度——A→B→A 深度才 2 就已经是环，
/// 只数深度会放过它。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn publish_检测到环就拒绝(pool: PgPool) {
    let ctx = common::setup(pool.clone()).await;
    seed_published_flow(&pool, "downstream").await;

    let token = ctx
        .register_plugin("publisher", "1.0.0", "http://127.0.0.1:9000")
        .await;
    let mut client = ctx.state_client().await;

    // 这次触发链已经走过 a → downstream → b，现在要回到 downstream
    let mut env = envelope();
    env.meta.insert(
        "hub.publish_chain".to_string(),
        "a,downstream,b".to_string(),
    );

    let resp = client
        .publish(common::with_token(
            Request::new(PublishRequest {
                target: "downstream".to_string(),
                envelope: Some(env),
            }),
            &token,
        ))
        .await
        .expect("publish 应可调用")
        .into_inner();

    assert!(
        !resp.accepted,
        "成环必须被拒，否则两个插件能互相触发到天荒地老"
    );
    assert!(
        resp.reason.contains("环"),
        "拒绝原因要点明是环，实际：{}",
        resp.reason
    );
}

/// 触发链过长也拒：挡的是「没有重复节点却无限长」的那种链。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn publish_触发链过长就拒绝(pool: PgPool) {
    let ctx = common::setup(pool.clone()).await;
    seed_published_flow(&pool, "downstream").await;

    let token = ctx
        .register_plugin("publisher", "1.0.0", "http://127.0.0.1:9000")
        .await;
    let mut client = ctx.state_client().await;

    // 8 个互不相同的节点，已经到上限
    let mut env = envelope();
    env.meta.insert(
        "hub.publish_chain".to_string(),
        "f1,f2,f3,f4,f5,f6,f7,f8".to_string(),
    );

    let resp = client
        .publish(common::with_token(
            Request::new(PublishRequest {
                target: "downstream".to_string(),
                envelope: Some(env),
            }),
            &token,
        ))
        .await
        .expect("publish 应可调用")
        .into_inner();

    assert!(!resp.accepted, "链到上限就不该再往下投");
    assert!(
        resp.reason.contains("上限"),
        "原因要说清是撞了哪种限制，实际：{}",
        resp.reason
    );
}

/// 配额打满后拒绝，且**只影响这个插件**。
///
/// 夹具把配额调到 2，所以这条用例只发 3 次——不必真发 61 条消息，
/// 也就不用把「配额是多少」这个实现细节焊进测试。
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn publish_配额打满后拒绝且只影响自己(pool: PgPool) {
    let ctx = common::setup_with_quota(pool.clone(), Some(2)).await;
    seed_published_flow(&pool, "downstream").await;

    let noisy = ctx
        .register_plugin("noisy", "1.0.0", "http://127.0.0.1:9001")
        .await;
    let quiet = ctx
        .register_plugin("quiet", "1.0.0", "http://127.0.0.1:9002")
        .await;
    let client = ctx.state_client().await;

    let publish = |token: &str| {
        let mut client = client.clone();
        let token = token.to_string();
        async move {
            client
                .publish(common::with_token(
                    Request::new(PublishRequest {
                        target: "downstream".to_string(),
                        envelope: Some(envelope()),
                    }),
                    &token,
                ))
                .await
                .expect("publish 应可调用")
                .into_inner()
        }
    };

    // 前两次在额度内
    for i in 1..=2 {
        let resp = publish(&noisy).await;
        assert!(resp.accepted, "第 {i} 次应当受理：{}", resp.reason);
    }

    // 第三次超了
    let resp = publish(&noisy).await;
    assert!(!resp.accepted, "超过配额必须被拒");
    assert!(
        resp.reason.contains("上限"),
        "原因要说清是撞了配额，实际：{}",
        resp.reason
    );

    // **另一个插件不受影响**：配额是按插件维度记账的，一个插件刷不出别人的额度
    let resp = publish(&quiet).await;
    assert!(
        resp.accepted,
        "配额按插件隔离，别的插件不该被连累：{}",
        resp.reason
    );
}
