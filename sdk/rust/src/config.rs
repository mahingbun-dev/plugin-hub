//! 插件骨架的运行参数：**全部来自环境变量**，与 Go 侧逐项对齐。
//!
//! | 变量 | 必填 | 说明 |
//! |---|---|---|
//! | `HUB_ADDR` | 是 | 中台插件面地址（gRPC） |
//! | `HUB_ADVERTISE_ADDR` | 是 | **中台能拨通**的本插件地址 |
//! | `HUB_LISTEN_ADDR` | 否 | 本插件监听地址，缺省 `:9000` |
//! | `HUB_INSTANCE_ID` | 否 | 实例标识，缺省「主机名-PID」 |
//! | `HUB_LOG_LEVEL` | 否 | `debug` / `info` / `warn` / `error`，缺省 `info` |

use std::time::Duration;

use crate::log::{Level, Logger};

/// 缺省监听地址。与中台文档、Go SDK 保持同一个值——
/// 换掉它会让「照文档敲」的人得到一个连不上的插件。
pub const DEFAULT_LISTEN_ADDR: &str = ":9000";

/// 注册失败后的重试间隔。
///
/// 注册会一直重试而不是放弃：中台可能比插件晚起来，而**插件先于中台启动是常态**
/// （编排系统里两者的启动顺序不保证）。
pub const DEFAULT_REGISTER_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// 中台没在注册回执里给出心跳周期时的兜底值。
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// 单次 HubState 调用（`Get` / `Put` / `Delete` / `Scan`）的默认时间上限。
///
/// 远小于信封预算（HTTP 面缺省 30s），让状态调用**先于**业务调用放弃：中台一次卡顿
/// 最坏能吃掉 10s（PG 取连接）+ 查询 + 5s（Redis 响应超时），不设上限时这些时间
/// 全部从调用方的预算里扣，随后的业务调用会拿到一个已过期的 deadline。
///
/// 取 2s 而不是 5s：客户端要**短于**中台侧的 Redis 响应超时（5s），由客户端先放弃，
/// 插件才有机会走 fail-open。与 Go 侧的 `DefaultStateCallTimeout` 同一个值。
pub const DEFAULT_STATE_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// 单次网关**发现类**调用（列清单 / 查消息端点 / 查契约）的默认时间上限。
///
/// 与状态调用同一理由、同一个值：这类查询是基础设施，卡住了就该早点放弃，
/// 别让一次中台抖动吃掉业务预算。互调（invoke）**不**吃这个上限——它的等待
/// 时长由调用方声明的预算决定（见 `hubkit::gateway`），拿 2s 去套会把合法的
/// 慢下游全部掐死在客户端。
pub const DEFAULT_GATEWAY_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// 优雅退出时给 Unregister 的时间上限。
pub const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(3);

/// 骨架的运行参数。
#[derive(Debug, Clone)]
pub struct Config {
    /// 中台插件面地址，例如 `http://127.0.0.1:8093`（本地）
    /// 或 `https://hub.example.com:8094`（UAT，经 nginx 的 TLS 终结）。
    pub hub_addr: String,

    /// 中台可达的本插件地址，例如 `http://10.0.0.5:9000`。
    ///
    /// 中台在注册时会连它做**可达性探测**，所以必须是「从中台那边拨得通」的地址，
    /// 而不是本机视角的 localhost——这是插件接入时最容易踩的坑。
    pub advertise_addr: String,

    /// 本插件 gRPC 的监听地址。接受 `:9000` 这种省略主机的写法。
    pub listen_addr: String,

    /// 实例标识。缺省「主机名-PID」。
    ///
    /// 同一个 ID 重复注册视为进程重启（中台刷新地址与心跳），不产生重复实例；
    /// 但**被别的插件占用**（例如两个插件把 `HUB_INSTANCE_ID` 写成同一个值）
    /// 会被 `INSTANCE_CONFLICT` 拒掉。
    pub instance_id: String,

    /// 日志器。缺省打到 stderr 的 JSON，级别取自 `HUB_LOG_LEVEL`。
    pub logger: Logger,

    /// 注册失败后的重试间隔。测试里会调到毫秒级，生产一般不必改。
    pub register_retry_interval: Duration,

    /// 中台没给心跳周期时的兜底。同样是给测试用的旋钮。
    pub heartbeat_fallback_interval: Duration,

    /// 单次 HubState 调用的时间上限，缺省 [`DEFAULT_STATE_CALL_TIMEOUT`]。
    ///
    /// 它是**上限而非承诺**：插件侧不通过这个字段表达业务预算，SDK 也只保证
    /// 「一次状态调用不会吃光整个预算」。需要更紧的策略（比如只吃剩余预算的一半）
    /// 请在插件侧自己算好了传。
    ///
    /// 与 Go 侧一样**不从环境变量读**：它不该是运维在部署时随手改的旋钮，
    /// 改大它只会让一次中台抖动吃掉更多业务预算。
    pub state_call_timeout: Duration,

