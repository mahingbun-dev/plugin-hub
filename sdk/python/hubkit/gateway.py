"""插件网关（``hub.v1.PluginGateway``）的客户端：插件间发现与互调。

契约在 ``crates/hub-proto/proto/hub/v1/gateway.proto``，服务端在
``crates/hub-grpc/src/gateway.rs``。设计前提：**插件之间不直连**——直连会绕过
中台的治理、审计、熔断与身份体系，所以同步互调一律 A→hub→B，由中台代调；
发现能力（谁在线、谁生产/消费什么消息、契约长什么样）也只从中台取。

两条必须知道的事：

* **身份与 HubState 同源**：每个 RPC 都凭 metadata ``x-hub-state-token``（注册时
  下发的那张凭证，见 :mod:`hubkit.state`）反查调用方插件名，信封 subject 由中台
  **无条件覆盖**为该身份——请求里自报的 subject 不可信，填了也没用。
* **结果语义分两层**（与中台的 ``InvokeOutcome`` 对应）：业务结果（下游拒绝、
  未授权、超限、成环、下游出错）走响应字段 ``outcome`` + ``reason``/``issues``，
  :meth:`GatewayClient.invoke_plugin` 把它们翻成类型化异常
  （:class:`InvokeRejected` / :class:`InvokeFailed`）；基础设施故障（未鉴权、中台
  没装配调用能力）走 gRPC Status，原样抛 :class:`grpc.RpcError`。分层的用意：网络
  错误该重试或上报，业务拒绝该改逻辑——两者混成一种异常，调用方就只能解析错误字符串。

**互调链**：``Envelope.meta["hub.call_chain"]`` 记录「已处理过这条消息的插件序列」，
中台据此防 A→B→A 成环。在 flow 节点里发起互调时，把**收到的那个信封**作为
``current_envelope`` 传给 :meth:`GatewayClient.invoke_plugin`：SDK 复制
``trace_id``/``run_id``/``node_id`` 让 trace 贯通，并把链**原样**带上——追加
caller 是中台的职责，客户端自己追加会把链写错（链的语义是「已处理者序列」，
见服务端 ``gateway.rs`` 的 ``check_call_chain``）。顶层发起（没有当前信封）时
生成新 trace、不带链。

与 Go 侧 ``hubkit.StateClient`` 的对齐方式同 :mod:`hubkit.state`：概念逐个对应，
按 Python 的习惯落成同步 API。
"""

from __future__ import annotations

import threading
import time
from typing import Any

import grpc

from .config import GATEWAY_CALL_TIMEOUT
from .envelope import new_ulid, with_payload, with_payload_json
from .proto.hubv1 import envelope_pb2, gateway_pb2, gateway_pb2_grpc
from .state import STATE_TOKEN_METADATA

# 互调链的 meta 键。事实源是 ``sdk/go/hubkit/testdata/hub-rules.json`` 的
# ``gateway.callChainMeta``（Rust 侧 ``hub_grpc::gateway::CALL_CHAIN_META`` 同值）。
#
# 客户端只**复制**链（把收到的信封的链原样带上下一个调用），不解析、不校验、
# 不追加——那些都是中台在代调时做的治理，插件侧自己再算一遍等于跟中台抢职责，
# 而且算错时会造成「本地放行、中台拒绝」的裂缝。
CALL_CHAIN_META = "hub.call_chain"

# 互调链长度上限（含本次 caller）。仅用于导出与自检：校验发生在中台，
# 这里钉住数值是为了让插件侧的文档与断言有同一个出处，不另编一个数。
MAX_INVOKE_DEPTH = 8

