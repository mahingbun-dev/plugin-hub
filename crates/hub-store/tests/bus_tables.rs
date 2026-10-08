//! M3 三张表的集成测试：幂等、死信、超限载荷。
//!
//! 跑在真实 PostgreSQL 上（`#[sqlx::test]` 为每个用例开独立库）而不是打桩：这三张表的
//! 关键性质全部由数据库保证——`ON CONFLICT DO NOTHING` 的去重、唯一索引挡住的重复死信、
//! TTL 判定放在 SQL 里的过期语义。脱离数据库这些性质一条都验证不了。

use std::time::Duration;

use chrono::{Duration as Span, Utc};
use hub_store::{dead_letters, idempotency, payloads};
use sqlx::PgPool;

// ------------------------------------------------------------------ 幂等

#[sqlx::test]
async fn 幂等键只有第一次认领成功(pool: PgPool) {
    let expires = Utc::now() + Span::hours(24);

    assert!(
        idempotency::claim(&pool, "node:run-1:a", Some("run-1"), expires)
            .await
            .expect("认领应成功"),
        "第一次见到应当返回 true"
    );
    assert!(
        !idempotency::claim(&pool, "node:run-1:a", Some("run-1"), expires)
            .await
            .expect("认领应成功"),
        "第二次见到必须返回 false——这正是把 at-least-once 变成只生效一次的那个判断"
    );
}

#[sqlx::test]
async fn 幂等键内容不同互不影响(pool: PgPool) {
    let expires = Utc::now() + Span::hours(24);

    assert!(
        idempotency::claim(&pool, "node:run-1:a", None, expires)
            .await
            .unwrap()
    );
    assert!(
        idempotency::claim(&pool, "node:run-1:b", None, expires)
            .await
            .unwrap(),
        "同一个 run 的另一个节点是另一件事，不该被前一个挡住"
    );
    assert!(
        idempotency::claim(&pool, "node:run-2:a", None, expires)
            .await
            .unwrap(),
        "另一个 run 的同一个节点同样各算各的"
    );
}

#[sqlx::test]
async fn 并发认领同一个键只有一个成功(pool: PgPool) {
    let expires = Utc::now() + Span::hours(24);

    let mut handles: Vec<tokio::task::JoinHandle<bool>> = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            idempotency::claim(&pool, "node:run-c:a", Some("run-c"), expires)
                .await
                .expect("认领应成功")
        }));
    }

    let mut won = 0;
    for handle in handles {
        if handle.await.expect("任务不应 panic") {
            won += 1;
        }
    }

    assert_eq!(
        won, 1,
        "「先查再插」会在这里失手：两个消费者同时查到「没见过」就双双放行。\
         用 ON CONFLICT DO NOTHING 的 rows_affected 才不会有这个窗口"
    );
}

#[sqlx::test]
async fn 过期的幂等键会被清理(pool: PgPool) {
    let past = Utc::now() - Span::hours(1);
    let future = Utc::now() + Span::hours(1);

    idempotency::claim(&pool, "old", None, past).await.unwrap();
    idempotency::claim(&pool, "new", None, future)
        .await
        .unwrap();

    let purged = idempotency::purge_expired(&pool, Utc::now()).await.unwrap();
    assert_eq!(purged, 1, "只该清掉过期的那一个");
    assert!(!idempotency::seen(&pool, "old").await.unwrap());
    assert!(idempotency::seen(&pool, "new").await.unwrap());
}

#[test]
fn 节点幂等键的拼法稳定() {
    // 键的格式一旦在调用点各拼各的，去重会**静默**失效——两边拼出不同字符串，
    // 谁也看不出问题，直到重复执行真的发生
    assert_eq!(idempotency::node_key("run-1", "a"), "node:run-1:a");
    assert_ne!(
        idempotency::node_key("run-1", "a"),
        idempotency::node_key("run-1a", ""),
        "冒号分隔必须无歧义，否则 (run-1, a) 与 (run-1a, 空) 会撞键"
    );
}

