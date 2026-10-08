//! 运维通道的客户端。`hubctl` 与集成测试都用它。

use std::path::Path;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{OpsRequest, OpsResponse};

/// 发一条指令并取回响应。
pub async fn request(path: &Path, request: &OpsRequest) -> std::io::Result<OpsResponse> {
    let stream = UnixStream::connect(path).await.map_err(|err| {
        std::io::Error::new(
            err.kind(),
            format!("连接运维通道 {} 失败: {err}", path.display()),
        )
    })?;

    let (reader, mut writer) = stream.into_split();

    let mut line = serde_json::to_vec(request)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();
    let response = lines.next_line().await?.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "中台没有返回响应")
    })?;

    serde_json::from_str(&response).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("响应无法解析为 JSON: {err}（原文: {response}）"),
        )
    })
}