    /// 单次网关**发现类**调用的时间上限，缺省 [`DEFAULT_GATEWAY_CALL_TIMEOUT`]。
    ///
    /// 互调（invoke）不吃它：等待时长跟随调用方声明的预算，理由见常量文档。
    /// 同样**不从环境变量读**，理由同 `state_call_timeout`。
    pub gateway_call_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hub_addr: String::new(),
            advertise_addr: String::new(),
            listen_addr: DEFAULT_LISTEN_ADDR.to_string(),
            instance_id: String::new(),
            logger: Logger::new(Level::Info),
            register_retry_interval: DEFAULT_REGISTER_RETRY_INTERVAL,
            heartbeat_fallback_interval: DEFAULT_HEARTBEAT_INTERVAL,
            state_call_timeout: DEFAULT_STATE_CALL_TIMEOUT,
            gateway_call_timeout: DEFAULT_GATEWAY_CALL_TIMEOUT,
        }
    }
}

impl Config {
    /// 从环境变量读配置。缺省值由 [`Config::with_defaults`] 补齐。
    pub fn from_env() -> Self {
        let get = |key: &str| std::env::var(key).unwrap_or_default();

        Self {
            hub_addr: get("HUB_ADDR"),
            advertise_addr: get("HUB_ADVERTISE_ADDR"),
            listen_addr: get("HUB_LISTEN_ADDR"),
            instance_id: get("HUB_INSTANCE_ID"),
            logger: Logger::from_env(),
            ..Default::default()
        }
    }

    /// 补齐缺省值。
    pub fn with_defaults(mut self) -> Self {
        // 只把**空**当作没设。与 Go 侧一致：`HUB_LISTEN_ADDR=` 与不设同义。
        if self.listen_addr.trim().is_empty() {
            self.listen_addr = DEFAULT_LISTEN_ADDR.to_string();
        }
        if self.instance_id.is_empty() {
            self.instance_id = format!("{}-{}", hostname(), std::process::id());
        }
        if self.register_retry_interval.is_zero() {
            self.register_retry_interval = DEFAULT_REGISTER_RETRY_INTERVAL;
        }
        if self.heartbeat_fallback_interval.is_zero() {
            self.heartbeat_fallback_interval = DEFAULT_HEARTBEAT_INTERVAL;
        }
        // 与 Go 侧一致：`<= 0`（Rust 里只剩 0）视为没设。**不给「0 = 不限时」的语义**：
        // 一次状态调用没有上限时，中台卡多久就吃多久的业务预算，
        // 而 SDK 无法替调用方知道那个预算是多少。
        if self.state_call_timeout.is_zero() {
            self.state_call_timeout = DEFAULT_STATE_CALL_TIMEOUT;
        }
        if self.gateway_call_timeout.is_zero() {
            self.gateway_call_timeout = DEFAULT_GATEWAY_CALL_TIMEOUT;
        }
        self
    }

    /// 检查必填项，并给出能直接照做的提示。
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut missing = Vec::new();
        if self.hub_addr.trim().is_empty() {
            missing.push("HUB_ADDR（中台插件面地址）");
        }
        if self.advertise_addr.trim().is_empty() {
            missing.push("HUB_ADVERTISE_ADDR（中台可达的本插件地址）");
        }
        if !missing.is_empty() {
            return Err(ConfigError::Missing(missing.join("、")));
        }
        Ok(())
    }

    /// 把 `:9000` / `0.0.0.0:9000` / `[::]:9000` 解析成可 bind 的 socket 地址。
    ///
    /// 空主机名要显式补成 `0.0.0.0`：`":9000".parse::<SocketAddr>()` 会失败，
    /// 而 `:9000` 恰恰是文档和 Go SDK 里写的那个缺省值。
    pub fn listen_socket_addr(&self) -> Result<std::net::SocketAddr, ConfigError> {
        let raw = self.listen_addr.trim();
        let normalized = if let Some(port) = raw.strip_prefix(':') {
            format!("0.0.0.0:{port}")
        } else {
            raw.to_string()
        };
        normalized
            .parse()
            .map_err(|_| ConfigError::BadListenAddr(raw.to_string()))
    }

    /// 把 `HUB_ADDR` 变成 tonic 能用的 endpoint。
    ///
    /// 补 `http://` 前缀：Go 侧允许写 `127.0.0.1:8093`，两侧行为要一致，
    /// 否则同一份部署脚本换个语言就挂。
    pub fn hub_endpoint(&self) -> Result<tonic::transport::Endpoint, ConfigError> {
        let raw = self.hub_addr.trim();
        let with_scheme = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("http://{raw}")
        };
        tonic::transport::Endpoint::from_shared(with_scheme).map_err(|e| ConfigError::BadHubAddr {
            addr: raw.to_string(),
            reason: e.to_string(),
        })
    }
}