// ------------------------------------------------------------------ 死信

#[sqlx::test]
async fn 死信能写入并查回(pool: PgPool) {
    let id = dead_letters::insert(
        &pool,
        &dead_letters::NewDeadLetter {
            stream: "hub:flows",
            stream_id: "1-1",
            run_id: Some("run-1"),
            flow_name: Some("intake"),
            node_id: Some("a"),
            attempts: 5,
            error: "插件不可达",
            payload_summary: Some(r#"{"payload_bytes":42}"#),
        },
    )
    .await
    .expect("写入应成功");

    let row = dead_letters::find(&pool, id)
        .await
        .unwrap()
        .expect("应查得到");
    assert_eq!(row.run_id.as_deref(), Some("run-1"));
    assert_eq!(row.flow_name.as_deref(), Some("intake"));
    assert_eq!(row.node_id.as_deref(), Some("a"));
    assert_eq!(row.attempts, 5);
    assert_eq!(row.error, "插件不可达");
    assert!(row.replayed_at.is_none(), "刚进死信的不该被标成已重放");

    assert_eq!(dead_letters::list(&pool, false, 10).await.unwrap().len(), 1);
}

#[sqlx::test]
async fn 同一条消息重复进死信只留一条(pool: PgPool) {
    let entry = |error: &'static str| dead_letters::NewDeadLetter {
        stream: "hub:flows",
        stream_id: "2-1",
        run_id: Some("run-2"),
        flow_name: Some("intake"),
        node_id: Some("b"),
        attempts: 5,
        error,
        payload_summary: None,
    };

    let first = dead_letters::insert(&pool, &entry("第一次的原因"))
        .await
        .unwrap();
    let second = dead_letters::insert(&pool, &entry("后来一次的原因"))
        .await
        .unwrap();

    assert_eq!(first, second, "同一条 Stream 消息只该有一条死信记录");

    let rows = dead_letters::list(&pool, false, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].error, "后来一次的原因",
        "重复进入时该刷新错误信息——最后一次的原因才是要看的那个"
    );
}

#[sqlx::test]
async fn 重放只记一次(pool: PgPool) {
    let id = dead_letters::insert(
        &pool,
        &dead_letters::NewDeadLetter {
            stream: "hub:flows",
            stream_id: "3-1",
            run_id: Some("run-3"),
            flow_name: None,
            node_id: None,
            attempts: 5,
            error: "失败了",
            payload_summary: None,
        },
    )
    .await
    .unwrap();

    let now = Utc::now();
    assert!(
        dead_letters::mark_replayed(&pool, id, "run-3-retry", now)
            .await
            .unwrap(),
        "第一次重放应成功"
    );
    assert!(
        !dead_letters::mark_replayed(&pool, id, "run-3-retry-again", now)
            .await
            .unwrap(),
        "已经重放过的再点一次不该产生第二次执行——重放按钮是最容易被连点的那种"
    );

    let row = dead_letters::find(&pool, id).await.unwrap().unwrap();
    assert_eq!(
        row.replayed_run_id.as_deref(),
        Some("run-3-retry"),
        "必须留下「重放成了哪次新执行」，否则重放之后就没法追踪了"
    );
}

#[sqlx::test]
async fn 默认只列还没重放的死信(pool: PgPool) {
    let entry = |stream_id: &'static str| dead_letters::NewDeadLetter {
        stream: "hub:flows",
        stream_id,
        run_id: None,
        flow_name: None,
        node_id: None,
        attempts: 5,
        error: "失败了",
        payload_summary: None,
    };

    let pending = dead_letters::insert(&pool, &entry("4-1")).await.unwrap();
    let done = dead_letters::insert(&pool, &entry("4-2")).await.unwrap();
    dead_letters::mark_replayed(&pool, done, "run-4-retry", Utc::now())
        .await
        .unwrap();

    let open = dead_letters::list(&pool, true, 10).await.unwrap();
    assert_eq!(open.len(), 1, "默认视图该只看需要处理的那一批");
    assert_eq!(open[0].id, pending);

    assert_eq!(
        dead_letters::list(&pool, false, 10).await.unwrap().len(),
        2,
        "要看历史时才全都列出来"
    );
}

