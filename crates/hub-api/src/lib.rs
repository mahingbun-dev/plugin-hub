//! HTTP 面（axum）：Ingress / Admin / 健康 / 指标。
//!
//! 生产只监听回环地址（`HUB_HTTP_HOST=127.0.0.1`），经 nginx 的 `/hub-api/`
//! 路径前缀对外，与 `anc-frontend` 控制台**同源**——零跨域、SSO Cookie 直接可用。
//!
//! ⚠️ 管理面按设计**不含中台内置鉴权**：鉴权由插件承担（含管理面），主机面运维通道
//! 是唯一的逃生口。这是刻意的设计意图，不是遗漏，详见 `docs/design.md` 的风险表。
//!
//! 两组路由的状态刻意分开：探活与指标**不依赖任何业务状态**（数据库连不上时
//! 它们仍要能回答），业务与管理面才需要 `Store` / `Registry` / `Invoker`。

pub mod admin;
pub mod authz;
pub mod bus;
pub mod error;
pub mod flows;
pub mod governance;
pub mod health;
pub mod ingress;
pub mod metrics;
pub mod payload;
pub mod templates;
pub mod trace;
pub mod triggers;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{delete, get, post};
use hub_engine::{AsyncExecutor, FlowService, Invoker};
use hub_registry::Registry;
use hub_store::Store;
use metrics_exporter_prometheus::PrometheusHandle;

pub use error::ApiError;

/// 探活与指标的状态。
#[derive(Clone)]
pub struct SystemState {
    /// Prometheus 指标句柄。
    ///
    /// 为 `None` 时 `/metrics` 返回 503。之所以是 `Option`：全局 recorder 在一个进程里
    /// 只能安装一次，单测无法各自安装，因此测试以 `None` 构造路由。
    pub metrics: Option<PrometheusHandle>,

    /// MCP 面**对外**的接入端点（来自 `HUB_MCP_PUBLIC_ENDPOINT`）。
    ///
    /// 为 `None` 时 `/endpoints` 返回 `mcp: null`，控制台退回从浏览器地址推导的
    /// 域名形态。IP 这类接入方真正要用的地址是部署属性，中台自己不知道，
    /// 只能由配置给出——与 `HUB_PLUGIN_PUBLIC_ADDR` 同一个「不猜」。
    pub mcp_endpoint: Option<String>,
}

impl SystemState {
    pub fn without_metrics() -> Self {
        Self {
            metrics: None,
            mcp_endpoint: None,
        }
    }
}

/// 业务面与管理面的状态。
#[derive(Clone)]
pub struct ApiState {
    pub store: Store,
    pub registry: Registry,
    pub invoker: Invoker,

    /// 编排服务：草稿、发布、触发、执行记录
    pub flows: FlowService,

    /// 异步链执行器。
    ///
    /// `Option` 而不是必需：探活、契约查询、同步触发都不需要总线，而单元测试也不该
    /// 为了构造一个路由就去连 Redis。没配时异步触发与死信重放返回 503——**明确的
    /// 「这个能力没开」**，比让调用方以为消息已经发出去了好。
    pub async_exec: Option<AsyncExecutor>,

    /// 管理面的鉴权配置。
    ///
    /// `None` = **这一层不生效**，管理面维持原样（无内置守卫）。这是过渡期的形态：
    /// 按设计管理面最终是全插件化的，但 UAT 上还没部署 auth 插件，
    /// 直接启用会让管理面在插件就位之前谁也进不去。见 [`authz`] 的模块说明。
    pub authz: Option<authz::AuthzConfig>,

    /// 中台插件面**对外**的可达地址（`HUB_PLUGIN_PUBLIC_ADDR`）。
    ///
    /// 下载插件工程时回填进 `HUB_ADDR`。`None` 时模板里留可见的占位文字并在
    /// 列表接口里标出来，控制台据此提示开发者自行填写——**不猜**：地址填错是
    /// L3「中台接受注册」永远过不去的头号原因，而报错只有一句 connection refused。
    pub plugin_public_addr: Option<String>,
}

impl ApiState {
    pub fn new(store: Store, registry: Registry, invoker: Invoker, flows: FlowService) -> Self {
        Self {
            store,
            registry,
            invoker,
            flows,
            async_exec: None,
            authz: None,
            plugin_public_addr: None,
        }
    }

    /// 配上插件面对外地址。不配时下载的工程里留占位文字。
    pub fn with_plugin_public_addr(mut self, addr: Option<String>) -> Self {
        self.plugin_public_addr = addr;
        self
    }

    /// 配上管理面的鉴权。不配就是这一层不生效。
    pub fn with_authz(mut self, authz: authz::AuthzConfig) -> Self {
        self.authz = Some(authz);
        self
    }

    /// 配上异步链执行器，开启异步触发与死信重放。
    pub fn with_async(mut self, exec: AsyncExecutor) -> Self {
        self.async_exec = Some(exec);
        self
    }

    /// 取异步执行器，没配就是 503。
    pub fn async_exec(&self) -> Result<&AsyncExecutor, ApiError> {
        self.async_exec
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("本实例未启用异步链（总线未装配）"))
    }
}

/// 探活与指标。数据库不可用时也必须能回答。
pub fn system_router(state: SystemState) -> Router {
    Router::new()
        .route("/health", get(health::health))
        .route("/metrics", get(metrics::metrics))
        // 对外接入端点。挂在 system 面而不是 api 面：它与 /plugin-templates 同属
        // 「接入用的资料」——按设计不鉴权（api 面的鉴权中间件只覆盖 merge 进它的路由，
        // system 面天然在其之外）
        .route("/endpoints", get(health::endpoints))
        .with_state(state)
}

