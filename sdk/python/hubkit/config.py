"""运行参数：全部来自环境变量。

变量名与 Go 侧**逐字一致**（``HUB_ADDR`` / ``HUB_ADVERTISE_ADDR`` / ``HUB_LISTEN_ADDR``
/ ``HUB_INSTANCE_ID`` / ``HUB_LOG_LEVEL``）。同一份部署清单、同一份接入指南要能同时
套在两门语言上，变量名分叉就意味着文档要分叉，而文档分叉之后一定有一边是过期的。
"""

from __future__ import annotations

import os
import socket
from dataclasses import dataclass, field, replace

from .log import JsonLogger, level_from_env

# 默认值。写成模块级常量便于插件在自己的启动逻辑里做校验或打印。
DEFAULT_LISTEN_ADDR = ":9000"

# 注册失败后的重试间隔（秒）。
#
# 注册会一直重试而不是放弃：中台可能比你晚起来，而插件进程先于中台启动是常态。
REGISTER_RETRY_INTERVAL = 5.0

# 中台没告诉我们心跳周期时的兜底值（秒）。
HEARTBEAT_FALLBACK_INTERVAL = 10.0

# 单次调用中台接口（注册/心跳/注销）的时间上限（秒）。
#
# 卡住的 RPC 必须能自己放弃：注册线程是单线程串行的，一次永不返回的调用会让
# 心跳和「被摘除后自愈」一起停摆——而这两种失败的现场长得一模一样（日志停在
# 最后一行不动），没有超时的话根本区分不出是中台慢了还是网络断了。
CALL_TIMEOUT = 5.0

# 单次 HubState 调用的时间上限（秒），缺省值。
#
# 远小于信封预算（HTTP 面缺省 30s），让状态调用**先于**业务调用放弃：中台一次卡顿
# 最坏能吃掉 10s（PG 取连接）+ 查询 + 5s（Redis 响应超时），不设上限时这些时间全部
# 从调用方的预算里扣，随后的业务调用会拿到已经过期的预算。
#
# 取 2s 而不是 5s：客户端应**短于**中台侧的 Redis 响应超时（5s），由客户端先放弃，
# 插件才有机会走 fail-open。与 Go 的 ``DefaultStateCallTimeout`` 逐字对齐。
STATE_CALL_TIMEOUT = 2.0

# 网关（发现三件套）单次调用的时间上限（秒），缺省值。
#
# ListPlugins / DescribeMessage / GetContract 与注册、状态查询同类——都是注册表的
# 快查询，量级与超时哲学相同（中台一次卡顿最坏十几秒，客户端先放弃）。
# **互调（Invoke）不走这个值**：它同步等下游业务执行完，预算由调用方每次给的
# ``timeout_ms`` 决定（见 ``hubkit.gateway.GatewayClient.invoke``）。
GATEWAY_CALL_TIMEOUT = STATE_CALL_TIMEOUT


