//! 运维通道的服务端。
//!
//! 一问一答：读一行请求、写一行响应、关连接。不维护会话状态——出问题时越简单越好。

use std::future::Future;
use std::path::Path;

use serde_json::json;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{info, warn};

use hub_store::Store;

use crate::protocol::{OpsRequest, OpsResponse};

/// 运维通道的依赖。
#[derive(Clone)]
pub struct OpsState {
    pub store: Store,

    /// 配置摘要（**不含敏感值**），供 `status` 展示
    pub config_summary: serde_json::Value,
}

/// 在 `path` 上提供运维通道，直到 `shutdown` 完成。
pub async fn serve(
    path: &Path,
    state: OpsState,
    shutdown: impl Future<Output = ()> + Send,
) -> std::io::Result<()> {
    // 上次异常退出可能留下 socket 文件，先清掉
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }

    let listener = UnixListener::bind(path)?;
    restrict_to_owner(path)?;
    info!(socket = %path.display(), "运维通道已监听（主机面）");

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => {
                info!("运维通道开始退出");
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        tokio::spawn(async move { handle(stream, state).await });
                    }
                    Err(err) => warn!(error = %err, "接受运维连接失败"),
                }
            }
        }
    }

    // 留下 socket 文件会让下次启动的 remove_file 白做一次，主动清掉更干净
    let _ = std::fs::remove_file(path);
    Ok(())
}

/// 只允许属主读写：信任边界靠文件系统，不靠鉴权。
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

async fn handle(stream: UnixStream, state: OpsState) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let response = match lines.next_line().await {
        Ok(Some(line)) => dispatch(&state, &line).await,
        Ok(None) => OpsResponse::failed("空请求：需要一行 JSON"),
        Err(err) => OpsResponse::failed(format!("读取请求失败: {err}")),
    };

    let mut body = serde_json::to_vec(&response)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"响应序列化失败"}"#.as_bytes().to_vec());
    body.push(b'\n');
    if let Err(err) = writer.write_all(&body).await {
        warn!(error = %err, "写运维响应失败");
    }
}

async fn dispatch(state: &OpsState, line: &str) -> OpsResponse {
    let request: OpsRequest = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(err) => return OpsResponse::failed(format!("指令无法解析: {err}")),
    };

    // 破坏性操作留痕：这条通道的动作不该是悄无声息的
    if request.is_destructive() {
        warn!(command = request.command_name(), "执行破坏性运维指令");
    }

    match execute(state, request).await {
        Ok(response) => response,
        Err(err) => OpsResponse::failed(err.to_string()),
    }
}

/// 运维指令的执行错误。
#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error(transparent)]
    Store(#[from] hub_store::StoreError),

    #[error("结果序列化失败: {0}")]
    Serialize(#[from] serde_json::Error),
}

async fn execute(state: &OpsState, request: OpsRequest) -> Result<OpsResponse, OpsError> {
    let pool = state.store.pool();

    Ok(match request {
        OpsRequest::Status => {
            let plugins = hub_store::plugins::list_plugins(pool).await?;
            let instances = hub_store::instances::list_instances(pool).await?;
            let version_count: i64 = plugins.iter().map(|p| p.version_count).sum();

            OpsResponse::ok(json!({
                // 能查到这一步就说明数据库通，不需要单独做一次探活
                "database": "ok",
                "plugin_count": plugins.len(),
                "version_count": version_count,
                "instance_count": instances.len(),
                "config": state.config_summary,
            }))
        }

        OpsRequest::ListPlugins => OpsResponse::ok(serde_json::to_value(
            hub_store::plugins::list_plugins(pool).await?,
        )?),

        OpsRequest::ListInstances => OpsResponse::ok(serde_json::to_value(
            hub_store::instances::list_instances(pool).await?,
        )?),

        OpsRequest::RemoveInstance { instance_id } => {
            let removed = hub_store::instances::delete_instance(pool, &instance_id).await?;
            OpsResponse::ok(json!({ "removed": removed, "instance_id": instance_id }))
        }

        OpsRequest::RemoveVersion { plugin, version } => {
            let removed = hub_store::plugins::delete_version(pool, &plugin, &version).await?;
            OpsResponse::ok(json!({ "removed": removed, "plugin": plugin, "version": version }))
        }

        OpsRequest::DeletePlugin { name } => {
            let removed = hub_store::plugins::delete_plugin(pool, &name).await?;
            OpsResponse::ok(json!({ "removed": removed, "plugin": name }))
        }
    })
}

/// 给 `serve` 用的关停信号对。
pub fn shutdown_channel() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 关停信号对可用() {
        let (tx, mut rx) = shutdown_channel();
        assert!(!*rx.borrow_and_update());
        let _ = tx.send(true);
        assert!(*rx.borrow_and_update());
    }
}
