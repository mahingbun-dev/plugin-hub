//! plugin-hub 入口：装配配置、日志、指标、存储、注册表与两个网络面。
//!
//! 两个面绑定的地址刻意不同（见 `docs/design.md`）：
//! HTTP 面只绑回环（经 nginx 同源暴露给控制台），插件面必须全网卡可达（远程插件要注册）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use hub_api::{ApiState, SystemState, health};
use hub_bus::{Bus, BusConfig};
use hub_core::Config;
use hub_engine::{AsyncExecutor, FlowExecutor, FlowService, Invoker};
use hub_plugin_client::{PluginClient, PluginClientConfig};
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_server::{http_app, scheduler, tasks};
use hub_store::Store;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// 数据库连接池上限。
///
/// 原注释以「跨机访问 UAT 的 PG、池子开太大反而拖慢」为由——那个前提已不成立
/// （PG 现在是部署自带的容器，走宿主回环）。16 这个值本身仍然合理，故保留。
const DB_MAX_CONNECTIONS: u32 = 16;

/// 摘除心跳超时实例的巡检周期。
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// 消费阻塞时长。到点没消息就返回空，让消费循环有机会看一眼关停信号。
const BUS_BLOCK: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env().context("加载配置失败")?;
    init_tracing(&cfg.log_level);

    health::mark_started();
    let metrics = install_metrics_recorder();

    let store = Store::connect(&cfg.database_url, DB_MAX_CONNECTIONS)
        .await
        .context("连接数据库失败")?;
    store.migrate().await.context("执行数据库迁移失败")?;
    info!("数据库已就绪");

    // 注册表用的探测实现就是真实的插件 gRPC 客户端：
    // 注册流程里的「可达性探测」与实际调用走同一条链路
    let plugin_client = Arc::new(PluginClient::new(PluginClientConfig::default()));
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&plugin_client) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );
    let invoker = Invoker::new(registry.clone(), (*plugin_client).clone()).with_governor(
        hub_engine::Governor::new(hub_engine::GovernorConfig {
            max_concurrency: cfg.node_max_concurrency as usize,
            queue_timeout: Duration::from_millis(cfg.node_queue_timeout_ms),
            failure_threshold: cfg.breaker_failure_threshold,
            open_cooldown: Duration::from_secs(cfg.breaker_cooldown_secs),
        }),
    );
    // 治理器要在 invoker 被移进 ApiState 之前留一份：巡检任务靠它清理下线实例的表项
    let governor = invoker.governor().clone();
    let flows = FlowService::new(
        store.clone(),
        FlowExecutor::new(registry.clone(), invoker.clone()),
    )
    .with_exporter(hub_observe::exporter_for(cfg.otlp_endpoint.as_deref()));

    // 异步链：消费者、留存巡检都由后台任务驱动，与两个网络面共用同一次关停。
    //
    // **每个消费者一条 Bus**：Redis 按消费者名字判断「谁的活卡住了」，同一个进程里
    // 的多个消费者共用名字的话，`XAUTOCLAIM` 会让它们互相抢对方正在处理的活。
    let mut executors = Vec::with_capacity(cfg.async_workers as usize);
    for index in 0..cfg.async_workers {
        let bus = connect_bus(&cfg, Some(index)).await?;
        executors.push(AsyncExecutor::new(
            store.clone(),
            bus,
            FlowExecutor::new(registry.clone(), invoker.clone()),
        ));
    }
    // 巡检用的这条不计入消费者编号：它只按时间扫，不参与接管
    let retention_bus = connect_bus(&cfg, None).await?;

    match cfg.otlp_endpoint.as_deref() {
        Some(endpoint) => info!(endpoint, "span 将同步导出到 OTLP 后端"),
        None => info!("未配置 OTLP_ENDPOINT，span 只自存不导出"),
    }

    let http_addr = SocketAddr::new(cfg.http_host, cfg.http_port);
    let grpc_addr = SocketAddr::new(cfg.grpc_host, cfg.grpc_port);

    let listener = TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("HTTP 面绑定失败: {http_addr}"))?;
    info!(%http_addr, "HTTP 面已监听");

    // MCP 的会话与流式响应需要自己的取消信号，与两个网络面共用同一次关停
    let mcp_shutdown = CancellationToken::new();
    let mcp = hub_mcp::HubMcp::new(store.clone(), invoker.clone(), flows.clone())
        // 与 HTTP 面共用同一个执行器：agent 与调用方看到的是同一条流
        .with_async(executors[0].clone())
        // 经反向代理对外时，代理传的是**原始** Host。不配这个，外部 agent
        // 打进来的每一个 /mcp 请求都会被当成 DNS rebinding 判 403——
        // 而直连 127.0.0.1 与本地测试都天然通过，所以这个问题只在线上暴露
        .with_allowed_hosts(cfg.mcp_allowed_hosts.clone())
        .with_login_gate(cfg.mcp_login_gate)
        .router(mcp_shutdown.clone());

    match cfg.mcp_allowed_hosts.as_deref() {
        Some(hosts) => info!(hosts = ?hosts, "MCP 面 Host 白名单已配置"),
        None => info!("MCP 面 Host 白名单未配置，沿用默认（只接受本机 Host）"),
    }
    if cfg.mcp_login_gate {
        info!("MCP 登录闸门已开启：插件调用前需经 elicitation 登录建立身份");
    }

    // 网关代调与 flow / MCP 面共用同一个 Invoker：同一套治理、熔断与实例路由。
    // **必须在 invoker 被 move 进 ApiState 之前 clone**
    let gateway_invoker = invoker.clone();
    // 管理面鉴权。**没配 auth 插件时这一层不生效**——过渡期形态，
    // 见 hub_api::authz 的模块说明
    let api_state = ApiState::new(store.clone(), registry.clone(), invoker, flows)
        // 第一个消费者兼任 HTTP 面的异步触发入口：两者共用同一份总线配置，
        // 分开构造反而会出现「HTTP 说入队成功、消费者看的是另一条流」这种事
        .with_async(executors[0].clone());

    let api_state = match cfg.auth_plugin.as_deref() {
        Some(plugin) => {
            info!(plugin, "管理面鉴权已启用");
            api_state.with_authz(hub_api::authz::AuthzConfig {
                plugin: plugin.to_string(),
                version: cfg.auth_plugin_version.clone(),
            })
        }
        None => {
            warn!(
                "未配置 HUB_AUTH_PLUGIN，管理面没有内置守卫——\
                 按设计鉴权由插件承担，过渡期靠 nginx 限制来源网段"
            );
            api_state
        }
    };

    // 插件面对外地址：五门模板的 AGENTS.md 里那句「本工程里填的是 …」回填的就是它
    // （README 里四门有、Go 那份没有，理由见 `hub_api::templates` 的说明）。
    //
    // **没配不猜**：下载下来的工程里留一段可读的占位文字，列表接口同时返回
    // `plugin_addr_configured: false`，控制台的卡片据此提示开发者自行确认。
    // 猜一个错地址的代价是 L3「中台接受注册」永远过不去，而报错只有一句
    // connection refused——那是最难查的一类问题。
    let api_state = api_state.with_plugin_public_addr(cfg.plugin_public_addr.clone());
    match cfg.plugin_public_addr.as_deref() {
        Some(addr) => info!(addr, "插件面对外地址已配置"),
        None => warn!(
            "未配置 HUB_PLUGIN_PUBLIC_ADDR：控制台的模板卡片会标出「中台地址未配置」，\
             下发的工程里那一段是占位文字，开发者要自己填"
        ),
    }

    // 组装走 `hub_server::http_app` 而不是自己 merge：**鉴权挂在那条链的最后一步**，
    // 只有走那个入口，merge 进来的 MCP 面才会被覆盖到。自己 `.merge(mcp)` 会漏掉它
    // ——那条路已经踩过一次，见 `hub_api::router_with_extras` 的说明。
    match cfg.mcp_public_endpoint.as_deref() {
        Some(endpoint) => info!(endpoint, "MCP 对外接入端点已配置（GET /endpoints 下发）"),
        None => info!(
            "未配置 HUB_MCP_PUBLIC_ENDPOINT，/endpoints 的 mcp 为 null，\
             控制台退回从浏览器地址推导的域名形态"
        ),
    }
    let app = http_app(
        SystemState {
            metrics,
            mcp_endpoint: cfg.mcp_public_endpoint.clone(),
        },
        api_state,
        mcp,
    );

    // 两个面共用一个关停信号
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tasks::spawn_consumers(executors.clone(), shutdown_rx.clone());

    // 触发器调度。cron 与 MQ 各一个循环，都用第一个消费者作执行器——触发只是把消息
    // 投上总线，真正的执行由消费循环负责，两者不必是同一个实例。
    scheduler::spawn_cron(store.clone(), executors[0].clone(), shutdown_rx.clone());
    scheduler::spawn_mq(
        store.clone(),
        executors[0].clone(),
        cfg.redis_url.clone(),
        shutdown_rx.clone(),
    );
    tasks::spawn_retention(
        store.clone(),
        retention_bus,
        tasks::RetentionPolicy {
            stream: Duration::from_secs(cfg.stream_retention_hours as u64 * 3600),
            runs: Duration::from_secs(cfg.run_retention_days as u64 * 86_400),
            spans: Duration::from_secs(cfg.span_retention_days as u64 * 86_400),
            dead_letters: Duration::from_secs(cfg.audit_retention_days as u64 * 86_400),
            rejections: Duration::from_secs(cfg.rejection_retention_days as u64 * 86_400),
        },
        shutdown_rx.clone(),
    );
    info!(
        workers = cfg.async_workers,
        max_depth = cfg.bus_max_depth,
        "异步链与触发器调度已启动"
    );

    let http_task = {
        let mut rx = shutdown_rx.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.changed().await;
                })
                .await
        })
    };

    // 状态面（HubState）用的 Redis 连接。**与总线的连接分开**：一个是队列、一个是
    // 键值，共用一条连接会让一边的慢操作顶住另一边。
    //
    // 连接由 `hub_grpc::state::connect_redis` 建：响应超时（默认 500ms 会误杀正常
    // 请求）只能在建连时设置，封装在那个构造函数里，调用方不必也不该照抄配置。
    let state_redis = hub_grpc::state::connect_redis(&cfg.redis_url)
        .await
        .context("连接 Redis（状态面）失败")?;

    let grpc_task = {
        let mut rx = shutdown_rx.clone();
        let registry = registry.clone();
        let state_redis = state_redis.clone();
        // `Publish` 用它把信封投给下游 flow。**与 HTTP / MCP 面共用同一个执行器**
        // ——三者必须是同一份总线配置，否则会出现「一边说入队成功、另一边看的是
        // 另一条流」，而那种偏差只在消息真的不上不下时才暴露。
        let async_exec = executors[0].clone();
        // 插件互调网关。策略来自 `HUB_PLUGIN_CALL_POLICY`（validate 已保证只有
        // allow / declared 两值，这里不用再防第三种）；下游代调与 flow / MCP 面
        // 共用同一个 Invoker。
        let call_policy = match cfg.plugin_call_policy.as_str() {
            "declared" => hub_grpc::CallPolicy::Declared,
            _ => hub_grpc::CallPolicy::Allow,
        };
        let gateway = hub_grpc::GatewayService::new(store.pool().clone(), state_redis.clone())
            .with_invoker(gateway_invoker)
            .with_call_policy(call_policy);
        tokio::spawn(async move {
            hub_grpc::serve(
                grpc_addr,
                registry,
                state_redis,
                Some(async_exec),
                Some(gateway),
                async move {
                    let _ = rx.changed().await;
                },
            )
            .await
        })
    };

    // 巡检任务随进程退出而结束，不需要单独协调关停
    spawn_sweeper(registry.clone(), store.clone(), governor);

    // 主机面运维通道。
    //
    // 管理面按设计是全插件化的，这条通道是 auth 插件坏掉时唯一的逃生口。
    // config_summary 刻意只放端口与留存策略——**不放任何连接串或凭据**：
    // status 的输出可能被贴进工单或聊天窗口。
    let ops_path = std::path::PathBuf::from(&cfg.ops_socket);
    let ops_state = hub_ops::OpsState {
        store: store.clone(),
        config_summary: serde_json::json!({
            "http_host": cfg.http_host.to_string(),
            "http_port": cfg.http_port,
            "grpc_host": cfg.grpc_host.to_string(),
            "grpc_port": cfg.grpc_port,
            "ops_socket": cfg.ops_socket,
            "span_retention_days": cfg.span_retention_days,
            "audit_retention_days": cfg.audit_retention_days,
            "stream_retention_hours": cfg.stream_retention_hours,
            "node_max_concurrency": cfg.node_max_concurrency,
            "node_queue_timeout_ms": cfg.node_queue_timeout_ms,
            "breaker_failure_threshold": cfg.breaker_failure_threshold,
            "breaker_cooldown_secs": cfg.breaker_cooldown_secs,
            // 插件互调的授权口径（allow/declared）。运维要能一眼确认当前
            // 收紧到哪一档，而不是去猜 env 有没有生效
            "plugin_call_policy": cfg.plugin_call_policy,
        }),
    };
    let ops_task = {
        let mut rx = shutdown_rx.clone();
        let path = ops_path.clone();
        tokio::spawn(async move {
            if let Err(err) = hub_ops::serve(&path, ops_state, async move {
                let _ = rx.changed().await;
            })
            .await
            {
                error!(error = %err, socket = %path.display(), "运维通道异常退出");
            }
        })
    };

    shutdown_signal().await;
    info!("开始优雅退出");
    let _ = shutdown_tx.send(true);
    mcp_shutdown.cancel();

    match http_task.await {
        Ok(Ok(())) => info!("HTTP 面已停止"),
        Ok(Err(err)) => error!(error = %err, "HTTP 面异常退出"),
        Err(err) => error!(error = %err, "HTTP 面任务 panic"),
    }
    match grpc_task.await {
        Ok(Ok(())) => info!("插件面已停止"),
        Ok(Err(err)) => error!(error = %err, "插件面异常退出"),
        Err(err) => error!(error = %err, "插件面任务 panic"),
    }
    if let Err(err) = ops_task.await {
        error!(error = %err, "运维通道任务 panic");
    }

    store.close().await;
    info!("已退出");
    Ok(())
}

