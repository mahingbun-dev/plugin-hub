//! 插件 → 中台方向的 gRPC 服务端。
//!
//! 插件可能部署在任何主机上，这条链路经 nginx 的 `grpc_pass` 独立端口进来
//! （见 `docs/design.md` 的网络拓扑）。
//!
//! **插件面按设计不做鉴权**：任何能连上该端口的进程都可以注册插件。这是刻意的
//! 设计决定，不是遗漏；补偿手段是注册时的可达性探测（地址不通即拒）与审计记录
//! 来源 IP。相关风险已记在 `docs/design.md` 的风险表里。
//!
//! 插件面同时挂三个服务：[`RegistryService`]（注册发现）、[`state::StateService`]
//! （外置状态）与 [`gateway::GatewayService`]（插件互调与发现）。后两者**是鉴权的**
//! ——它们认注册时下发的凭证，不认插件自报的身份，详见各自模块的说明。

use std::net::SocketAddr;

use hub_proto::v1::plugin_gateway_server::PluginGatewayServer;
use hub_proto::v1::hub_state_server::HubStateServer;
use hub_proto::v1::plugin_registry_server::{PluginRegistry, PluginRegistryServer};
use hub_proto::v1::{
    HeartbeatRequest, HeartbeatResponse, RegisterRequest, RegisterResponse, UnregisterRequest,
    UnregisterResponse,
};
use hub_registry::Registry;
use redis::aio::ConnectionManager;
use tonic::{Request, Response, Status};

use state::StateService;

pub mod gateway;
pub mod state;

// main.rs / 测试夹具直接从 crate 根取网关类型（与 `hub_grpc::state::` 同款习惯）
pub use gateway::{CallPolicy, GatewayService};

/// 来源 IP 的元数据键。
///
/// nginx 侧用 `grpc_set_header X-Real-IP $remote_addr;` 覆写它，所以经 nginx 进来的
/// 连接无法伪造；直连中台端口时这个头可以随便填，因此它只是**审计线索**而非可信身份
/// ——插件面本就不鉴权，不该在这里制造"有身份"的假象。
const REAL_IP_METADATA: &str = "x-real-ip";

#[derive(Clone)]
pub struct RegistryService {
    registry: Registry,
}

impl RegistryService {
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }
}

/// 取注册来源 IP：优先 nginx 写入的 `X-Real-IP`，否则退回对端地址。
///
/// 经 nginx 转发时对端地址是 nginx 自己（127.0.0.1），没有这个头就追不到真实来源。
///
/// `to_str()` 在 `MetadataValue<Ascii>` 上不会失败（类型保证值都是可见 ASCII），
/// 这里的 `and_then` 只是防御性写法，没有对应的可触发用例。
fn source_ip<T>(request: &Request<T>) -> Option<String> {
    request
        .metadata()
        .get(REAL_IP_METADATA)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| request.remote_addr().map(|addr| addr.ip().to_string()))
}

#[tonic::async_trait]
impl PluginRegistry for RegistryService {
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let source_ip = source_ip(&request);
        let req = request.into_inner();

        // 注册失败以结构化 rejections 回给插件方，**不用 gRPC 错误码**：
        // 插件侧只需要一套处理逻辑，且拒绝原因要能原样展示给人看。
        let response = self.registry.register(&req, source_ip.as_deref()).await;
        Ok(Response::new(response))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        let response = self
            .registry
            .heartbeat(&req.instance_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "心跳处理失败");
                Status::internal(err.to_string())
            })?;
        Ok(Response::new(response))
    }

    async fn unregister(
        &self,
        request: Request<UnregisterRequest>,
    ) -> Result<Response<UnregisterResponse>, Status> {
        let req = request.into_inner();
        self.registry
            .unregister(&req.instance_id, &req.state_token, &req.reason)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "注销处理失败");
                Status::internal(err.to_string())
            })?;
        Ok(Response::new(UnregisterResponse {}))
    }
}

/// 组装插件面的 gRPC 服务。
///
/// 返回元组而不是单个服务：tonic 的 `add_service` 一次只接一个，装配顺序由
/// 调用方（`serve` 或测试）决定。网关是 `Option`：没装配（mock、部分测试夹具）
/// 时整个 service 不挂上端口——挂一个每个方法都报错的空壳，只会让插件把
/// 「中台没这能力」误判成「中台坏了」。
pub fn services(
    registry: Registry,
    redis: ConnectionManager,
    async_exec: Option<hub_engine::AsyncExecutor>,
    gateway: Option<GatewayService>,
) -> (
    PluginRegistryServer<RegistryService>,
    HubStateServer<StateService>,
    Option<PluginGatewayServer<GatewayService>>,
) {
    // 注册面与状态面共用同一个 store：状态面的凭证校验要查注册时落下的行
    let pool = registry.store().pool().clone();

    let mut state = StateService::new(pool, redis);
    if let Some(exec) = async_exec {
        // 装了才有 `Publish`。没装时那个方法明确回「这个能力没开」，
        // 而不是静默地把信封丢掉——静默丢消息是最难查的一类问题
        state = state.with_async(exec);
    }

    (
        PluginRegistryServer::new(RegistryService::new(registry)),
        HubStateServer::new(state),
        gateway.map(PluginGatewayServer::new),
    )
}

/// 在 `addr` 上提供插件面，直到 `shutdown` 完成。
///
/// 绑定地址由调用方决定：生产环境必须是 `0.0.0.0`——远程插件要靠它注册进来
/// （与只绑回环的 HTTP 面刻意不同，见 `docs/design.md`）。
pub async fn serve(
    addr: SocketAddr,
    registry: Registry,
    redis: ConnectionManager,
    async_exec: Option<hub_engine::AsyncExecutor>,
    gateway: Option<GatewayService>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), tonic::transport::Error> {
    tracing::info!(%addr, "插件面已监听");
    let (registry_svc, state_svc, gateway_svc) = services(registry, redis, async_exec, gateway);
    // builder 的类型随 add_service 逐级变化，optional 服务只能在分支里各自收口
    match gateway_svc {
        Some(gateway_svc) => {
            tonic::transport::Server::builder()
                .add_service(registry_svc)
                .add_service(state_svc)
                .add_service(gateway_svc)
                .serve_with_shutdown(addr, shutdown)
                .await
        }
        None => {
            tonic::transport::Server::builder()
                .add_service(registry_svc)
                .add_service(state_svc)
                .serve_with_shutdown(addr, shutdown)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::metadata::MetadataValue;

    #[test]
    fn 优先采用_x_real_ip_头() {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert(REAL_IP_METADATA, MetadataValue::from_static("10.1.2.3"));
        assert_eq!(source_ip(&request).as_deref(), Some("10.1.2.3"));
    }

    #[test]
    fn 没有头时退回对端地址() {
        // 测试里没有真实连接，remote_addr 为空 → None
        let request = Request::new(());
        assert_eq!(source_ip(&request), None);
    }
}
