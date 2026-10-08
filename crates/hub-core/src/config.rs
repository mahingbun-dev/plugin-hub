//! 中台运行配置：全部来自环境变量。
//!
//! 端口分配（详见 `docs/design.md`）：
//!
//! - **HTTP 面 8092（默认值）**：Ingress / Admin / MCP / 健康 / 指标。默认只绑回环，
//!   经 nginx 的 `/hub-api/` 路径前缀对外，与 `anc-frontend` 控制台**同源**。
//!   ⚠️ **UAT 上不用这个默认值**：8092 已归 anc 平台后端，部署时由 `.env` 的
//!   `HUB_HTTP_PORT=8095` 覆盖（见 `deploy/.env.example`）。
//! - **插件面 8093**：插件注册 / 心跳 / 回调（gRPC）。默认绑全网卡，
//!   经 nginx `grpc_pass` 以 TLS（外部端口 8094）对外，供**其他主机**上的插件注册。
//!
//! 两个端口的绑定差异是刻意的：HTTP 面不该被外网直接访问，插件面则必须可达。

use std::env;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

/// HTTP 面默认端口。
///
/// 这只是**没有配置时的回落值**，不是部署值：UAT 上 8092 归 anc 平台后端，
/// 那边由 `.env` 覆盖成 8095（见 `deploy/.env.example` 里的说明）。
pub const DEFAULT_HTTP_PORT: u16 = 8092;
/// 插件面默认端口
pub const DEFAULT_GRPC_PORT: u16 = 8093;

/// 运维通道的默认 socket 路径。
///
/// 选 `/run` 下的独立目录：该目录只有 root 可写，天然构成运维通道的信任边界。
pub const DEFAULT_OPS_SOCKET: &str = "/run/plugin-hub/ops.sock";

pub const DEFAULT_LOG_LEVEL: &str = "info";
pub const DEFAULT_SPAN_RETENTION_DAYS: u32 = 7;
pub const DEFAULT_AUDIT_RETENTION_DAYS: u32 = 90;
pub const DEFAULT_STREAM_RETENTION_HOURS: u32 = 24;

/// 每实例并发上限的默认值。与 `hub_engine::govern::DEFAULT_MAX_CONCURRENCY` 一致。
pub const DEFAULT_NODE_MAX_CONCURRENCY: u32 = 32;
/// 背压排队上限（毫秒）。与 `hub_engine::govern::DEFAULT_QUEUE_TIMEOUT_MS` 一致。
pub const DEFAULT_NODE_QUEUE_TIMEOUT_MS: u64 = 100;
/// 连续失败跳闸阈值。与 `hub_engine::govern::DEFAULT_FAILURE_THRESHOLD` 一致。
pub const DEFAULT_BREAKER_FAILURE_THRESHOLD: u32 = 5;
/// 跳闸冷却（秒）。与 `hub_engine::govern::DEFAULT_OPEN_COOLDOWN_SECS` 一致。
pub const DEFAULT_BREAKER_COOLDOWN_SECS: u64 = 10;

/// 异步链的常驻消费者数。
///
/// 不止一个的理由是**消费者挂掉后的接管**：`XAUTOCLAIM` 按消费者名字判断「谁的活
/// 卡住了」，同一个进程里的多个消费者必须各有各的名字，否则它们会互相抢。
pub const DEFAULT_ASYNC_WORKERS: u32 = 4;

/// 总线堆积上限。到顶时新入队被拒（429）而不是无限堆在 Redis 里。
pub const DEFAULT_BUS_MAX_DEPTH: usize = 100_000;

/// 一条消息最多投递几次。用完进死信。
pub const DEFAULT_BUS_MAX_DELIVERY: i64 = 5;

/// 闲置多久的消息视为「原消费者已死」，可被接管重投（秒）。
pub const DEFAULT_BUS_CLAIM_MIN_IDLE_SECS: u64 = 60;

/// 执行记录的保留天数。
///
/// 比 span（7 天）长得多：run/run_node 是「这条数据卡在哪一跳」的答案，属于排障依据；
/// span 是调用链的细节，过了窗口就没那么要紧了。
pub const DEFAULT_RUN_RETENTION_DAYS: u32 = 90;