/// 业务数据入口、编排与管理面。
pub fn api_router(state: ApiState) -> Router {
    Router::new()
        // 业务数据入口。不鉴权——鉴权由插件承担
        .route("/ingress/{plugin}", post(ingress::ingress))
        // 插件脚手架模板：列语言、按语言下载一份可运行的插件工程。
        // 同样不鉴权（见 `authz::required_scope` 里的说明）：它们是接入用的开发资料，
        // 不是运行中的数据；管理面鉴权管的是「谁能看/改运行中的东西」。
        .route("/plugin-templates", get(templates::list))
        .route(
            "/plugin-templates/{lang}/download",
            get(templates::download),
        )
        // 管理面。按设计无内置守卫，过渡期由 nginx 在部署层限制来源
        .route("/admin/plugins", get(admin::list_plugins))
        .route("/admin/plugins/{name}", get(admin::get_plugin))
        .route(
            "/admin/plugins/{name}/versions/{version}",
            delete(admin::delete_plugin_version),
        )
        .route("/admin/instances", get(admin::list_instances))
        // 注册拒绝留痕。「实例 0 个但容器活着」的答案——拒绝原因的可查询面
        .route("/admin/rejections", get(admin::list_rejections))
        .route("/admin/plugin-audit", get(admin::list_plugin_audit))
        .route("/admin/messages/{fq_name}", get(admin::message_usage))
        // 治理面：实例级的并发 / 熔断 / 背压此刻的状态。
        // 数字全在中台进程内存里，不落库——多实例部署时每个中台只知道自己那一份
        .route("/admin/governance", get(governance::governance))
        // 编排：读
        .route("/flows", get(flows::list_flows))
        .route("/flows/{flow}", get(flows::get_flow))
        .route("/runs", get(flows::list_runs))
        .route("/runs/{run_id}", get(flows::get_run))
        // 调用链：控制台的「这条数据卡在哪一跳」
        .route("/traces", get(flows::list_traces))
        .route("/traces/{trace_id}", get(flows::get_trace))
        // 编排：写。**发布刻意只在这里暴露、不进 MCP 工具面**——
        // 它直接改变生产流量走向，按设计 agent 只能改草稿
        .route("/flows/{flow}/draft", post(flows::save_draft))
        .route("/flows/{flow}/publish", post(flows::publish))
        .route("/flows/{flow}/rename", post(flows::rename))
        .route("/flows/{flow}/trigger", post(flows::trigger))
        // 异步触发。与同步触发分成两个端点而不是加一个 mode 字段：同步返回执行结果，
        // 异步返回一个句柄，两者响应语义完全不同，混在一个端点里迟早被读错
        .route("/flows/{flow}/trigger-async", post(bus::trigger_async))
        // 超限载荷的引用通道。带 TTL 的限时中转，不是归档
        .route("/blobs/{id}", get(payload::get_blob))
        // 死信：重投耗尽之后的消息，可查看、可重放
        .route("/dead-letters", get(bus::list_dead_letters))
        .route("/dead-letters/{id}", get(bus::get_dead_letter))
        .route("/dead-letters/{id}/replay", post(bus::replay_dead_letter))
        // 触发器：让一条已发布的编排多一个被触发的入口。
        // 与「发布」不同，它不改变编排本身，所以它在 MCP 工具面里也有对应工具
        .route("/triggers", get(triggers::list_triggers))
        .route("/flows/{flow}/triggers", post(triggers::save_trigger))
        .route("/triggers/{id}", delete(triggers::delete_trigger))
        .route("/triggers/{id}/enabled", post(triggers::set_enabled))
        // 请求体上限必须**大于内联上限**，否则引用通道够不着：超限请求会在进到
        // handler 之前就被挡成 413，那段「落库 + 给 uri」的代码一行都跑不到。
        // axum 的默认值是 2MB，比 4MB 的内联上限还小，正是这个坑。
        .layer(DefaultBodyLimit::max(payload::MAX_REQUEST_BYTES))
        .with_state(state)
}

/// 完整 HTTP 面（不含外部面）。
pub fn router(system: SystemState, api: ApiState) -> Router {
    router_with_extras(system, api, Router::new())
}

/// 完整 HTTP 面 + 外部面（如 MCP 面），**鉴权统一挂在最后一步**。
///
/// 为什么要有这个函数：`Router::layer` 只作用于调用它时**已经存在**的路由，
/// 之后 `merge` 进来的不受影响（axum 文档原话："Additional routes added after
/// `layer` is called will not have the middleware added"）。
///
/// 鉴权曾经挂在 `api_router` 内部，于是任何由调用方 `merge` 进来的面都绕过了它
/// ——`/mcp` 就是这样漏的：`required_scope` 里写着 `/mcp → hub:invoke`，
/// 看着对、实际是死代码，而且不报错、不打日志。
///
/// 把挂载点收到这里之后，**新增的面只要走这个入口就不会漏**，
/// 不再依赖「记得给它补一层」这种约定。
pub fn router_with_extras(system: SystemState, api: ApiState, extras: Router) -> Router {
    let authz_state = api.clone();
    system_router(system).merge(
        api_router(api)
            .merge(extras)
            // 鉴权在最外层（后加的 layer 先执行）：未认证的请求不该被读一遍 body。
            // 没配 auth 插件时这一层直接放行，见 `authz` 的模块说明
            .layer(middleware::from_fn_with_state(
                authz_state,
                authz::authorize,
            )),
    )
}