@dataclass
class Config:
    """骨架的运行参数。"""

    # 中台插件面地址。
    #
    # 生产形如 https://hub.example.com:8094（经 nginx 的 TLS 终结）。
    # 中台与插件不要求同机——插件可以部署在任何能连上这个地址的地方。
    hub_addr: str = ""

    # 中台可达的本插件地址，例如 http://10.0.0.5:9000。
    #
    # 中台在注册时会连它做**可达性探测**，所以必须是「从中台那边拨得通」的地址，
    # 而不是本机视角的 localhost——这是插件接入时最容易踩的坑。
    advertise_addr: str = ""

    # 本插件 gRPC 的监听地址，缺省 :9000。
    listen_addr: str = DEFAULT_LISTEN_ADDR

    # 实例标识，缺省 「主机名-PID」。
    #
    # 同一 ID 重复注册视为进程重启（中台会刷新地址与心跳），不产生重复实例。
    instance_id: str = ""

    # 注册失败后的重试间隔（秒），缺省 REGISTER_RETRY_INTERVAL。
    #
    # 测试里会调到零点几秒；生产一般不必改。
    retry_interval: float = REGISTER_RETRY_INTERVAL

    # 单次中台调用的时间上限（秒），缺省 CALL_TIMEOUT。
    call_timeout: float = CALL_TIMEOUT

    # 单次 HubState（外置状态）调用的时间上限（秒），缺省 STATE_CALL_TIMEOUT。
    #
    # **没有对应的环境变量**，与 Go 侧一致（Go 的 ConfigFromEnv 也不读它）：
    # 两门语言的环境变量清单是逐字对齐的，多一个只在一侧存在的变量，就等于让
    # 「同一份部署清单」这个承诺失效。要调就在代码里给 Config 传值。
    #
    # 它是**上限而非承诺**：状态调用永远不会占满整个调用预算，但 SDK 保证不了
    # 「给业务留多少」——SDK 不知道调用方的总预算意图。需要更紧的策略（比如只吃
    # 剩余预算的一半）请在插件侧自己控制。
    state_call_timeout: float = STATE_CALL_TIMEOUT

    # 结构化日志，缺省打到 stderr。
    logger: JsonLogger = field(default_factory=JsonLogger, compare=False)

    def validate(self) -> None:
        """检查必填项，并给出能直接照做的提示。"""
        missing = []
        if not (self.hub_addr or "").strip():
            missing.append("HUB_ADDR（中台插件面地址）")
        if not (self.advertise_addr or "").strip():
            missing.append("HUB_ADVERTISE_ADDR（中台可达的本插件地址）")
        if missing:
            raise ValueError("hubkit: 缺少必填配置 " + "、".join(missing))

    def with_defaults(self) -> "Config":
        """补齐缺省值。"""
        cfg = replace(self)
        if not cfg.listen_addr:
            cfg.listen_addr = DEFAULT_LISTEN_ADDR
        if not cfg.instance_id:
            cfg.instance_id = f"{_hostname()}-{os.getpid()}"
        if cfg.retry_interval is None or cfg.retry_interval <= 0:
            cfg.retry_interval = REGISTER_RETRY_INTERVAL
        if cfg.call_timeout is None or cfg.call_timeout <= 0:
            cfg.call_timeout = CALL_TIMEOUT
        if cfg.state_call_timeout is None or cfg.state_call_timeout <= 0:
            cfg.state_call_timeout = STATE_CALL_TIMEOUT
        if cfg.logger is None:
            cfg.logger = JsonLogger()
        return cfg

    def grpc_target(self) -> str:
        """中台的 gRPC 连接目标（去掉 scheme）。

        Python 的 grpc 只认 ``host:port``；把 ``http://`` 一起传进去会被当成
        一个默认端口的 DNS 名，报错是 ``DNS resolution failed``——完全看不出
        是 URL 多写了个前缀。
        """
        target = (self.hub_addr or "").strip()
        for prefix in ("https://", "http://"):
            if target.startswith(prefix):
                return target[len(prefix):]
        return target

    def use_tls(self) -> bool:
        """按 scheme 决定是否走 TLS，与 Go 侧同款判断。"""
        return (self.hub_addr or "").strip().startswith("https://")

    def grpc_listen_target(self) -> str:
        """本插件 gRPC 的监听目标。

        ``HUB_LISTEN_ADDR`` 允许写成 Go 风格的 ``:9000``（省掉主机名表示所有网卡），
        而 grpc-python 的 ``add_insecure_port`` 不认这种写法——它要 ``host:port``。
        这里把缺省主机名补成 ``[::]``（双栈，与 Go 的 ``:9000`` 等价），
        于是同一份部署清单在两边都能用。
        """
        raw = (self.listen_addr or DEFAULT_LISTEN_ADDR).strip()
        if raw.startswith(":"):
            return "[::]" + raw
        if raw.isdigit():
            # 只写端口号也是常见写法，同样补成双栈
            return f"[::]:{raw}"
        if raw.startswith("0.0.0.0:"):
            # 0.0.0.0 只覆盖 IPv4；中台在 IPv6 网段时拨不通。升成双栈。
            return "[::]" + raw[len("0.0.0.0"):]
        return raw


def _hostname() -> str:
    try:
        return socket.gethostname()
    except OSError:
        return "unknown-host"


def config_from_env() -> Config:
    """从环境变量读取配置；缺省值由 :meth:`Config.with_defaults` 补齐。

    ``HUB_ADDR``            中台插件面地址（必填）
    ``HUB_ADVERTISE_ADDR``  本插件对外可达地址（必填）
    ``HUB_LISTEN_ADDR``     本插件监听地址（缺省 ``:9000``）
    ``HUB_INSTANCE_ID``     实例标识（缺省 主机名-PID）
    ``HUB_LOG_LEVEL``       debug / info / warn / error（缺省 info）
    """
    return Config(
        hub_addr=os.environ.get("HUB_ADDR", ""),
        advertise_addr=os.environ.get("HUB_ADVERTISE_ADDR", ""),
        listen_addr=os.environ.get("HUB_LISTEN_ADDR", ""),
        instance_id=os.environ.get("HUB_INSTANCE_ID", ""),
        logger=JsonLogger(level_from_env(os.environ.get("HUB_LOG_LEVEL"))),
    )
