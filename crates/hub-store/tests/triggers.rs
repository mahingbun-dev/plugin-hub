//! 触发器的集成测试。
//!
//! 最要紧的两条：**「flow 不存在」要报成一句人话而不是外键约束**，以及
//! **成功时把 last_error 清掉**——后者关系到「狼来了」：一个修好的定时器若永远带着
//! 一条历史错误，控制台上的告警就不再有人看。

use chrono::Utc;
use hub_store::flows::{self, DraftInput};
use hub_store::triggers::{self, NewTrigger};
use serde_json::json;
use sqlx::PgPool;

/// 建一条 flow 出来——触发器必须挂在一条存在的 flow 上。
async fn flow(pool: &PgPool, name: &str) {
    flows::upsert_draft(
        pool,
        &DraftInput {
            name,
            description: "触发器测试用",
            definition: &json!({
                "name": name,
                "nodes": [{"id": "a", "plugin": "p"}],
                "edges": []
            }),
            validation: None,
            created_by: "tester",
        },
    )
    .await
    .expect("建 flow 应成功");
}

/// 造一个 cron 触发器登记项。
///
/// 写成宏而不是函数：`NewTrigger` 借用 `config`，而函数里构造完就返回会把 `json!`
/// 产生的临时值丢掉。宏展开成表达式，临时值的生命周期自然延续到整条语句结束。
macro_rules! cron {
    ($flow:expr, $name:expr, $expr:expr) => {
        triggers::NewTrigger {
            flow_name: $flow,
            kind: triggers::KIND_CRON,
            name: $name,
            config: &json!({ "expr": $expr }),
        }
    };
}

#[sqlx::test]
async fn 能登记并读回触发器(pool: PgPool) {
    flow(&pool, "intake").await;

    let row = triggers::upsert(&pool, &cron!("intake", "nightly", "0 2 * * *"))
        .await
        .expect("登记应成功");

    assert_eq!(row.kind, "cron");
    assert_eq!(row.name, "nightly");
    assert_eq!(row.config["expr"], "0 2 * * *");
    assert!(row.enabled, "新登记的默认启用");
    assert_eq!(row.fired_count, 0);
    assert!(row.last_fired_at.is_none(), "还没跑过，不该有触发时间");

    let all = triggers::list_of_flow(&pool, "intake").await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, row.id);
}

#[sqlx::test]
async fn 同名触发器是改不是新增(pool: PgPool) {
    flow(&pool, "intake").await;

    let first = triggers::upsert(&pool, &cron!("intake", "nightly", "0 2 * * *"))
        .await
        .unwrap();
    let second = triggers::upsert(&pool, &cron!("intake", "nightly", "0 3 * * *"))
        .await
        .unwrap();

    assert_eq!(first.id, second.id, "改表达式是「改」不是「换一个新的」");
    assert_eq!(second.config["expr"], "0 3 * * *");

    assert_eq!(
        triggers::list_of_flow(&pool, "intake").await.unwrap().len(),
        1,
        "留下历史版本只会让「现在到底有哪几个定时器」变模糊"
    );
}

#[sqlx::test]
async fn 同名但不同类可以共存(pool: PgPool) {
    flow(&pool, "intake").await;

    triggers::upsert(&pool, &cron!("intake", "default", "0 2 * * *"))
        .await
        .unwrap();
    triggers::upsert(
        &pool,
        &NewTrigger {
            flow_name: "intake",
            kind: triggers::KIND_MQ,
            name: "default",
            config: &json!({"stream": "wms:orders"}),
        },
    )
    .await
    .unwrap();

    let all = triggers::list_of_flow(&pool, "intake").await.unwrap();
    assert_eq!(all.len(), 2, "cron 与 mq 各是一条，名字撞了也不该互相顶掉");
}

#[sqlx::test]
async fn 挂在不存在的_flow_上会被人话拒绝(pool: PgPool) {
    let err = triggers::upsert(&pool, &cron!("从来没有过", "default", "0 2 * * *"))
        .await
        .expect_err("应被拒绝");

    let message = err.to_string();
    assert!(
        message.contains("从来没有过"),
        "要说清是哪条 flow 不存在，而不是把一句外键约束错误抛给用户：{message}"
    );
}

#[sqlx::test]
async fn 只列启用中的触发器(pool: PgPool) {
    flow(&pool, "intake").await;

    let on = triggers::upsert(&pool, &cron!("intake", "on", "0 2 * * *"))
        .await
        .unwrap();
    let off = triggers::upsert(&pool, &cron!("intake", "off", "0 4 * * *"))
        .await
        .unwrap();
    assert!(triggers::set_enabled(&pool, off.id, false).await.unwrap());

    let enabled = triggers::list_enabled(&pool, Some(triggers::KIND_CRON))
        .await
        .unwrap();
    assert_eq!(enabled.len(), 1, "关掉的触发器不该被装配进调度器");
    assert_eq!(enabled[0].id, on.id);

    assert_eq!(
        triggers::list_enabled(&pool, None).await.unwrap().len(),
        1,
        "不限类型时也只列启用的"
    );
}

