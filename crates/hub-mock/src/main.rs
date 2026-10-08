//! mock 中台的入口：一个进程、两个面。
//!
//! 插件面（gRPC，默认 `:8093`）**与真中台同端口**，插件模板里的 `HUB_ADDR` 不用改；
//! HTTP 面（默认 `127.0.0.1:8092`）也刻意与真中台本地端口一致。
//!
//! 端口刻意对齐是有代价的：真中台与 mock 不能同时跑。这是有意的取舍——让开发者
//! 「把中台换掉」只需要启停一个进程，而不是去插件的配置里改地址；改地址这件事
//! 正是接入时最容易错、且报错最难懂的一步。
//!
//! 端口被占时会**明确报出来并给出退路**，而不是静默换一个——换一个的话插件按原
//! 地址连不上，开发者会以为是插件的问题。
//!
//! **两个面的服务实现都在 lib 里**（[`hub_mock::MockService`]）：那层有几个容易错、
//! 而开发者一定会撞上的地方（metadata 取凭证、错误码、参数校验），放这儿就测不着。
//! 这个文件只做装配。

use std::net::SocketAddr;
use std::sync::Arc;

use hub_mock::{MOCK_NAME, MockRegistry, MockService};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_proto::v1::hub_state_server::HubStateServer;
use hub_proto::v1::plugin_registry_server::PluginRegistryServer;
use hub_registry::RegistryConfig;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HUB_MOCK_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let grpc_addr: SocketAddr = env_or("HUB_MOCK_GRPC_ADDR", "0.0.0.0:8093").parse()?;
    let http_addr: SocketAddr = env_or("HUB_MOCK_HTTP_ADDR", "127.0.0.1:8092").parse()?;
    let heartbeat_seconds: i32 = env_or("HUB_MOCK_HEARTBEAT_SECONDS", "10").parse()?;

    let cfg = RegistryConfig {
        heartbeat_interval_seconds: heartbeat_seconds,
        ..RegistryConfig::default()
    };

    // 探测用真实的 gRPC 客户端：本地最该验的恰恰是「中台能不能拨通你上报的地址」，
    // 用一个永远说健康的假探针就把这一关的意义抹掉了
    let registry = Arc::new(MockRegistry::new(
        Arc::new(PluginClient::new(PluginClientConfig::default())),
        cfg,
    ));

    let grpc_listener = tokio::net::TcpListener::bind(grpc_addr)
        .await
        .map_err(|e| bind_failure("插件面（gRPC）", grpc_addr, e))?;
    let http_listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .map_err(|e| bind_failure("HTTP 面", http_addr, e))?;

    // 起手先把「我是谁」喊清楚。mock 与真中台绑同一组端口，而「注册成功了」这句日志
    // 两者都会打——不声明身份的话，人会拿着 mock 的结果当对真中台验过了。
    info!(
        "这是 **mock 中台**，不是真中台。判据与真中台同源（注册校验调 hub-registry 的函数，\
         状态面调 hub-core 的规则），但它**不落库**、**不做心跳超时摘除**、\
         **没有 flow 引擎**（Publish 回 Unimplemented）——涉及这三点的结论在 mock 上不成立。\
         确认对面是谁：curl http://{http_addr}/health，name 字段应为 {MOCK_NAME}"
    );
    info!("状态面已挂载：KvGet / KvPut / KvDelete / KvScan（Publish 未实现）");
    info!(%grpc_addr, "插件面已监听——把插件的 HUB_ADDR 指向这里");
    info!(%http_addr, "HTTP 面已监听——GET /plugins 看在册的，GET /rejections 看被拒的");

    let grpc_task = tokio::spawn({
        let service = MockService::new(Arc::clone(&registry));
        async move {
            tonic::transport::Server::builder()
                .add_service(PluginRegistryServer::new(service.clone()))
                // 状态面与注册面挂在**同一个端口**：契约里它们本就是两个 service，
                // 而插件的 HUB_ADDR 只有一个，没有理由让插件去配第二个地址
                .add_service(HubStateServer::new(service))
                .serve_with_incoming(listener_stream(grpc_listener))
                .await
        }
    });

    let http_task = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            let app = axum::Router::new()
                .route(
                    "/plugins",
                    axum::routing::get({
                        let registry = Arc::clone(&registry);
                        move || {
                            let registry = Arc::clone(&registry);
                            async move { axum::Json(registry.plugins()) }
                        }
                    }),
                )
                .route(
                    "/rejections",
                    axum::routing::get({
                        let registry = Arc::clone(&registry);
                        move || {
                            let registry = Arc::clone(&registry);
                            async move { axum::Json(registry.rejections()) }
                        }
                    }),
                )
                .route(
                    "/health",
                    axum::routing::get(|| async {
                        axum::Json(serde_json::json!({
                            "status": "ok",
                            // 这一个字段是「我打的是谁」的判据：真中台报的是 anc-hub
                            "name": hub_mock::MOCK_NAME,
                            "version": env!("CARGO_PKG_VERSION"),
                        }))
                    }),
                );

            axum::serve(http_listener, app).await
        }
    });

    tokio::select! {
        r = grpc_task => r??,
        r = http_task => r??,
        _ = tokio::signal::ctrl_c() => {
            info!("收到 Ctrl-C，退出");
        }
    }

    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// 端口被占时给一句能照做的话。
///
/// 「Address already in use」这句话本身不说明是谁占的，也不说明该怎么办；
/// 而 mock 与真中台**故意用同一个端口**，所以最常见的成因就是真中台还开着。
fn bind_failure(face: &str, addr: SocketAddr, err: std::io::Error) -> Box<dyn std::error::Error> {
    if err.kind() == std::io::ErrorKind::AddrInUse {
        return format!(
            "{face}绑不上 {addr}：端口已被占用。\n\
             mock 与真中台刻意用同一个端口（这样插件的 HUB_ADDR 不用改），\
             所以最常见的原因是**真中台还开着**——先把它停掉。\n\
             另一个进程占着的话，用 HUB_MOCK_GRPC_ADDR / HUB_MOCK_HTTP_ADDR 换端口，\
             但记得插件的 HUB_ADDR 也要跟着改。"
        )
        .into();
    }
    format!("{face}绑不上 {addr}：{err}").into()
}

/// 把一个 `TcpListener` 变成 tonic 要的 stream。
fn listener_stream(listener: tokio::net::TcpListener) -> tokio_stream::wrappers::TcpListenerStream {
    tokio_stream::wrappers::TcpListenerStream::new(listener)
}