#[sqlx::test]
async fn 死信按两条时间线清理(pool: PgPool) {
    let new_letter = |stream_id: &'static str| dead_letters::NewDeadLetter {
        stream: "hub:flows",
        stream_id,
        run_id: None,
        flow_name: None,
        node_id: None,
        attempts: 5,
        error: "e",
        payload_summary: None,
    };

    let replayed = dead_letters::insert(&pool, &new_letter("5-1"))
        .await
        .unwrap();
    dead_letters::mark_replayed(&pool, replayed, "run-5-retry", Utc::now())
        .await
        .unwrap();

    let fresh = dead_letters::insert(&pool, &new_letter("5-2"))
        .await
        .unwrap();

    let now = Utc::now();
    let old = now - Span::hours(1);
    assert_eq!(dead_letters::purge(&pool, old, old).await.unwrap(), 0);
    assert!(
        dead_letters::find(&pool, fresh).await.unwrap().is_some(),
        "还没重放的不该被清——那正是需要人去看的一批"
    );

    // 两条时间线都推到未来，两条都该被清
    let future = now + Span::hours(1);
    assert_eq!(dead_letters::purge(&pool, future, future).await.unwrap(), 2);
}

// ------------------------------------------------------------------ 超限载荷

#[sqlx::test]
async fn 载荷能存能取且摘要正确(pool: PgPool) {
    let bytes = b"payload-needle".repeat(1000);

    let stored = payloads::put(&pool, &bytes, Duration::from_secs(3600))
        .await
        .expect("存应成功");
    assert_eq!(stored.size, bytes.len() as i64);

    let row = payloads::get(&pool, &stored.id)
        .await
        .unwrap()
        .expect("应取得回来");
    assert_eq!(row.bytes, bytes, "取回的字节必须逐字节一致");
    assert_eq!(row.sha256, stored.sha256);

    let meta = payloads::meta(&pool, &stored.id).await.unwrap().unwrap();
    assert_eq!(meta.size, stored.size);
    assert_eq!(meta.sha256, stored.sha256);
}

#[sqlx::test]
async fn 过期载荷取不到(pool: PgPool) {
    // TTL 为 0：写下去就已经过期
    let stored = payloads::put(&pool, b"short-lived", Duration::ZERO)
        .await
        .expect("存应成功");

    assert!(
        payloads::get(&pool, &stored.id).await.unwrap().is_none(),
        "过期判定放在 SQL 的条件里，调用方不可能因为忘了比时间而用上一份本该消失的数据"
    );
    assert!(payloads::meta(&pool, &stored.id).await.unwrap().is_none());
}

#[sqlx::test]
async fn 过期载荷会被真正删掉(pool: PgPool) {
    payloads::put(&pool, b"a", Duration::ZERO).await.unwrap();
    payloads::put(&pool, b"b", Duration::from_secs(3600))
        .await
        .unwrap();

    let purged = payloads::purge_expired(&pool, Utc::now()).await.unwrap();
    assert_eq!(purged, 1, "只该清掉过期的那个");

    // TTL 只让数据「不再可读」，不等于「不再占空间」——清理必须真的删
    let (remaining,): (i64,) = sqlx::query_as("SELECT count(*) FROM payload_blobs")
        .fetch_one(&pool)
        .await
        .expect("查询应成功");
    assert_eq!(remaining, 1);
}

#[sqlx::test]
async fn 取不存在的载荷返回空而不是报错(pool: PgPool) {
    assert!(payloads::get(&pool, "从来没存过").await.unwrap().is_none());
    assert!(payloads::meta(&pool, "从来没存过").await.unwrap().is_none());
}