/// 注册拒绝留痕的保留天数。
///
/// 拒绝是「现在进行时」的问题——被拒的插件只要还在跑就会每几秒重试一次，
/// 留痕随之刷新（`last_seen_at` 永远新鲜）；一行 30 天没人再来重试，
/// 说明问题早就没了（修好或下线），记录可以清了。
pub const DEFAULT_REJECTION_RETENTION_DAYS: u32 = 30;

/// 插件互调策略的默认值。
///
/// 默认 `allow` 而不是 `declared`：M6 落地时存量插件一个都没声明过 `invokes`，
/// 默认收紧等于让所有互调当场死掉。治理收紧是部署者的显式决定（env 覆盖），
/// 不是中台替人做的默认。
pub const DEFAULT_PLUGIN_CALL_POLICY: &str = "allow";

/// 中台运行配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// HTTP 面监听地址；生产为 127.0.0.1（只经 nginx 对外）
    pub http_host: IpAddr,
    pub http_port: u16,
    /// 插件面监听地址；生产为 0.0.0.0（远程插件必须能连上）
    pub grpc_host: IpAddr,
    pub grpc_port: u16,

    /// 主机面运维通道的 unix socket 路径。
    ///
    /// 管理面按设计是全插件化的（中台内不含鉴权逻辑），**这条通道是唯一的逃生口**：
    /// auth 插件自己坏了、HTTP 面进不去时靠它把中台救回来。信任边界是文件系统权限，
    /// 因此路径应落在只有 root 能访问的目录里。
    pub ops_socket: String,

    pub database_url: String,
    pub redis_url: String,
    pub log_level: String,
    pub span_retention_days: u32,
    pub audit_retention_days: u32,
    pub stream_retention_hours: u32,
    /// 注册拒绝留痕的保留天数
    pub rejection_retention_days: u32,

    /// OTLP/HTTP 的 trace 后端端点，例如 `http://127.0.0.1:4318/v1/traces`。
    ///
    /// **缺省不启用**：UAT 目前没有 trace 后端，没配就不该有任何网络动作。
    /// span 本身已经自存（控制台按 traceId 能确定性地查到链路），导出只是「顺便也推一份」。
    pub otlp_endpoint: Option<String>,

    /// 每个插件实例的在途调用上限。
    ///
    /// 到顶之后的动作是**背压快速失败**而不是无限排队：见 `hub_engine::govern`。
    pub node_max_concurrency: u32,

    /// 背压时最多排队多久（毫秒）。等不到就报 Overloaded。
    pub node_queue_timeout_ms: u64,

    /// 实例连续失败多少次跳闸。
    pub breaker_failure_threshold: u32,

    /// 跳闸后多久放一个探测请求过去（秒）。
    pub breaker_cooldown_secs: u64,

    /// 异步链的常驻消费者数。每个消费者在 Redis 里是独立身份，才能互相接管。
    pub async_workers: u32,

    /// 总线堆积上限。到顶时新入队被拒而不是无限堆在 Redis 里。
    pub bus_max_depth: usize,

    /// 一条消息最多投递几次；用完进死信。
    pub bus_max_delivery: i64,

    /// 闲置多久的消息可被接管重投（秒）。
    pub bus_claim_min_idle_secs: u64,

    /// 执行记录保留天数。
    pub run_retention_days: u32,

    /// 引导模式凭据。
    ///
    /// 管理面按设计是全插件化的（中台内不含鉴权逻辑），主机面运维通道是唯一逃生口。
    /// 该凭据只在「库中尚无任何插件」时有意义，用于完成首次配置。
    pub bootstrap_token: Option<String>,

    /// 管理面鉴权用的 auth 插件名。
    ///
    /// **留空即这一层不生效**，管理面维持原样（无内置守卫）——这是过渡期的形态。
    /// 之所以给开关而不是直接启用：auth 插件在 UAT 上还没部署，
    /// 直接启用会让管理面在插件就位之前谁也进不去。
    pub auth_plugin: Option<String>,

    /// auth 插件的版本约束；留空跟随最新版本。
    pub auth_plugin_version: Option<String>,

    /// 中台插件面**对外**的可达地址，用于回填进下载的插件工程（`HUB_ADDR`）。
    ///
    /// 中台自己不知道自己对外是什么地址：它监听的是 `grpc_host:grpc_port`，而插件连的
    /// 是经 nginx 做 TLS 终结之后的域名（UAT 是 `https://hub.example.com:8094`）。
    ///
    /// **留空时不猜**：模板里留一段可见的占位文字，列表接口返回
    /// `plugin_addr_configured: false`，控制台据此提示开发者自行填写。
    /// 猜错地址的代价是 L3「中台接受注册」永远过不去，而报错只有一句 connection refused。
    pub plugin_public_addr: Option<String>,
    /// MCP 面的 Host 白名单（防 DNS rebinding）。
    ///
    /// **留空 = 沿用 rmcp 的默认**（`localhost` / `127.0.0.1` / `::1`），即只接受
    /// 本机 Host。中台经反向代理对外时**必须显式列出对外域名**：反向代理默认
    /// 把原始 Host 原样传下来，不在白名单里的请求会被
    /// **403 `Forbidden: Host header is not allowed`**。
    ///
    /// 这条只在跨机部署时暴露——直连 `127.0.0.1:8095`、以及本机测试与 vite 代理
    /// 都天然通过，所以本地怎么测都测不出来。UAT 上 2026-09-18 就是这么撞上的。
    ///
    /// 逗号分隔。**不带端口的项放行该域名的任意端口**，带端口的精确匹配
    /// （语义由 rmcp 的 `host_is_allowed` 决定）。
    pub mcp_allowed_hosts: Option<Vec<String>>,
    /// MCP 面的**登录闸门**：插件工具调用前先向用户弹账号密码（elicitation），
    /// 经 auth 插件 `login` 换登录态并建立调用身份。**默认关闭**——关闭时 MCP
    /// 面维持匿名调用（与历史行为一致）；UAT 部署把它打开。
    ///
    /// 为什么不是 HUB_AUTH_PLUGIN 那层 Cookie 鉴权：CLI 侧要配浏览器 Cookie、
    /// 会过期、要手工维护。登录闸门把「问账号密码」放进调用链路的第一次
    /// 插件调用里，弹窗由客户端原生渲染，密码不经过模型上下文。
    /// 管理类工具（list_plugins 等）不受闸门约束——那是权限位体系的事。
    pub mcp_login_gate: bool,

    /// MCP 面**对外**的接入端点（完整 URL），控制台的「插件目录」页据此显示。
    ///
    /// 接入方（agent 所在的机器）不一定解析得了对外域名——UAT 上 agent 走 IP
    /// （`https://203.0.113.10:8081/mcp`），而控制台只能从浏览器地址推导出域名形态，
    /// IP 是部署属性，浏览器侧无从得知。与 [`Self::plugin_public_addr`] 同一个道理：
    /// **只有部署者知道，所以只能由配置给出**。
    ///
    /// **留空时不猜**：`GET /endpoints` 返回 `mcp: null`，控制台退回显示
    /// 浏览器推导的地址（域名形态），行为与没加这个配置之前完全一致。
    pub mcp_public_endpoint: Option<String>,

    /// 插件互调的权限策略（env `HUB_PLUGIN_CALL_POLICY`）。
    ///
    /// manifest 的 `invokes` 声明是**可选**的（老插件没有这个字段），没声明时听谁的
    /// 就是这个开关：`allow`（默认）放行未声明的互调，`declared` 要求调用方在其
    /// 最新注册版 manifest 的 `invokes` 里声明目标插件，否则拒绝。合法值只有这两个，
    /// 写错要在启动时拦下（[`Self::validate`]）——策略拼错若被静默当成默认值，
    /// 收紧治理的部署会毫不知情地敞着口子。
    pub plugin_call_policy: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// 必填项缺失或为空
    Missing(&'static str),
    /// 取值无法解析
    Invalid {
        key: &'static str,
        value: String,
        reason: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(key) => write!(f, "缺少必填环境变量 {key}"),
            Self::Invalid { key, value, reason } => {
                write!(f, "环境变量 {key} 取值非法（{value}）：{reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// 从进程环境变量加载。
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|key| env::var(key).ok())
    }

    /// 从任意键值来源加载（测试用，避免依赖进程级环境变量）。
    pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let required = |key: &'static str| -> Result<String, ConfigError> {
            match get(key) {
                Some(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
                _ => Err(ConfigError::Missing(key)),
            }
        };

        let cfg = Self {
            http_host: parsed(&get, "HUB_HTTP_HOST", IpAddr::from([127, 0, 0, 1]))?,
            http_port: parsed(&get, "HUB_HTTP_PORT", DEFAULT_HTTP_PORT)?,
            grpc_host: parsed(&get, "HUB_GRPC_HOST", IpAddr::from([0, 0, 0, 0]))?,
            grpc_port: parsed(&get, "HUB_GRPC_PORT", DEFAULT_GRPC_PORT)?,
            ops_socket: optional(&get, "HUB_OPS_SOCKET")
                .unwrap_or_else(|| DEFAULT_OPS_SOCKET.to_string()),
            database_url: required("DATABASE_URL")?,
            redis_url: required("REDIS_URL")?,
            log_level: optional(&get, "LOG_LEVEL").unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string()),
            span_retention_days: parsed(&get, "SPAN_RETENTION_DAYS", DEFAULT_SPAN_RETENTION_DAYS)?,
            audit_retention_days: parsed(
                &get,
                "AUDIT_RETENTION_DAYS",
                DEFAULT_AUDIT_RETENTION_DAYS,
            )?,
            stream_retention_hours: parsed(
                &get,
                "STREAM_RETENTION_HOURS",
                DEFAULT_STREAM_RETENTION_HOURS,
            )?,
            otlp_endpoint: optional(&get, "OTLP_ENDPOINT"),
            node_max_concurrency: parsed(
                &get,
                "NODE_MAX_CONCURRENCY",
                DEFAULT_NODE_MAX_CONCURRENCY,
            )?,
            node_queue_timeout_ms: parsed(
                &get,
                "NODE_QUEUE_TIMEOUT_MS",
                DEFAULT_NODE_QUEUE_TIMEOUT_MS,
            )?,
            breaker_failure_threshold: parsed(
                &get,
                "BREAKER_FAILURE_THRESHOLD",
                DEFAULT_BREAKER_FAILURE_THRESHOLD,
            )?,
            breaker_cooldown_secs: parsed(
                &get,
                "BREAKER_COOLDOWN_SECS",
                DEFAULT_BREAKER_COOLDOWN_SECS,
            )?,
            async_workers: parsed(&get, "ASYNC_WORKERS", DEFAULT_ASYNC_WORKERS)?,
            bus_max_depth: parsed(&get, "BUS_MAX_DEPTH", DEFAULT_BUS_MAX_DEPTH)?,
            bus_max_delivery: parsed(&get, "BUS_MAX_DELIVERY", DEFAULT_BUS_MAX_DELIVERY)?,
            bus_claim_min_idle_secs: parsed(
                &get,
                "BUS_CLAIM_MIN_IDLE_SECS",
                DEFAULT_BUS_CLAIM_MIN_IDLE_SECS,
            )?,
            run_retention_days: parsed(&get, "RUN_RETENTION_DAYS", DEFAULT_RUN_RETENTION_DAYS)?,
            rejection_retention_days: parsed(
                &get,
                "REJECTION_RETENTION_DAYS",
                DEFAULT_REJECTION_RETENTION_DAYS,
            )?,
            bootstrap_token: optional(&get, "BOOTSTRAP_TOKEN"),
            auth_plugin: optional(&get, "HUB_AUTH_PLUGIN"),
            auth_plugin_version: optional(&get, "HUB_AUTH_PLUGIN_VERSION"),
            plugin_public_addr: optional(&get, "HUB_PLUGIN_PUBLIC_ADDR"),
            mcp_allowed_hosts: split_list(&get, "HUB_MCP_ALLOWED_HOSTS"),
            mcp_login_gate: matches!(
                get("HUB_MCP_LOGIN_GATE")
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .as_str(),
                "1" | "true" | "yes" | "on"
            ),
            mcp_public_endpoint: optional(&get, "HUB_MCP_PUBLIC_ENDPOINT"),
            plugin_call_policy: optional(&get, "HUB_PLUGIN_CALL_POLICY")
                .unwrap_or_else(|| DEFAULT_PLUGIN_CALL_POLICY.to_string()),
        };

        cfg.validate()?;
        Ok(cfg)
    }

    /// 跨字段校验。两个端口若相同，后启动的监听会失败，属配置错误而非运行期偶发。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.http_port == self.grpc_port {
            return Err(ConfigError::Invalid {
                key: "HUB_GRPC_PORT",
                value: self.grpc_port.to_string(),
                reason: format!("不能与 HTTP 面端口 {} 相同", self.http_port),
            });
        }
        for (key, days) in [
            ("SPAN_RETENTION_DAYS", self.span_retention_days),
            ("AUDIT_RETENTION_DAYS", self.audit_retention_days),
            ("STREAM_RETENTION_HOURS", self.stream_retention_hours),
            ("RUN_RETENTION_DAYS", self.run_retention_days),
            ("REJECTION_RETENTION_DAYS", self.rejection_retention_days),
        ] {
            if days == 0 {
                return Err(ConfigError::Invalid {
                    key,
                    value: "0".to_string(),
                    reason: "留存时间必须大于 0".to_string(),
                });
            }
        }

        // 只拦「一定配置错了」的取值，不拦「偏激进」的取值：
        // 并发上限为 0 会让每一次调用都被背压拦下，中台等于死了；而排队 0ms
        // （不排队，直接失败）、冷却 0s（立刻重新探测）都是能用的激进档位。
        if self.node_max_concurrency == 0 {
            return Err(ConfigError::Invalid {
                key: "NODE_MAX_CONCURRENCY",
                value: "0".to_string(),
                reason: "并发上限为 0 会让每一次调用都被背压拦下".to_string(),
            });
        }

        // 同理：没有消费者，队会一直涨到堆积上限然后每一次入队都被拒——中台看起来
        // 「在跑」但什么都不做。多个消费者之间要能互相接管，所以下限是 1。
        if self.async_workers == 0 {
            return Err(ConfigError::Invalid {
                key: "ASYNC_WORKERS",
                value: "0".to_string(),
                reason: "没有消费者会让队列一路涨到堆积上限".to_string(),
            });
        }
        if self.bus_max_depth == 0 {
            return Err(ConfigError::Invalid {
                key: "BUS_MAX_DEPTH",
                value: "0".to_string(),
                reason: "堆积上限为 0 会让每一次入队都被拒".to_string(),
            });
        }

        // 策略值只在两档里二选一，且不做大小写宽容："Allow" 之类多半是手滑，
        // 宽容它等于让「配了 declared」悄悄退化成「没配」。
        if !matches!(self.plugin_call_policy.as_str(), "allow" | "declared") {
            return Err(ConfigError::Invalid {
                key: "HUB_PLUGIN_CALL_POLICY",
                value: self.plugin_call_policy.clone(),
                reason: "只接受 allow 或 declared".to_string(),
            });
        }
        Ok(())
    }
}