/// 按配置连一条总线。`worker` 给定时在该消费者名后加编号。
async fn connect_bus(cfg: &Config, worker: Option<u32>) -> anyhow::Result<Bus> {
    let consumer = match worker {
        Some(index) => format!("{}-{index}", default_consumer_name()),
        None => format!("{}-retention", default_consumer_name()),
    };

    Bus::connect(
        &cfg.redis_url,
        BusConfig {
            consumer,
            block: BUS_BLOCK,
            claim_min_idle: Duration::from_secs(cfg.bus_claim_min_idle_secs),
            max_delivery: cfg.bus_max_delivery,
            max_depth: cfg.bus_max_depth,
            ..BusConfig::default()
        },
    )
    .await
    .context("连接 Redis 总线失败")
}

/// 本进程的消费者名前缀。主机名 + 进程号：多实例部署时不能撞。
fn default_consumer_name() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "hub".to_string());
    format!("{host}-{}", std::process::id())
}

/// 周期性摘除心跳超时的实例，并清理治理表里已消失的实例。
///
/// 治理表按实例建项，插件反复重启会不断产生新的 instance_id。不清理就是一条慢速泄漏。
/// 放在这个任务里做是自然的：摘除刚跑完，正是「哪些实例已经没了」最清楚的时刻。
fn spawn_sweeper(registry: Registry, store: Store, governor: hub_engine::Governor) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        // 第一次 tick 会立即触发，跳过以免启动瞬间做无意义的巡检
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match registry.sweep_stale().await {
                Ok(removed) if !removed.is_empty() => {
                    info!(count = removed.len(), "已摘除心跳超时的实例");
                }
                Ok(_) => {}
                Err(err) => warn!(error = %err, "摘除巡检失败"),
            }

            // 清理拿不到实例列表时**跳过而不是清空**：把「查不到」当成「都下线了」
            // 会把所有存活实例的失败计数抹掉，刚跳闸的实例立刻满血复活。
            match hub_store::instances::list_instances(store.pool()).await {
                Ok(alive) => {
                    let alive: std::collections::HashSet<String> =
                        alive.into_iter().map(|i| i.instance_id).collect();
                    let pruned = governor.prune(&alive);
                    if pruned > 0 {
                        info!(count = pruned, "已清理下线实例的治理表项");
                    }
                }
                Err(err) => warn!(error = %err, "查询存活实例失败，本次跳过治理表清理"),
            }
        }
    });
}

