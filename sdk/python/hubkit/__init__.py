"""hubkit —— plugin-hub 插件侧的服务端骨架（Python）。

插件作者只需要实现 :class:`hubkit.Plugin` 下的四件事，然后调用 :func:`hubkit.run`，
骨架会处理掉其余一切：gRPC 服务、向中台自注册、心跳续期、被摘除后自动重新注册、
优雅退出。

最小插件见 ``templates/plugin/plugin.py.tmpl`` 生成出来的样子，或者 README 里的例子。

本包与 ``sdk/go/hubkit`` 是同一套契约的两门语言实现，对外暴露的概念**逐个对应**：

===============================  ===============================  ==========================
Go                               Python                           作用
===============================  ===============================  ==========================
``hubkit.Run``                   :func:`run`                      起服务并跑到收到信号
``hubkit.RunContext``            :func:`serve`                    同上，由调用方决定何时结束
``hubkit.Plugin``                :class:`Plugin`                  插件要实现的接口
``hubkit.Config``                :class:`Config`                  运行参数
``hubkit.ConfigFromEnv``         :func:`config_from_env`          从环境变量读参数
``hubkit.PayloadJSON``           :func:`payload_json`             取 JSON 载荷 → (载荷, 是否 JSON)
``hubkit.WithPayloadJSON``       :func:`with_payload_json`        装 JSON 载荷
``hubkit.Valid`` / ``Invalid``   :func:`valid` / :func:`invalid`  构造校验结果
``hubkit.Budget`` / ``Expired``  :func:`budget` / :func:`expired` 读信封预算
``hubkit.StateAware``            :meth:`Plugin.set_state`         插件接收状态客户端（可选）
``hubkit.StateClient``           :class:`StateClient`             外置状态（HubState）客户端
``hubkit.StateEntry``            :class:`StateEntry`              一次扫描返回的一项
``hubkit.GatewayClient``         :class:`GatewayClient`           插件间发现与互调客户端
``conformance.Local``            ``conformance.local``            契约一致性自检
``mockhub``                      ``mockhub``                      本地 mock 中台（测试用）
===============================  ===============================  ==========================
"""

from .config import (
    CALL_TIMEOUT,
    DEFAULT_LISTEN_ADDR,
    GATEWAY_CALL_TIMEOUT,
    HEARTBEAT_FALLBACK_INTERVAL,
    REGISTER_RETRY_INTERVAL,
    STATE_CALL_TIMEOUT,
    Config,
    config_from_env,
)
from .envelope import (
    STRUCT_FQ_NAME,
    STRUCT_TYPE_URL,
    budget,
    deadline,
    expired,
    invalid,
    is_well_known_fq_name,
    issue,
    new_ulid,
    payload_json,
    valid,
    warning,
    with_payload,
    with_payload_json,
)
from .gateway import (
    CALL_CHAIN_META,
    DEFAULT_INVOKE_TIMEOUT_MS,
    GatewayClient,
    InvokeFailed,
    InvokeFailure,
    InvokeRejected,
)
from .log import DEBUG, ERROR, INFO, WARN, JsonLogger, level_from_env
from .plugin import Plugin, descriptor_of
from .rules import valid_plugin_name, valid_state_segment
from .run import (
    RegistrationRejected,
    Registrar,
    check_manifest,
    reject_code_name,
    run,
    serve,
)
from .state import (
    MAX_SCAN_LIMIT,
    MAX_VALUE_BYTES,
    PublishReceipt,
    STATE_TOKEN_METADATA,
    DenialSignal,
    StateClient,
    StateEntry,
)

__version__ = "0.1.0"

__all__ = [
    "CALL_CHAIN_META",
    "CALL_TIMEOUT",
    "DEBUG",
    "DEFAULT_INVOKE_TIMEOUT_MS",
    "DEFAULT_LISTEN_ADDR",
    "DenialSignal",
    "ERROR",
    "GATEWAY_CALL_TIMEOUT",
    "GatewayClient",
    "HEARTBEAT_FALLBACK_INTERVAL",
    "INFO",
    "InvokeFailed",
    "InvokeFailure",
    "InvokeRejected",
    "JsonLogger",
    "MAX_SCAN_LIMIT",
    "MAX_VALUE_BYTES",
    "Plugin",
    "PublishReceipt",
    "REGISTER_RETRY_INTERVAL",
    "RegistrationRejected",
    "Registrar",
    "STATE_CALL_TIMEOUT",
    "STATE_TOKEN_METADATA",
    "STRUCT_FQ_NAME",
    "STRUCT_TYPE_URL",
    "StateClient",
    "StateEntry",
    "Config",
    "WARN",
    "__version__",
    "budget",
    "check_manifest",
    "config_from_env",
    "deadline",
    "descriptor_of",
    "expired",
    "invalid",
    "is_well_known_fq_name",
    "issue",
    "level_from_env",
    "new_ulid",
    "payload_json",
    "reject_code_name",
    "run",
    "serve",
    "valid",
    "valid_plugin_name",
    "valid_state_segment",
    "warning",
    "with_payload",
    "with_payload_json",
]