fn optional(get: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 逗号分隔的列表。**留空（或只写了逗号/空白）得到 `None`，而不是空 `Vec`**。
///
/// 这个区分不是洁癖：rmcp 把**空列表**解释为「放行全部 Host」，所以若把「没配」
/// 直译成空 `Vec`，等于在用户没表达任何意图的情况下把防 DNS rebinding 的那一层
/// 关掉——而且看起来像配了。`None` 才表示「我没说，用默认」。
fn split_list(get: &impl Fn(&str) -> Option<String>, key: &str) -> Option<Vec<String>> {
    let raw = optional(get, key)?;
    let items: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!items.is_empty()).then_some(items)
}

fn parsed<T>(
    get: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    default: T,
) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match get(key) {
        Some(v) if !v.trim().is_empty() => {
            v.trim().parse::<T>().map_err(|e| ConfigError::Invalid {
                key,
                value: v,
                reason: e.to_string(),
            })
        }
        _ => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn source(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn minimal() -> Vec<(&'static str, &'static str)> {
        vec![
            ("DATABASE_URL", "postgresql://u:p@127.0.0.1:55432/plugin_hub"),
            ("REDIS_URL", "redis://127.0.0.1:6379/2"),
        ]
    }

    #[test]
    fn 缺必填项时报错并指明键名() {
        let err = Config::from_source(source(&[])).unwrap_err();
        assert_eq!(err, ConfigError::Missing("DATABASE_URL"));
        assert!(err.to_string().contains("DATABASE_URL"));
    }

    #[test]
    fn 空值等同缺失() {
        let mut pairs = minimal();
        pairs[0] = ("DATABASE_URL", "   ");
        assert_eq!(
            Config::from_source(source(&pairs)).unwrap_err(),
            ConfigError::Missing("DATABASE_URL")
        );
    }

    #[test]
    fn 未设置时套用默认端口与留存策略() {
        let cfg = Config::from_source(source(&minimal())).unwrap();
        // HTTP 面只绑回环，插件面绑全网卡——这个差异是刻意的
        assert_eq!(cfg.http_host, IpAddr::from([127, 0, 0, 1]));
        assert_eq!(cfg.http_port, DEFAULT_HTTP_PORT);
        assert_eq!(cfg.grpc_host, IpAddr::from([0, 0, 0, 0]));
        assert_eq!(cfg.grpc_port, DEFAULT_GRPC_PORT);
        assert_eq!(cfg.ops_socket, DEFAULT_OPS_SOCKET);
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(cfg.span_retention_days, 7);
        assert_eq!(cfg.audit_retention_days, 90);
        assert_eq!(cfg.stream_retention_hours, 24);
        assert_eq!(
            cfg.rejection_retention_days,
            DEFAULT_REJECTION_RETENTION_DAYS
        );
        assert_eq!(cfg.node_max_concurrency, DEFAULT_NODE_MAX_CONCURRENCY);
        assert_eq!(cfg.node_queue_timeout_ms, DEFAULT_NODE_QUEUE_TIMEOUT_MS);
        assert_eq!(
            cfg.breaker_failure_threshold,
            DEFAULT_BREAKER_FAILURE_THRESHOLD
        );
        assert_eq!(cfg.breaker_cooldown_secs, DEFAULT_BREAKER_COOLDOWN_SECS);
        assert_eq!(cfg.async_workers, DEFAULT_ASYNC_WORKERS);
        assert_eq!(cfg.bus_max_depth, DEFAULT_BUS_MAX_DEPTH);
        assert_eq!(cfg.bus_max_delivery, DEFAULT_BUS_MAX_DELIVERY);
        assert_eq!(cfg.bus_claim_min_idle_secs, DEFAULT_BUS_CLAIM_MIN_IDLE_SECS);
        assert_eq!(cfg.run_retention_days, DEFAULT_RUN_RETENTION_DAYS);
        assert_eq!(cfg.bootstrap_token, None);
        // 默认不配 MCP 白名单：只认本机 Host，够本机开发用，且不会误放开
        assert_eq!(cfg.mcp_allowed_hosts, None);
    }

    #[test]
    fn 治理参数可由环境变量覆盖() {
        let mut pairs = minimal();
        pairs.extend([
            ("NODE_MAX_CONCURRENCY", "8"),
            ("NODE_QUEUE_TIMEOUT_MS", "0"),
            ("BREAKER_FAILURE_THRESHOLD", "3"),
            ("BREAKER_COOLDOWN_SECS", "30"),
        ]);
        let cfg = Config::from_source(source(&pairs)).unwrap();
        assert_eq!(cfg.node_max_concurrency, 8);
        // 排队 0ms 是合法的激进档位：不排队，拿不到许可立刻失败
        assert_eq!(cfg.node_queue_timeout_ms, 0);
        assert_eq!(cfg.breaker_failure_threshold, 3);
        assert_eq!(cfg.breaker_cooldown_secs, 30);
    }

    #[test]
    fn 总线参数可由环境变量覆盖() {
        let mut pairs = minimal();
        pairs.extend([
            ("ASYNC_WORKERS", "8"),
            ("BUS_MAX_DEPTH", "500"),
            ("BUS_MAX_DELIVERY", "9"),
            ("BUS_CLAIM_MIN_IDLE_SECS", "30"),
            ("RUN_RETENTION_DAYS", "14"),
        ]);
        let cfg = Config::from_source(source(&pairs)).unwrap();
        assert_eq!(cfg.async_workers, 8);
        assert_eq!(cfg.bus_max_depth, 500);
        assert_eq!(cfg.bus_max_delivery, 9);
        assert_eq!(cfg.bus_claim_min_idle_secs, 30);
        assert_eq!(cfg.run_retention_days, 14);
    }

    #[test]
    fn 没有消费者被拒绝() {
        let mut pairs = minimal();
        pairs.push(("ASYNC_WORKERS", "0"));
        let err = Config::from_source(source(&pairs)).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    key: "ASYNC_WORKERS",
                    ..
                }
            ),
            "没有消费者时队会一路涨到堆积上限，中台看起来在跑但什么都不做：{err}"
        );
    }

    #[test]
    fn 并发上限为零被拒绝() {
        let mut pairs = minimal();
        pairs.push(("NODE_MAX_CONCURRENCY", "0"));
        let err = Config::from_source(source(&pairs)).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    key: "NODE_MAX_CONCURRENCY",
                    ..
                }
            ),
            "并发上限为 0 等于把中台关死，必须在启动时就拦下：{err}"
        );
    }

    #[test]
    fn 显式设置覆盖默认值并去除空白() {
        let mut pairs = minimal();
        pairs.extend([
            ("HUB_HTTP_HOST", "0.0.0.0"),
            ("HUB_HTTP_PORT", " 18092 "),
            ("LOG_LEVEL", "debug"),
            ("BOOTSTRAP_TOKEN", " boot-me "),
            ("SPAN_RETENTION_DAYS", "3"),
            ("HUB_OPS_SOCKET", "/tmp/hub-test.sock"),
        ]);
        let cfg = Config::from_source(source(&pairs)).unwrap();
        assert_eq!(cfg.http_host, IpAddr::from([0, 0, 0, 0]));
        assert_eq!(cfg.http_port, 18092);
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.bootstrap_token.as_deref(), Some("boot-me"));
        assert_eq!(cfg.span_retention_days, 3);
        assert_eq!(cfg.ops_socket, "/tmp/hub-test.sock");
    }

    #[test]
    fn 未配_otlp_端点时默认不导出() {
        let cfg = Config::from_source(source(&minimal())).unwrap();
        assert_eq!(
            cfg.otlp_endpoint, None,
            "UAT 没有 trace 后端，缺省不该有任何导出动作"
        );

        // 空串也算没配，避免误以为「写了个空值就是在用」
        let mut pairs = minimal();
        pairs.push(("OTLP_ENDPOINT", "   "));
        assert_eq!(
            Config::from_source(source(&pairs)).unwrap().otlp_endpoint,
            None
        );
    }

    #[test]
    fn 配置了_otlp_端点时生效() {
        let mut pairs = minimal();
        pairs.push(("OTLP_ENDPOINT", "http://127.0.0.1:4318/v1/traces"));
        let cfg = Config::from_source(source(&pairs)).unwrap();
        assert_eq!(
            cfg.otlp_endpoint.as_deref(),
            Some("http://127.0.0.1:4318/v1/traces")
        );
    }

    #[test]
    fn 端口非法时报错() {
        let mut pairs = minimal();
        pairs.push(("HUB_HTTP_PORT", "not-a-port"));
        let err = Config::from_source(source(&pairs)).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "HUB_HTTP_PORT",
                ..
            }
        ));
    }

    #[test]
    fn 两面端口相同被拒绝() {
        let mut pairs = minimal();
        pairs.extend([("HUB_HTTP_PORT", "9000"), ("HUB_GRPC_PORT", "9000")]);
        let err = Config::from_source(source(&pairs)).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "HUB_GRPC_PORT",
                ..
            }
        ));
    }

    #[test]
    fn 留存为零被拒绝() {
        let mut pairs = minimal();
        pairs.push(("SPAN_RETENTION_DAYS", "0"));
        let err = Config::from_source(source(&pairs)).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "SPAN_RETENTION_DAYS",
                ..
            }
        ));
    }

    /// **留空 ≠ 空的允许列表**。前者是「我没说，用默认（只认本机）」；后者在 rmcp
    /// 的语义里是「放行全部 Host」——也就是把防 DNS rebinding 的那一层关掉。
    /// 所以没配、配了空串、只写逗号或空白，都必须得到 `None`。
    #[test]
    fn mcp_白名单留空得到_none_而不是空列表() {
        for raw in ["", "   ", ",", " , , "] {
            let mut pairs = minimal();
            pairs.push(("HUB_MCP_ALLOWED_HOSTS", raw));
            let cfg = Config::from_source(source(&pairs)).unwrap();
            assert_eq!(
                cfg.mcp_allowed_hosts, None,
                "HUB_MCP_ALLOWED_HOSTS={raw:?} 应得到 None（用默认），\
                 而不是空 Vec（rmcp 会把它当成放行全部）"
            );
        }
    }

    #[test]
    fn mcp_白名单按逗号切分并去掉空白与空项() {
        let mut pairs = minimal();
        pairs.push((
            "HUB_MCP_ALLOWED_HOSTS",
            " hub.example.com , localhost:8095 ,",
        ));
        let cfg = Config::from_source(source(&pairs)).unwrap();
        assert_eq!(
            cfg.mcp_allowed_hosts,
            Some(vec![
                "hub.example.com".to_string(),
                "localhost:8095".to_string()
            ])
        );
    }

    /// 对外接入端点是可选的：留空（含空白）必须得到 `None`——`/endpoints` 里的
    /// `mcp: null` 是控制台「退回浏览器推导地址」的信号，不能变成 `Some("")`。
    #[test]
    fn mcp_对外端点留空得到_none_配了原样保留() {
        for raw in ["", "   "] {
            let mut pairs = minimal();
            pairs.push(("HUB_MCP_PUBLIC_ENDPOINT", raw));
            let cfg = Config::from_source(source(&pairs)).unwrap();
            assert_eq!(cfg.mcp_public_endpoint, None);
        }

        let mut pairs = minimal();
        pairs.push(("HUB_MCP_PUBLIC_ENDPOINT", "https://203.0.113.10:8081/mcp"));
        let cfg = Config::from_source(source(&pairs)).unwrap();
        assert_eq!(
            cfg.mcp_public_endpoint,
            Some("https://203.0.113.10:8081/mcp".to_string())
        );
    }

    /// 互调策略不配 = `allow`（存量插件都没声明过 invokes，默认收紧会让互调全死）；
    /// 两档合法值原样进配置，不做大小写转换。
    #[test]
    fn 互调策略默认_allow_且只接受两档合法值() {
        let cfg = Config::from_source(source(&minimal())).unwrap();
        assert_eq!(
            cfg.plugin_call_policy, "allow",
            "缺省必须是 allow：升级到 M6 不能悄悄改变既有插件的互调行为"
        );

        for value in ["allow", "declared"] {
            let mut pairs = minimal();
            pairs.push(("HUB_PLUGIN_CALL_POLICY", value));
            let cfg = Config::from_source(source(&pairs)).unwrap();
            assert_eq!(cfg.plugin_call_policy, value);
        }
    }

    /// 策略拼错（大小写、拼写）必须在启动时报错：静默回落默认值会让「想收紧成
    /// declared」的部署以为自己配好了，实际一直敞着。
    #[test]
    fn 互调策略非法值被拒绝() {
        for value in ["Allow", "DECLARED", "open", "decalred"] {
            let mut pairs = minimal();
            pairs.push(("HUB_PLUGIN_CALL_POLICY", value));
            let err = Config::from_source(source(&pairs)).unwrap_err();
            assert!(
                matches!(
                    err,
                    ConfigError::Invalid {
                        key: "HUB_PLUGIN_CALL_POLICY",
                        ..
                    }
                ),
                "HUB_PLUGIN_CALL_POLICY={value:?} 应在启动时被拦下：{err}"
            );
        }
    }
}

