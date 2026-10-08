//! 运维通道的集成测试：真起一个 socket，用真客户端连。
//!
//! 这条通道是管理面全插件化架构下的唯一逃生口，所以它必须真的能用——
//! 单测协议编解码不够，得验证「起得来、连得上、权限对、破坏性操作真的生效」。

use std::path::PathBuf;

use hub_ops::{OpsRequest, OpsResponse, OpsState};
use hub_store::Store;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::watch;

/// 每个用例一个独立 socket 路径。
fn socket_path(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("anc-hub-ops-tests");
    std::fs::create_dir_all(&dir).expect("建临时目录失败");
    dir.join(format!("{tag}-{}.sock", std::process::id()))
}

struct Running {
    path: PathBuf,
    stop: watch::Sender<bool>,
}

impl Running {
    async fn shutdown(self) {
        let _ = self.stop.send(true);
        // 等 serve 把 socket 文件清掉
        for _ in 0..50 {
            if !self.path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

async fn start(store: Store, tag: &str) -> Running {
    let path = socket_path(tag);
    let state = OpsState {
        store,
        config_summary: json!({"http_port": 8092, "grpc_port": 8093}),
    };
    let (tx, mut rx) = watch::channel(false);
    let serving = path.clone();
    tokio::spawn(async move {
        let _ = hub_ops::serve(&serving, state, async move {
            let _ = rx.changed().await;
        })
        .await;
    });

    // 等 socket 出现
    for _ in 0..50 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(path.exists(), "运维 socket 应已创建");

    Running { path, stop: tx }
}

fn ok_data(response: OpsResponse) -> serde_json::Value {
    assert!(response.ok, "指令应成功: {:?}", response.error);
    response.data.unwrap_or(serde_json::Value::Null)
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn status_给出自检概览(pool: PgPool) {
    let running = start(Store::from_pool(pool), "status").await;

    let data = ok_data(
        hub_ops::request(&running.path, &OpsRequest::Status)
            .await
            .expect("请求失败"),
    );

    assert_eq!(data["database"], "ok");
    assert_eq!(data["plugin_count"], 0);
    assert_eq!(data["instance_count"], 0);
    assert_eq!(
        data["config"]["grpc_port"], 8093,
        "配置摘要要能一眼看到关键端口"
    );

    running.shutdown().await;
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 列出插件与实例(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    // 直接落库，不经过注册流程：这里测的是运维通道，不是注册
    let version = hub_store::plugins::upsert_version(
        store.pool(),
        &hub_store::plugins::NewVersion {
            plugin_name: "auth",
            description: "鉴权插件",
            owner: "qa",
            version: "1.0.0",
            manifest: b"manifest",
            descriptor: b"descriptor",
            produces: &[],
            consumes: &[],
            tools: &[],
        },
    )
    .await
    .expect("写入失败");
    hub_store::instances::upsert_instance(store.pool(), version.row.id, "i-1", "http://a:1", None)
        .await
        .expect("注册失败");

    let running = start(store, "list").await;

    let plugins = ok_data(
        hub_ops::request(&running.path, &OpsRequest::ListPlugins)
            .await
            .expect("请求失败"),
    );
    assert_eq!(plugins[0]["name"], "auth");
    assert_eq!(plugins[0]["version_count"], 1);

    let instances = ok_data(
        hub_ops::request(&running.path, &OpsRequest::ListInstances)
            .await
            .expect("请求失败"),
    );
    assert_eq!(instances[0]["instance_id"], "i-1");

    running.shutdown().await;
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 摘掉卡住的实例(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    let version = hub_store::plugins::upsert_version(
        store.pool(),
        &hub_store::plugins::NewVersion {
            plugin_name: "p",
            description: "",
            owner: "",
            version: "1.0.0",
            manifest: b"m",
            descriptor: b"d",
            produces: &[],
            consumes: &[],
            tools: &[],
        },
    )
    .await
    .expect("写入失败");
    hub_store::instances::upsert_instance(
        store.pool(),
        version.row.id,
        "stuck",
        "http://a:1",
        None,
    )
    .await
    .expect("注册失败");

    let running = start(store, "remove-instance").await;

    let data = ok_data(
        hub_ops::request(
            &running.path,
            &OpsRequest::RemoveInstance {
                instance_id: "stuck".to_string(),
            },
        )
        .await
        .expect("请求失败"),
    );
    assert_eq!(data["removed"], true);

    // 再摘一次应返回 false，而不是报错
    let data = ok_data(
        hub_ops::request(
            &running.path,
            &OpsRequest::RemoveInstance {
                instance_id: "stuck".to_string(),
            },
        )
        .await
        .expect("请求失败"),
    );
    assert_eq!(data["removed"], false);

    running.shutdown().await;
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 删除坏的插件版本(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    let produces = vec!["wms.v1.OrderCreated".to_string()];
    for version in ["1.0.0", "1.1.0"] {
        hub_store::plugins::upsert_version(
            store.pool(),
            &hub_store::plugins::NewVersion {
                plugin_name: "p",
                description: "",
                owner: "",
                version,
                manifest: b"m",
                descriptor: b"d",
                produces: &produces,
                consumes: &[],
                tools: &[],
            },
        )
        .await
        .expect("写入失败");
    }

    let running = start(store, "remove-version").await;

    let data = ok_data(
        hub_ops::request(
            &running.path,
            &OpsRequest::RemoveVersion {
                plugin: "p".to_string(),
                version: "1.1.0".to_string(),
            },
        )
        .await
        .expect("请求失败"),
    );
    assert_eq!(data["removed"], true);

    // 契约应随版本级联删除
    let contracts =
        hub_store::plugins::versions_by_fq_name(&pool, "wms.v1.OrderCreated", "produces")
            .await
            .expect("查询失败");
    assert_eq!(
        contracts,
        vec![("p".to_string(), "1.0.0".to_string())],
        "只应剩下 1.0.0"
    );

    running.shutdown().await;
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 删除整个插件(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    hub_store::plugins::upsert_version(
        store.pool(),
        &hub_store::plugins::NewVersion {
            plugin_name: "auth",
            description: "",
            owner: "",
            version: "1.0.0",
            manifest: b"m",
            descriptor: b"d",
            produces: &[],
            consumes: &[],
            tools: &[],
        },
    )
    .await
    .expect("写入失败");

    let running = start(store, "delete-plugin").await;

    let data = ok_data(
        hub_ops::request(
            &running.path,
            &OpsRequest::DeletePlugin {
                name: "auth".to_string(),
            },
        )
        .await
        .expect("请求失败"),
    );
    assert_eq!(data["removed"], true);

    assert!(
        hub_store::plugins::find_plugin(&pool, "auth")
            .await
            .expect("查询失败")
            .is_none()
    );

    running.shutdown().await;
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 无法解析的指令被拒绝且连接不崩(pool: PgPool) {
    let running = start(Store::from_pool(pool), "bad-request").await;

    // 绕过客户端类型，直接往 socket 写一段垃圾——这条通道要经得起手敲错
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let stream = tokio::net::UnixStream::connect(&running.path)
        .await
        .expect("连接失败");
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(b"{\"command\":\"drop-everything\"}\n")
        .await
        .expect("写失败");
    let mut lines = tokio::io::BufReader::new(reader).lines();
    let line = lines.next_line().await.expect("读失败").expect("应有响应");

    let response: OpsResponse = serde_json::from_str(&line).expect("应是合法 JSON");
    assert!(!response.ok);
    assert!(
        response.error.unwrap_or_default().contains("无法解析"),
        "应说明是指令问题"
    );

    // 服务仍应可用
    let status = hub_ops::request(&running.path, &OpsRequest::Status)
        .await
        .expect("服务应仍可用");
    assert!(status.ok);

    running.shutdown().await;
}

#[cfg(unix)]
#[sqlx::test(migrations = "../hub-store/migrations")]
async fn socket_只对属主开放(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt as _;

    let running = start(Store::from_pool(pool), "perms").await;

    let mode = std::fs::metadata(&running.path)
        .expect("取元数据失败")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "信任边界靠文件系统：socket 必须只对属主可读写，实际 {mode:o}"
    );

    running.shutdown().await;
}

#[sqlx::test(migrations = "../hub-store/migrations")]
async fn 关停后_socket_文件被清理(pool: PgPool) {
    let running = start(Store::from_pool(pool), "cleanup").await;
    let path = running.path.clone();
    running.shutdown().await;
    assert!(!path.exists(), "退出时应清掉 socket 文件，避免下次启动误判");
}