#[sqlx::test]
async fn 重启一条触发器会把它重新启用(pool: PgPool) {
    flow(&pool, "intake").await;

    let row = triggers::upsert(&pool, &cron!("intake", "nightly", "0 2 * * *"))
        .await
        .unwrap();
    triggers::set_enabled(&pool, row.id, false).await.unwrap();

    let again = triggers::upsert(&pool, &cron!("intake", "nightly", "0 3 * * *"))
        .await
        .unwrap();

    assert!(
        again.enabled,
        "重新登记一份配置的意图就是「让它按这个跑」，还留着禁用状态会让人以为没生效"
    );
}

#[sqlx::test]
async fn 触发结果会被记下来(pool: PgPool) {
    flow(&pool, "intake").await;
    let row = triggers::upsert(&pool, &cron!("intake", "nightly", "0 2 * * *"))
        .await
        .unwrap();

    // 失败一次
    triggers::record_fired(&pool, row.id, Utc::now(), Some("插件不可达"))
        .await
        .unwrap();
    let failed = triggers::list_of_flow(&pool, "intake").await.unwrap();
    assert_eq!(failed[0].fired_count, 1);
    assert_eq!(failed[0].last_error.as_deref(), Some("插件不可达"));
    assert!(failed[0].last_fired_at.is_some());

    // 再成功一次
    triggers::record_fired(&pool, row.id, Utc::now(), None)
        .await
        .unwrap();
    let healed = triggers::list_of_flow(&pool, "intake").await.unwrap();
    assert_eq!(healed[0].fired_count, 2);
    assert!(
        healed[0].last_error.is_none(),
        "成功时必须把 last_error 清掉，否则一个修好的定时器会永远带着历史错误，\
         控制台据此报警就变成狼来了"
    );
}

#[sqlx::test]
async fn 非法类型被拒绝且不落库(pool: PgPool) {
    flow(&pool, "intake").await;

    let err = triggers::upsert(
        &pool,
        &NewTrigger {
            flow_name: "intake",
            kind: "webhook",
            name: "default",
            config: &json!({}),
        },
    )
    .await
    .expect_err("不该接受未定义的类型");

    assert!(err.to_string().contains("webhook"), "{err}");
    assert!(
        triggers::list_of_flow(&pool, "intake")
            .await
            .unwrap()
            .is_empty(),
        "被拒绝的登记不该留下一行——数据库的 CHECK 会挡住它，但错误信息远不如这一句清楚"
    );
}

#[sqlx::test]
async fn 删除触发器(pool: PgPool) {
    flow(&pool, "intake").await;
    let row = triggers::upsert(&pool, &cron!("intake", "nightly", "0 2 * * *"))
        .await
        .unwrap();

    assert!(triggers::delete(&pool, row.id).await.unwrap());
    assert!(
        !triggers::delete(&pool, row.id).await.unwrap(),
        "删过了再删返回 false"
    );
    assert!(
        triggers::list_of_flow(&pool, "intake")
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn 缺必填配置的触发器被拒且不落库(pool: PgPool) {
    flow(&pool, "intake").await;

    // 空串与缺字段是同一件事：调度器读到的都是空，都会被跳过
    let cases = [
        (triggers::KIND_CRON, json!({}), "expr"),
        (triggers::KIND_CRON, json!({ "expr": "   " }), "expr"),
        (triggers::KIND_MQ, json!({}), "stream"),
        (triggers::KIND_MQ, json!({ "stream": "" }), "stream"),
    ];

    for (kind, config, hint) in cases {
        let err = triggers::upsert(
            &pool,
            &NewTrigger {
                flow_name: "intake",
                kind,
                name: "default",
                config: &config,
            },
        )
        .await
        .expect_err("缺必填项应当被拒");

        assert!(
            err.to_string().contains(hint),
            "{kind} 配 {config} 的错误里应当点出 {hint}，实际是：{err}"
        );
    }

    assert!(
        triggers::list_of_flow(&pool, "intake")
            .await
            .unwrap()
            .is_empty(),
        "被拒的登记不该留下行——那种触发器从存下来第一秒就注定不工作，\
         却在界面上看起来是配好了的"
    );
}

#[sqlx::test]
async fn 登记列表带上所属_flow_且含已停用的(pool: PgPool) {
    flow(&pool, "intake").await;
    flow(&pool, "shipping").await;

    let intake = triggers::upsert(&pool, &cron!("intake", "nightly", "0 2 * * *"))
        .await
        .unwrap();
    triggers::upsert(&pool, &cron!("shipping", "hourly", "0 * * * *"))
        .await
        .unwrap();
    triggers::set_enabled(&pool, intake.id, false)
        .await
        .unwrap();

    let all = triggers::list_all(&pool).await.unwrap();
    assert_eq!(all.len(), 2, "停用的那条也要在列表里");

    let by_name: std::collections::HashMap<_, _> =
        all.iter().map(|t| (t.name.as_str(), t)).collect();
    assert_eq!(by_name["nightly"].flow_name, "intake");
    assert_eq!(by_name["hourly"].flow_name, "shipping");
    assert!(!by_name["nightly"].enabled, "停用状态要如实带出来");
    // 排障时问「它为什么没跑」，第一个答案是「它被关了」——所以停用的排在最前
    assert_eq!(all[0].name, "nightly", "停用的排前面");
}
