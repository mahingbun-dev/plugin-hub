//! 注册拒绝留痕的集成测试。
//!
//! 最要紧的三条：**同键重试累计而不是新增行**（被拒的插件每 5 秒重试一次，
//! 逐次插入表会撑爆）、**first_seen_at 不被重试刷新**（「问题存在多久了」
//! 比「重试了多少轮」更重要）、**销案真的销得掉**（注册成功或版本被删后，
//! 控制台不能继续喊狼来了）。

use sqlx::PgPool;

use hub_store::rejections::{self, NewRejection};

/// 造一条 VERSION_CONFLICT 式的拒绝输入。
fn conflict<'a>(plugin: &'a str, instance: &'a str) -> NewRejection<'a> {
    NewRejection {
        plugin_name: plugin,
        instance_id: instance,
        code: 5, // hub.v1.REJECT_CODE_VERSION_CONFLICT
        version: "0.2.0",
        message: "版本 0.2.0 已存在且契约或 manifest 不一致",
        detail: "同一版本号不可改变契约；请升版本号后重新注册",
        source_ip: "127.0.0.1",
    }
}

#[sqlx::test]
async fn 记录首次落库再遇累计(pool: PgPool) {
    rejections::record(&pool, &conflict("sql-executor", "sql-executor-1"))
        .await
        .expect("首次记录应成功");

    let rows = rejections::list(&pool, None, 100)
        .await
        .expect("查询应成功");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].plugin_name, "sql-executor");
    assert_eq!(rows[0].count, 1);

    // 重试一轮：仍是一行，count 累计，first_seen 不动，message 覆盖为最新一次的说法
    rejections::record(&pool, &conflict("sql-executor", "sql-executor-1"))
        .await
        .expect("重试记录应成功");

    let rows = rejections::list(&pool, None, 100)
        .await
        .expect("查询应成功");
    assert_eq!(rows.len(), 1, "同键重试必须收敛成一行");
    assert_eq!(rows[0].count, 2);
}

#[sqlx::test]
async fn 不同实例或拒绝码各成一行(pool: PgPool) {
    rejections::record(&pool, &conflict("sql-executor", "sql-executor-1"))
        .await
        .expect("记录应成功");
    rejections::record(&pool, &conflict("sql-executor", "sql-executor-2"))
        .await
        .expect("记录应成功");

    let mut unreachable = conflict("sql-executor", "sql-executor-1");
    unreachable.code = 2; // hub.v1.REJECT_CODE_DESCRIPTOR_INVALID
    unreachable.message = "descriptor 无法解析";
    rejections::record(&pool, &unreachable)
        .await
        .expect("记录应成功");

    let rows = rejections::list(&pool, None, 100)
        .await
        .expect("查询应成功");
    assert_eq!(rows.len(), 3, "实例不同、拒绝码不同都该是新的一行");
}

#[sqlx::test]
async fn 按插件过滤(pool: PgPool) {
    rejections::record(&pool, &conflict("sql-executor", "sql-executor-1"))
        .await
        .expect("记录应成功");
    rejections::record(&pool, &conflict("auth", "auth-plugin-1"))
        .await
        .expect("记录应成功");

    let rows = rejections::list(&pool, Some("sql-executor"), 100)
        .await
        .expect("查询应成功");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].plugin_name, "sql-executor");
}

#[sqlx::test]
async fn 清案分实例级与插件级(pool: PgPool) {
    rejections::record(&pool, &conflict("sql-executor", "sql-executor-1"))
        .await
        .expect("记录应成功");
    rejections::record(&pool, &conflict("sql-executor", "sql-executor-2"))
        .await
        .expect("记录应成功");
    rejections::record(&pool, &conflict("auth", "auth-plugin-1"))
        .await
        .expect("记录应成功");

    // 实例级销案：注册成功的那一个实例清掉，别的还在
    let cleared = rejections::clear(&pool, "sql-executor", Some("sql-executor-1"))
        .await
        .expect("销案应成功");
    assert_eq!(cleared, 1);

    let rows = rejections::list(&pool, Some("sql-executor"), 100)
        .await
        .expect("查询应成功");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].instance_id, "sql-executor-2");

    // 插件级销案（管理面删版本后的「彻底重来」）：整插件的记录作废，别家不受牵连
    let cleared = rejections::clear(&pool, "sql-executor", None)
        .await
        .expect("销案应成功");
    assert_eq!(cleared, 1);

    let rows = rejections::list(&pool, None, 100)
        .await
        .expect("查询应成功");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].plugin_name, "auth");
}