/// 配置错误。每一条都写清「怎么改」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// 必填项没给。里面是缺了哪几项的中文清单。
    Missing(String),
    /// 监听地址解析不了。
    BadListenAddr(String),
    /// 中台地址解析不了。
    BadHubAddr {
        /// 原始取值，原样带出来供人对照。
        addr: String,
        /// 底层解析器给的原因。
        reason: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Missing(what) => {
                write!(f, "hubkit: 缺少必填配置 {what}")
            }
            ConfigError::BadListenAddr(addr) => write!(
                f,
                "hubkit: HUB_LISTEN_ADDR 不是合法的监听地址：{addr:?}（形如 :9000 或 0.0.0.0:9000）"
            ),
            ConfigError::BadHubAddr { addr, reason } => {
                write!(f, "hubkit: HUB_ADDR 不是合法的地址：{addr:?}（{reason}）")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// 取主机名。
///
/// 不引 `hostname` crate：SDK 要随包发出去，能不加的依赖就不加，而这件事三条
/// 环境相关的手段就能覆盖全部目标平台：
///
///   1. `HOSTNAME` 环境变量（多数 shell 与容器运行时会设）
///   2. `/proc/sys/kernel/hostname`（Linux，容器里通常没有 1）
///   3. `hostname` 命令（macOS / BSD，前两条都没有）
///
/// 都拿不到就用 `unknown-host`——**不 panic**：实例标识取不到主机名只是不好认，
/// 不该让插件起不来。
fn hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.trim().is_empty() {
            return h.trim().to_string();
        }
    }

    if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        if !h.trim().is_empty() {
            return h.trim().to_string();
        }
    }

    if let Ok(out) = std::process::Command::new("hostname").output() {
        if out.status.success() {
            let h = String::from_utf8_lossy(&out.stdout);
            if !h.trim().is_empty() {
                return h.trim().to_string();
            }
        }
    }

    "unknown-host".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 缺省监听地址按_9000_补齐主机() {
        let cfg = Config {
            listen_addr: ":9000".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.listen_socket_addr().unwrap().to_string(),
            "0.0.0.0:9000"
        );

        let cfg = Config {
            listen_addr: "127.0.0.1:19000".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.listen_socket_addr().unwrap().to_string(),
            "127.0.0.1:19000"
        );
    }

    #[test]
    fn 监听地址写错时报出原始取值() {
        let cfg = Config {
            listen_addr: "not-an-addr".into(),
            ..Default::default()
        };
        let err = cfg.listen_socket_addr().unwrap_err();
        assert!(err.to_string().contains("not-an-addr"), "{err}");
    }

    #[test]
    fn 必填项缺失时一次列全() {
        let cfg = Config::default();
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("HUB_ADDR"), "{msg}");
        assert!(msg.contains("HUB_ADVERTISE_ADDR"), "{msg}");
    }

    #[test]
    fn 实例标识缺省成主机名加_pid() {
        let cfg = Config::default().with_defaults();
        assert!(
            cfg.instance_id
                .ends_with(&format!("-{}", std::process::id())),
            "{}",
            cfg.instance_id
        );
        assert!(cfg.instance_id.len() > 1);
    }

    #[test]
    fn 空串监听地址视为没设() {
        let cfg = Config {
            listen_addr: String::new(),
            ..Default::default()
        }
        .with_defaults();
        assert_eq!(cfg.listen_addr, DEFAULT_LISTEN_ADDR);
    }

    #[test]
    fn 状态调用超时缺省补成两秒_显式设置不被覆盖() {
        let cfg = Config {
            state_call_timeout: Duration::ZERO,
            ..Default::default()
        }
        .with_defaults();
        assert_eq!(cfg.state_call_timeout, DEFAULT_STATE_CALL_TIMEOUT);
        assert_eq!(cfg.state_call_timeout, Duration::from_secs(2));

        // 测试与特殊场景会把它调小，缺省补齐不该把它顶回去
        let custom = Duration::from_millis(50);
        let cfg = Config {
            state_call_timeout: custom,
            ..Default::default()
        }
        .with_defaults();
        assert_eq!(cfg.state_call_timeout, custom);
    }

    #[test]
    fn 网关发现调用超时缺省补成两秒_显式设置不被覆盖() {
        let cfg = Config {
            gateway_call_timeout: Duration::ZERO,
            ..Default::default()
        }
        .with_defaults();
        assert_eq!(cfg.gateway_call_timeout, DEFAULT_GATEWAY_CALL_TIMEOUT);

        let custom = Duration::from_millis(80);
        let cfg = Config {
            gateway_call_timeout: custom,
            ..Default::default()
        }
        .with_defaults();
        assert_eq!(cfg.gateway_call_timeout, custom);
    }

    #[test]
    fn hub_地址缺协议头时补上_http() {
        let cfg = Config {
            hub_addr: "127.0.0.1:8093".into(),
            ..Default::default()
        };
        let ep = cfg.hub_endpoint().unwrap();
        assert_eq!(ep.uri().to_string(), "http://127.0.0.1:8093/");

        let cfg = Config {
            hub_addr: "https://hub.example.com:8094".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.hub_endpoint().unwrap().uri().to_string(),
            "https://hub.example.com:8094/"
        );
    }
}