/// 初始化日志。`LOG_LEVEL` 非法时回退到 info 而不是拒绝启动——
/// 一个日志级别写错不应该让中台起不来。
fn init_tracing(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|err| {
        eprintln!("LOG_LEVEL={level} 非法（{err}），回退到 info");
        EnvFilter::new("info")
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// 安装全局 Prometheus recorder。一个进程只能安装一次。
///
/// 安装失败不阻断启动：探活与编排才是主链路，指标可以退化为不可用（`/metrics` 返回 503）。
fn install_metrics_recorder() -> Option<PrometheusHandle> {
    match PrometheusBuilder::new().install_recorder() {
        Ok(handle) => Some(handle),
        Err(err) => {
            error!(error = %err, "Prometheus recorder 安装失败，/metrics 将返回 503");
            None
        }
    }
}

/// 等待 SIGINT / SIGTERM。
///
/// 仅支持 Unix——部署目标（UAT）与开发机均为 Unix。
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("注册 SIGINT 处理器失败");
    };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("注册 SIGTERM 处理器失败")
            .recv()
            .await;
    };

    tokio::select! {
        () = ctrl_c => info!("收到 SIGINT，开始优雅退出"),
        _ = terminate => info!("收到 SIGTERM，开始优雅退出"),
    }
}