# ``timeout_ms`` 没填（0）时的兜底预算（毫秒）。
#
# 与服务端 ``gateway.rs`` 的 ``DEFAULT_INVOKE_TIMEOUT_MS`` 同值：0 是「没填」
# 而不是「不等结果」，直译成零预算会让所有没配超时的调用瞬间失败。两端同值的
# 理由：SDK 用它填信封的 ``deadline_ms``，中台用同一预算做 deadline 夹紧的上限，
# 错开的话「SDK 给的预算」与「中台认的预算」会静默不一致。
DEFAULT_INVOKE_TIMEOUT_MS = 30_000

# 互调的 gRPC deadline 在业务预算之上加的余量（秒）。
#
# 中台超时会以 ``outcome=ERROR`` 正常返回（那是业务结果），gRPC deadline 只是防
# 「中台本身挂死」的兜底，所以必须**长于**业务预算——正好等于预算的话，下游的
# 正常超时会先被 gRPC 掐断，调用方看到的就是 ``DEADLINE_EXCEEDED`` 而不是带
# reason 的异常，排查时少了最重要的一条信息。
INVOKE_RPC_HEADROOM_SECONDS = 5.0


class InvokeFailure(Exception):
    """互调业务失败的共同基类（``outcome=REJECTED`` / ``ERROR``）。

    想对两类失败做同一件事（记日志、退避）就抓它；想区分处理就抓子类。
    ``outcome`` / ``reason`` / ``issues`` 挂在属性上，不用解析异常消息——
    那是「结果语义走响应字段」这条设计在客户端的落点：原因进了异常消息就只剩
    给人看的份，进了属性才能被程序分支。

    手写 ``__init__`` 而不是用 ``@dataclass``：异常子类上的 dataclass 会跟
    ``Exception`` 自身的构造/序列化行为搅在一起，省的那几行不值当。
    """

    def __init__(self, outcome: str, reason: str, issues: tuple = ()) -> None:
        # "REJECTED" 或 "ERROR"，与 proto 的 ``InvokeOutcome`` 枚举名一致
        self.outcome = outcome
        # 人类可读原因。REJECTED 时是中台的固定提示（结构化内容在 issues 里）
        self.reason = reason
        # REJECTED 时是目标插件 Validate 的结构化问题（``plugin_pb2.ValidationIssue``），
        # ERROR 时恒为空元组
        self.issues = tuple(issues)
        super().__init__(str(self))

    def __str__(self) -> str:
        head = f"hubkit: 插件互调失败（{self.outcome}）: {self.reason}"
        if not self.issues:
            return head
        detail = "; ".join(f"{issue.path}: {issue.message}" for issue in self.issues)
        return f"{head}（issues: {detail}）"


class InvokeRejected(InvokeFailure):
    """目标插件的 Validate 拒绝（``outcome=REJECTED``）。

    ``issues`` 是结构化校验问题（每条的 ``path`` 能定位到具体字段），调用方该拿它
    改数据重试，而不是无脑重发——数据不变的重发只会得到同一批 issues。
    """

    def __init__(self, issues: tuple, reason: str) -> None:
        super().__init__("REJECTED", reason, issues)


class InvokeFailed(InvokeFailure):
    """互调被中台拦下或下游出错（``outcome=ERROR``）。

    覆盖的情形（reason 里会说明是哪种）：未声明调用授权、分钟级配额打满、互调成环、
    链深超限、目标插件不存在 / 没有健康实例 / 熔断 / 超时。按 reason 决定重试
    （「没有健康实例」值得退避重试）还是改逻辑（「未声明授权」重试没有意义）。
    """

    def __init__(self, reason: str) -> None:
        super().__init__("ERROR", reason)


