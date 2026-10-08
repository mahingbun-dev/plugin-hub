//! flow 仓储的集成测试：跑在真实 PostgreSQL 上。
//!
//! 重点在「草稿 → 发布」状态机——它决定生产流量走向，出错的方式又很隐蔽
//! （比如悄悄存在两份草稿、或发布后上一版没留档）。

use hub_store::Store;
use hub_store::flows::{
    self, DraftInput, FlowStateError, find_draft, find_flow, find_published, list_flows,
    list_revisions,
};
use serde_json::json;
use sqlx::PgPool;

fn definition(name: &str, plugin: &str) -> serde_json::Value {
    json!({
        "name": name,
        "description": "",
        "nodes": [{"id": "n1", "plugin": plugin}],
        "edges": []
    })
}

fn draft<'a>(name: &'a str, def: &'a serde_json::Value) -> DraftInput<'a> {
    DraftInput {
        name,
        description: "测试编排",
        definition: def,
        validation: None,
        created_by: "tester",
    }
}

async fn store(pool: PgPool) -> Store {
    Store::from_pool(pool)
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 首次写入草稿会建出_flow(pool: PgPool) {
    let store = store(pool).await;
    let def = definition("f1", "auth");

    let revision = flows::upsert_draft(store.pool(), &draft("f1", &def))
        .await
        .expect("写入失败");

    assert_eq!(revision.revision, 1);
    assert_eq!(revision.status, "draft");
    assert_eq!(revision.created_by, "tester");
    assert!(revision.published_at.is_none());

    let flow = find_flow(store.pool(), "f1")
        .await
        .expect("查询失败")
        .expect("应已建出");
    assert_eq!(flow.published_revision, 0, "还没发布过");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 再次写入草稿就地更新而不是新增修订(pool: PgPool) {
    let store = store(pool).await;
    let first = definition("f1", "auth");
    let second = json!({
        "name": "f1",
        "description": "",
        "nodes": [{"id": "n1", "plugin": "auth"}, {"id": "n2", "plugin": "audit"}],
        "edges": [{"from": "n1", "to": "n2"}]
    });

    let a = flows::upsert_draft(store.pool(), &draft("f1", &first))
        .await
        .expect("首次写入失败");
    let b = flows::upsert_draft(store.pool(), &draft("f1", &second))
        .await
        .expect("二次写入失败");

    assert_eq!(a.id, b.id, "草稿是可改的，不该每次编辑都开新修订");
    assert_eq!(b.definition, second, "内容应被更新");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 发布后草稿转为已发布并更新指针(pool: PgPool) {
    let store = store(pool).await;
    let def = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &def))
        .await
        .expect("写入失败");

    let published = flows::publish_draft(store.pool(), "f1")
        .await
        .expect("发布失败");

    assert_eq!(published.status, "published");
    assert!(published.published_at.is_some(), "发布要有时间戳");

    let flow = find_flow(store.pool(), "f1")
        .await
        .expect("查询失败")
        .expect("应存在");
    assert_eq!(flow.published_revision, published.revision);

    assert!(
        find_draft(store.pool(), flow.id)
            .await
            .expect("查询失败")
            .is_none(),
        "发布后不该还留着草稿"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 发布后再编辑会开出新的一版草稿(pool: PgPool) {
    let store = store(pool).await;
    let v1 = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &v1))
        .await
        .expect("写入失败");
    let published = flows::publish_draft(store.pool(), "f1")
        .await
        .expect("发布失败");

    let v2 = definition("f1", "audit");
    let new_draft = flows::upsert_draft(store.pool(), &draft("f1", &v2))
        .await
        .expect("再次写入失败");

    assert_eq!(
        new_draft.revision,
        published.revision + 1,
        "新草稿的修订号应接在最大值之后"
    );
    assert_eq!(new_draft.status, "draft");

    let flow = find_flow(store.pool(), "f1")
        .await
        .expect("查询失败")
        .expect("应存在");
    let still_published = find_published(store.pool(), flow.id)
        .await
        .expect("查询失败")
        .expect("已发布的那版应还在");
    assert_eq!(
        still_published.revision, published.revision,
        "编辑草稿不该影响已发布的版本"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 发布新版会把上一版留档为_archived(pool: PgPool) {
    let store = store(pool).await;
    let v1 = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &v1))
        .await
        .expect("写入失败");
    let first = flows::publish_draft(store.pool(), "f1")
        .await
        .expect("发布失败");

    let v2 = definition("f1", "audit");
    flows::upsert_draft(store.pool(), &draft("f1", &v2))
        .await
        .expect("写入失败");
    let second = flows::publish_draft(store.pool(), "f1")
        .await
        .expect("发布失败");

    assert!(second.revision > first.revision);

    let flow = find_flow(store.pool(), "f1")
        .await
        .expect("查询失败")
        .expect("应存在");
    let revisions = list_revisions(store.pool(), flow.id)
        .await
        .expect("查询失败");
    assert_eq!(revisions.len(), 2);

    let old = revisions
        .iter()
        .find(|r| r.revision == first.revision)
        .expect("旧版应还在");
    assert_eq!(old.status, "archived", "旧版要留档，但不再是当前生效");
    assert!(old.published_at.is_some(), "它确实曾经发布过");

    let current = find_published(store.pool(), flow.id)
        .await
        .expect("查询失败")
        .expect("应有当前生效版本");
    assert_eq!(current.revision, second.revision);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 没有草稿时发布报错(pool: PgPool) {
    let store = store(pool).await;
    let def = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &def))
        .await
        .expect("写入失败");
    flows::publish_draft(store.pool(), "f1")
        .await
        .expect("首次发布应成功");

    // 已发布，没有新草稿
    let err = flows::publish_draft(store.pool(), "f1")
        .await
        .expect_err("应报错");
    assert!(
        matches!(
            err,
            hub_store::StoreError::Invalid(ref msg) if msg.contains("没有草稿")
        ),
        "应是可读的状态错误而不是数据库故障，实际 {err:?}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 发布不存在的_flow_报错(pool: PgPool) {
    let store = store(pool).await;
    let err = flows::publish_draft(store.pool(), "不存在")
        .await
        .expect_err("应报错");
    assert!(
        matches!(
            err,
            hub_store::StoreError::Invalid(ref msg) if msg.contains("未定义")
        ),
        "实际 {err:?}"
    );
    // 顺带确认这条错误的存在，避免 FlowStateError 被当成死代码
    let _ = FlowStateError::FlowNotFound("x".to_string());
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 放弃草稿后可以重新开始(pool: PgPool) {
    let store = store(pool).await;
    let def = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &def))
        .await
        .expect("写入失败");

    assert!(
        flows::discard_draft(store.pool(), "f1")
            .await
            .expect("放弃失败")
    );
    assert!(
        !flows::discard_draft(store.pool(), "f1")
            .await
            .expect("放弃失败"),
        "没有草稿时应返回 false"
    );

    let flow = find_flow(store.pool(), "f1")
        .await
        .expect("查询失败")
        .expect("应存在");
    assert!(
        find_draft(store.pool(), flow.id)
            .await
            .expect("查询失败")
            .is_none()
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 列表统计修订数与草稿状态(pool: PgPool) {
    let store = store(pool).await;
    let v1 = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &v1))
        .await
        .expect("写入失败");
    flows::publish_draft(store.pool(), "f1")
        .await
        .expect("发布失败");

    let v2 = definition("f1", "audit");
    flows::upsert_draft(store.pool(), &draft("f1", &v2))
        .await
        .expect("写入失败");

    let other = definition("f2", "auth");
    flows::upsert_draft(store.pool(), &draft("f2", &other))
        .await
        .expect("写入失败");

    let list = list_flows(store.pool()).await.expect("列表失败");
    assert_eq!(list.len(), 2);

    let f1 = list.iter().find(|f| f.name == "f1").expect("应有 f1");
    assert_eq!(f1.revision_count, 2);
    assert_eq!(f1.published_revision, 1);
    assert!(f1.has_draft, "f1 有未发布的改动");

    let f2 = list.iter().find(|f| f.name == "f2").expect("应有 f2");
    assert_eq!(f2.revision_count, 1);
    assert_eq!(f2.published_revision, 0, "从未发布");
    assert!(f2.has_draft);
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 删除_flow_级联删掉全部修订(pool: PgPool) {
    let store = store(pool).await;
    let def = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &def))
        .await
        .expect("写入失败");
    flows::publish_draft(store.pool(), "f1")
        .await
        .expect("发布失败");

    assert!(
        flows::delete_flow(store.pool(), "f1")
            .await
            .expect("删除失败")
    );
    assert!(
        !flows::delete_flow(store.pool(), "f1")
            .await
            .expect("删除失败")
    );

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_revisions")
        .fetch_one(store.pool())
        .await
        .expect("查询失败");
    assert_eq!(remaining, 0, "修订应随 flow 级联删除");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 校验结果随草稿一起存下来(pool: PgPool) {
    let store = store(pool).await;
    let def = definition("f1", "auth");
    let validation = json!([{"severity": "Warning", "code": "IsolatedNode"}]);

    let revision = flows::upsert_draft(
        store.pool(),
        &DraftInput {
            name: "f1",
            description: "d",
            definition: &def,
            validation: Some(&validation),
            created_by: "tester",
        },
    )
    .await
    .expect("写入失败");

    // 排障时最常问「当时为什么存不下去」，这份快照就是答案
    assert_eq!(revision.validation, Some(validation));
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 修订号按_flow_各自独立(pool: PgPool) {
    let store = store(pool).await;
    let def1 = definition("f1", "auth");
    let def2 = definition("f2", "auth");

    flows::upsert_draft(store.pool(), &draft("f1", &def1))
        .await
        .expect("写入失败");
    flows::publish_draft(store.pool(), "f1")
        .await
        .expect("发布失败");
    let f1_second = flows::upsert_draft(store.pool(), &draft("f1", &def1))
        .await
        .expect("写入失败");

    let f2_first = flows::upsert_draft(store.pool(), &draft("f2", &def2))
        .await
        .expect("写入失败");

    assert_eq!(f1_second.revision, 2);
    assert_eq!(f2_first.revision, 1, "另一条 flow 的修订号从 1 开始");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 未发布的_flow_可以改名且修订跟着走(pool: PgPool) {
    let store = store(pool).await;
    let def = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &def))
        .await
        .expect("写入失败");

    let renamed = flows::rename_flow(store.pool(), "f1", "order-intake")
        .await
        .expect("改名失败");
    assert_eq!(renamed.name, "order-intake");
    assert_eq!(renamed.published_revision, 0, "改名不改发布状态");

    // 修订挂在 flow 的 id 上，改名后原样跟着走
    let flow = find_flow(store.pool(), "order-intake")
        .await
        .expect("查询失败")
        .expect("新名字应能查到");
    assert!(
        find_flow(store.pool(), "f1")
            .await
            .expect("查询失败")
            .is_none()
    );
    assert!(
        find_draft(store.pool(), flow.id)
            .await
            .expect("查询失败")
            .is_some(),
        "草稿应跟着 flow 走"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 改成同一个名字是幂等的(pool: PgPool) {
    let store = store(pool).await;
    let def = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &def))
        .await
        .expect("写入失败");

    let renamed = flows::rename_flow(store.pool(), "f1", "f1")
        .await
        .expect("同名改名应成功");
    assert_eq!(renamed.name, "f1");
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 发布过的_flow_改名被拒(pool: PgPool) {
    let store = store(pool).await;
    let def = definition("f1", "auth");
    flows::upsert_draft(store.pool(), &draft("f1", &def))
        .await
        .expect("写入失败");
    flows::publish_draft(store.pool(), "f1")
        .await
        .expect("发布失败");

    let err = flows::rename_flow(store.pool(), "f1", "f2")
        .await
        .expect_err("应被拒");
    assert!(
        matches!(
            err,
            hub_store::StoreError::Invalid(ref msg) if msg.contains("已发布过")
        ),
        "实际 {err:?}"
    );
    // 名字没被动过
    assert!(
        find_flow(store.pool(), "f1")
            .await
            .expect("查询失败")
            .is_some()
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 改成已存在的名字被拒(pool: PgPool) {
    let store = store(pool).await;
    flows::upsert_draft(store.pool(), &draft("f1", &definition("f1", "auth")))
        .await
        .expect("写入失败");
    flows::upsert_draft(store.pool(), &draft("f2", &definition("f2", "auth")))
        .await
        .expect("写入失败");

    let err = flows::rename_flow(store.pool(), "f1", "f2")
        .await
        .expect_err("应被拒");
    assert!(
        matches!(
            err,
            hub_store::StoreError::Invalid(ref msg) if msg.contains("f2 已存在")
        ),
        "实际 {err:?}"
    );
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 改名不存在的_flow_报未定义(pool: PgPool) {
    let store = store(pool).await;
    let err = flows::rename_flow(store.pool(), "不存在", "别的名字")
        .await
        .expect_err("应报错");
    assert!(
        matches!(
            err,
            hub_store::StoreError::Invalid(ref msg) if msg.contains("未定义")
        ),
        "实际 {err:?}"
    );
}