#[test]
fn mcp_login_gate_默认关_显式开启才开() {
    let cfg = Config::from_source(|key| {
        Some(match key {
            "DATABASE_URL" => "postgresql://u:p@127.0.0.1:5432/plugin_hub".to_string(),
            "REDIS_URL" => "redis://127.0.0.1:6379/2".to_string(),
            _ => return None,
        })
    })
    .expect("最小配置应可加载");
    assert!(!cfg.mcp_login_gate, "缺省必须关闭：旧部署不能悄悄改变行为");

    for on in ["1", "true", "TRUE", "yes", "on"] {
        let cfg = Config::from_source(|key| {
            Some(match key {
                "DATABASE_URL" => "postgresql://u:p@127.0.0.1:5432/plugin_hub".to_string(),
                "REDIS_URL" => "redis://127.0.0.1:6379/2".to_string(),
                "HUB_MCP_LOGIN_GATE" => on.to_string(),
                _ => return None,
            })
        })
        .expect("配置应可加载");
        assert!(cfg.mcp_login_gate, "{on} 应视为开启");
    }
    for off in ["0", "false", "", "随便"] {
        let cfg = Config::from_source(|key| {
            Some(match key {
                "DATABASE_URL" => "postgresql://u:p@127.0.0.1:5432/plugin_hub".to_string(),
                "REDIS_URL" => "redis://127.0.0.1:6379/2".to_string(),
                "HUB_MCP_LOGIN_GATE" => off.to_string(),
                _ => return None,
            })
        })
        .expect("配置应可加载");
        assert!(!cfg.mcp_login_gate, "{off:?} 应视为关闭");
    }
}