class GatewayClient:
    """插件网关（发现 + 互调）的客户端。

    构造与 :class:`hubkit.StateClient` 同款（同一个 channel、同一套凭证管理），
    凭证就是状态凭证——中台用同一张 ``x-hub-state-token`` 认两件事：状态面的
    「这条键值是谁写的」与网关面的「这次互调是谁发起的」。由 ``run`` 在注册成功后
    注入给实现了 :meth:`hubkit.Plugin.set_gateway` 的插件，凭证轮换由骨架驱动，
    插件作者不该自己管 token。

    出错时抛 :class:`grpc.RpcError`（**原样**抛，与 :class:`hubkit.StateClient`
    一致）：插件侧看 ``err.code()`` 决定退避还是上报。撞上 ``UNAUTHENTICATED``
    会顺带叫醒注册循环换新凭证（``denied`` 信号），但错误本身仍原样抛。
    """

    def __init__(
        self,
        channel: grpc.Channel,
        *,
        call_timeout: float = GATEWAY_CALL_TIMEOUT,
        denied: Any = None,
    ) -> None:
        """
        :param channel: 与中台的连接。**与注册、心跳、状态共用同一条**（见 ``run``）。
        :param call_timeout: 发现类调用（list/describe/contract）单次的时间上限
            （秒）。互调不走它——互调的预算是调用方每次给的 ``timeout_ms``。
        :param denied: 凭证被拒时的信号出口，与 :class:`hubkit.StateClient` 共享
            同一个 :class:`hubkit.DenialSignal`（401 的处置是同一件事：换凭证）。
        """
        self._stub = gateway_pb2_grpc.PluginGatewayStub(channel)
        self._call_timeout = call_timeout if call_timeout and call_timeout > 0 else None
        self._denied = denied
        self._mu = threading.Lock()
        self._token = ""

    # ------------------------------------------------------------ 凭证

    def set_token(self, token: str) -> None:
        """换上新凭证。**骨架用**（注册成功后调用），与 :meth:`hubkit.StateClient.set_token`
        在同一处被驱动——两边持有的是同一张凭证的两个引用，必须一起换。
        """
        with self._mu:
            self._token = token

    @property
    def token(self) -> str:
        """当前凭证。空串表示中台还没下发（此时调用必然被回 UNAUTHENTICATED）。"""
        with self._mu:
            return self._token

    # ------------------------------------------------------------ 发现

    def list_plugins(self, include_offline: bool = False) -> list:
        """在线插件清单（``gateway_pb2.PluginSummary`` 列表）。

        缺省只列有健康实例的；``include_offline=True`` 时附上离线插件。插件用它找
        「能调谁」，不必旁路维护一份硬编码名单。
        """
        response = self._rpc(
            self._stub.ListPlugins,
            gateway_pb2.ListPluginsRequest(include_offline=include_offline),
        )
        return list(response.plugins)

    def describe_message(self, fq_name: str) -> Any:
        """消息类型 → 生产者 / 消费者（``gateway_pb2.DescribeMessageResponse``）。

        ``fq_name`` 与 manifest 契约里的全限定名同口径（如 ``wms.v1.OrderCreated``）。
        两边都查无时返回空响应而不是报错：「没人生产/消费这个消息」本身就是有效答案。
        """
        return self._rpc(
            self._stub.DescribeMessage,
            gateway_pb2.DescribeMessageRequest(fq_name=fq_name),
        )

    def get_contract(self, plugin: str, version: str = "", fq_name: str = "") -> Any:
        """插件契约 + 字段级 schema（``gateway_pb2.GetContractResponse``）。

        ``version`` 空取最新登记版；``fq_name`` 空 = 不展开单个消息的字段级 schema
        （``schema_json`` 为空串），要看某个消息吃什么就把它传进来。查无插件或版本
        会抛 ``grpc.RpcError``（``NOT_FOUND``）——与发现不同，「要调的对象不存在」
        对调用方是异常而不是空答案。
        """
        return self._rpc(
            self._stub.GetContract,
            gateway_pb2.GetContractRequest(plugin=plugin, version=version, fq_name=fq_name),
        )

    # ------------------------------------------------------------ 互调

    def invoke(
        self,
        plugin: str,
        envelope: envelope_pb2.Envelope,
        version: str = "",
        timeout_ms: int = 0,
    ) -> Any:
        """同步互调的底层形态：信封由调用方完整构造，原样返回 ``InvokeResponse``。

        便捷入口是 :meth:`invoke_plugin`（替你造信封、把业务结果翻成异常）。直接用
        本方法的人要自己处理 ``outcome``：HANDLED 取 ``response.envelope``；
        REJECTED/ERROR 是业务结果而非 gRPC 错误，**不会**自己变成异常。

        ``timeout_ms`` 是本次调用的业务预算；中台会把信封 deadline 夹到
        ``min(信封 deadline, now + timeout_ms)``。gRPC deadline 在此之上加
        :data:`INVOKE_RPC_HEADROOM_SECONDS` 兜底——理由见那个常量的注释。
        """
        return self._rpc(
            self._stub.Invoke,
            gateway_pb2.InvokeRequest(
                plugin=plugin,
                version=version,
                envelope=envelope,
                timeout_ms=timeout_ms,
            ),
            rpc_timeout=self._invoke_rpc_timeout(timeout_ms),
        )

    def invoke_plugin(
        self,
        plugin: str,
        payload: Any = None,
        *,
        payload_type: str = "",
        version: str = "",
        timeout_ms: int = 0,
        current_envelope: envelope_pb2.Envelope | None = None,
    ) -> envelope_pb2.Envelope:
        """互调的便捷入口：替你造信封、发调用、把业务结果翻成异常。

        返回**下游处理后的信封**（``outcome=HANDLED`` 时的 ``response.envelope``）；
        ``REJECTED`` 抛 :class:`InvokeRejected`（issues 随异常携带）、``ERROR`` 抛
        :class:`InvokeFailed`（reason 随异常携带）。

        :param plugin: 目标插件名。
        :param payload: ``dict``（JSON 载荷，最常见）、protobuf message（业务类型，
            打包成 ``Any``）或 ``bytes``（此时必须给 ``payload_type``——裸字节没有
            类型信息，中台与下游都无从解释它）。``None`` = 不带载荷。
        :param payload_type: ``payload`` 是 bytes 时的全限定消息名。
        :param version: 目标版本，空 = 最新。
        :param timeout_ms: 本次调用的业务预算（毫秒），0 = 用
            :data:`DEFAULT_INVOKE_TIMEOUT_MS` 兜底。
        :param current_envelope: 调用方**正在处理的那个信封**（``handle`` 收到的
            ``env``）。给了它：``trace_id``/``run_id``/``node_id`` 复制过去让 trace
            贯通，``meta["hub.call_chain"]`` **原样**带上让中台能防环——不要自己往
            链里追加自己，那是中台的职责；其 ``deadline_ms`` 也一并继承（与本次
            预算取较早者，整体预算更紧时以它为准）。顶层发起（没有当前信封）就不
            传：新 trace、不带链。
        """
        envelope = envelope_pb2.Envelope(
            message_id=new_ulid(),
            type=envelope_pb2.PAYLOAD_TYPE_REQUEST,
        )
        if current_envelope is not None:
            envelope.trace_id = current_envelope.trace_id
            envelope.run_id = current_envelope.run_id
            envelope.node_id = current_envelope.node_id
            chain = current_envelope.meta.get(CALL_CHAIN_META, "")
            if chain:
                # 原样复制，不追加自己：链的语义是「已处理过该消息的插件序列」，
                # 追加 caller 发生在中台代调时（那时 caller 才真正「处理过」）
                envelope.meta[CALL_CHAIN_META] = chain
        else:
            envelope.trace_id = new_ulid()

        budget_ms = timeout_ms if timeout_ms and timeout_ms > 0 else DEFAULT_INVOKE_TIMEOUT_MS
        # 信封 deadline 用与中台同一个预算起算：让调用方（以及下游的 budget()）
        # 看到的剩余时间与这次调用真正的预算一致。父信封自带 deadline 时取较早者
        # ——整体预算比本次预算更紧时以它为准，不能让下游活得比父处理更久
        # （与 Go/Node/C#/Rust 四门同语义）。
        deadline_ms = _now_ms() + budget_ms
        if current_envelope is not None and current_envelope.deadline_ms > 0:
            deadline_ms = min(deadline_ms, current_envelope.deadline_ms)
        envelope.deadline_ms = deadline_ms

        envelope = _pack_payload(envelope, payload, payload_type)

        response = self.invoke(plugin, envelope, version=version, timeout_ms=budget_ms)
        outcome = gateway_pb2.InvokeOutcome.Name(response.outcome)
        if outcome == "HANDLED":
            if not response.HasField("envelope"):
                # 中台违约（协议上 HANDLED 必带信封）。返回一个空信封会让调用方把
                # 「没有结果」当成「结果是空的」用下去，宁可在这里显式炸出来
                raise InvokeFailed("中台返回 HANDLED 但没有携带下游信封")
            return response.envelope
        if outcome == "REJECTED":
            raise InvokeRejected(tuple(response.issues), response.reason)
        if outcome == "ERROR":
            raise InvokeFailed(response.reason)
        # UNSPECIFIED 只可能来自不按契约实现的假中台；按未知错误处理而不是猜
        raise InvokeFailed(f"中台返回了未知的 outcome: {response.outcome}")

    # ------------------------------------------------------------ 内部

    def _invoke_rpc_timeout(self, timeout_ms: int) -> float | None:
        """互调的 gRPC deadline：业务预算 + 余量，且不小于发现调用的上限。"""
        budget_ms = timeout_ms if timeout_ms and timeout_ms > 0 else DEFAULT_INVOKE_TIMEOUT_MS
        rpc_timeout = budget_ms / 1000.0 + INVOKE_RPC_HEADROOM_SECONDS
        if self._call_timeout is not None:
            rpc_timeout = max(rpc_timeout, self._call_timeout)
        return rpc_timeout

    def _rpc(self, method, request, rpc_timeout: float | None = None):
        """发一次网关 RPC：统一带凭证与超时，并把「被拒」翻译成信号。

        ``rpc_timeout`` 缺省用发现调用的上限；互调传自己的预算（见
        :meth:`_invoke_rpc_timeout`）。凭证被拒的处置与 :class:`hubkit.StateClient`
        完全一致——那是同一个凭证、同一个 401，不该有两套反应。
        """
        try:
            return method(
                request,
                timeout=rpc_timeout if rpc_timeout is not None else self._call_timeout,
                metadata=((STATE_TOKEN_METADATA, self.token),),
            )
        except grpc.RpcError as err:
            if self._denied is not None and err.code() == grpc.StatusCode.UNAUTHENTICATED:
                self._denied.notify()
            raise


def _pack_payload(
    envelope: envelope_pb2.Envelope, payload: Any, payload_type: str
) -> envelope_pb2.Envelope:
    """把三种形态的载荷装进信封；装不进去的在这里报错，而不是发到中台才炸。"""
    if payload is None:
        return envelope
    if isinstance(payload, dict):
        return with_payload_json(envelope, payload)
    if hasattr(payload, "SerializeToString"):
        return with_payload(envelope, payload)
    if isinstance(payload, (bytes, bytearray, memoryview)):
        if not payload_type:
            raise ValueError(
                "hubkit: 裸字节载荷必须给 payload_type（全限定消息名）——"
                "没有类型信息，中台与下游都无从解释这段字节"
            )
        envelope.payload.type_url = f"type.googleapis.com/{payload_type}"
        envelope.payload.value = bytes(payload)
        return envelope
    raise ValueError(
        f"hubkit: 不支持的载荷类型 {type(payload).__name__}——"
        "只接受 dict（JSON）、protobuf message 或 bytes（需配 payload_type）"
    )


def _now_ms() -> int:
    return time.time_ns() // 1_000_000
