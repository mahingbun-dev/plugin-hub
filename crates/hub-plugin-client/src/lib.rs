//! 中台 → 插件方向的 gRPC 客户端。
//!
//! 插件可能部署在任何主机上，中台按插件自注册时上报的 `advertise_addr` 直连。
//! 连接按地址缓存复用：插件是独立进程，热路径上每次重建连接的开销不可忽略；
//! `tonic::transport::Channel` 自身带重连，缓存住即可。
//!
//! 本 crate 同时实现了 [`hub_registry::PluginProbe`]，于是注册流程里的「可达性探测」
//! 用的就是这条真实链路，而不是另一套走样的检查。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hub_proto::v1::plugin_runtime_client::PluginRuntimeClient;
use hub_proto::v1::{
    Envelope, HandleResponse, HealthRequest, HealthResponse, ValidateRequest, ValidateResponse,
};
use hub_registry::{PluginProbe, ProbeOutcome};
use tokio::sync::Mutex;
use tonic::transport::{Channel, Endpoint};

/// 客户端超时。默认值刻意偏保守：插件是外部进程，不设超时会让中台的连接被拖死。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginClientConfig {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for PluginClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("插件地址 {addr} 非法: {reason}")]
    InvalidAddress { addr: String, reason: String },

    #[error("连接插件 {addr} 失败: {source}")]
    Connect {
        addr: String,
        #[source]
        source: tonic::transport::Error,
    },

    #[error("调用插件 {addr} 的 {method} 失败: {source}")]
    Call {
        addr: String,
        method: &'static str,
        #[source]
        source: tonic::Status,
    },
}

pub type Result<T> = std::result::Result<T, ClientError>;

/// 按地址复用连接的插件客户端。
#[derive(Clone)]
pub struct PluginClient {
    cfg: PluginClientConfig,
    channels: Arc<Mutex<HashMap<String, Channel>>>,
}

impl PluginClient {
    pub fn new(cfg: PluginClientConfig) -> Self {
        Self {
            cfg,
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 取（必要时建立）到插件地址的连接。
    ///
    /// 刻意不在持锁期间连接：并发首次调用会各自建连一次、后到的覆盖先到的，
    /// 代价是一次多余连接，换来的是不同插件的建连互不阻塞。
    async fn channel(&self, addr: &str) -> Result<Channel> {
        {
            let cache = self.channels.lock().await;
            if let Some(channel) = cache.get(addr) {
                return Ok(channel.clone());
            }
        }

        let channel = Endpoint::from_shared(addr.to_string())
            .map_err(|err| ClientError::InvalidAddress {
                addr: addr.to_string(),
                reason: err.to_string(),
            })?
            .connect_timeout(self.cfg.connect_timeout)
            .timeout(self.cfg.request_timeout)
            .connect()
            .await
            .map_err(|source| ClientError::Connect {
                addr: addr.to_string(),
                source,
            })?;

        self.channels
            .lock()
            .await
            .insert(addr.to_string(), channel.clone());
        Ok(channel)
    }

    /// 丢弃某个地址的缓存连接。插件重启换地址后由调用方触发。
    pub async fn forget(&self, addr: &str) {
        self.channels.lock().await.remove(addr);
    }

    async fn runtime(&self, addr: &str) -> Result<PluginRuntimeClient<Channel>> {
        Ok(PluginRuntimeClient::new(self.channel(addr).await?))
    }

    pub async fn health(&self, addr: &str) -> Result<HealthResponse> {
        let mut client = self.runtime(addr).await?;
        let response =
            client
                .health(HealthRequest {})
                .await
                .map_err(|source| ClientError::Call {
                    addr: addr.to_string(),
                    method: "Health",
                    source,
                })?;
        Ok(response.into_inner())
    }

    /// 调插件的校验器。数据进中台后先过它，通过才进 [`Self::handle`]。
    pub async fn validate(&self, addr: &str, envelope: Envelope) -> Result<ValidateResponse> {
        let mut client = self.runtime(addr).await?;
        let response = client
            .validate(ValidateRequest {
                envelope: Some(envelope),
            })
            .await
            .map_err(|source| ClientError::Call {
                addr: addr.to_string(),
                method: "Validate",
                source,
            })?;
        Ok(response.into_inner())
    }

    /// 调插件的插件体。
    pub async fn handle(&self, addr: &str, envelope: Envelope) -> Result<HandleResponse> {
        let mut client = self.runtime(addr).await?;
        let response = client
            .handle(hub_proto::v1::HandleRequest {
                envelope: Some(envelope),
            })
            .await
            .map_err(|source| ClientError::Call {
                addr: addr.to_string(),
                method: "Handle",
                source,
            })?;
        Ok(response.into_inner())
    }
}

/// 注册流程里的可达性探测走的就是这条真实链路。
#[async_trait]
impl PluginProbe for PluginClient {
    async fn health(&self, advertise_addr: &str) -> ProbeOutcome {
        match self.health(advertise_addr).await {
            Ok(response) if response.healthy => ProbeOutcome::Healthy {
                message: response.message,
            },
            Ok(response) => ProbeOutcome::Unhealthy {
                message: response.message,
            },
            Err(err) => ProbeOutcome::Unreachable {
                message: err.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> PluginClient {
        // 缩短超时，让「连不上」的用例快速失败
        PluginClient::new(PluginClientConfig {
            connect_timeout: Duration::from_millis(300),
            request_timeout: Duration::from_millis(300),
        })
    }

    #[tokio::test]
    async fn 地址非法时报地址错误而非连接错误() {
        let err = client().health("这不是一个地址").await.unwrap_err();
        assert!(
            matches!(err, ClientError::InvalidAddress { .. }),
            "应识别为地址问题，实际 {err:?}"
        );
    }

    #[tokio::test]
    async fn 地址不可达时报连接错误() {
        // 127.0.0.1:1 是保留端口，不会有服务监听
        let err = client().health("http://127.0.0.1:1").await.unwrap_err();
        assert!(
            matches!(err, ClientError::Connect { .. }),
            "应识别为连接问题，实际 {err:?}"
        );
    }

    #[tokio::test]
    async fn 探测把不可达归类为_unreachable() {
        // 显式走 trait 方法：PluginClient 上还有个同名的固有方法返回 Result
        let outcome = PluginProbe::health(&client(), "http://127.0.0.1:1").await;
        assert!(
            matches!(outcome, ProbeOutcome::Unreachable { .. }),
            "注册流程据此拒绝注册，必须是 Unreachable，实际 {outcome:?}"
        );
    }

    #[tokio::test]
    async fn 探测把非法地址也归类为_unreachable() {
        let outcome = PluginProbe::health(&client(), "::://bad").await;
        assert!(matches!(outcome, ProbeOutcome::Unreachable { .. }));
        assert!(
            !outcome.message().is_empty(),
            "拒绝原因不能为空，否则插件方无从排查"
        );
    }

    #[tokio::test]
    async fn 遗忘连接后再次调用会重新建连() {
        let client = client();
        // 先让它缓存一次失败的尝试（失败不会入缓存，这里验证 forget 对空缓存也安全）
        let _ = client.health("http://127.0.0.1:1").await;
        client.forget("http://127.0.0.1:1").await;
        assert!(client.channels.lock().await.is_empty());
    }
}
