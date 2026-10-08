"""本地 mock 中台。

用途有二：

* **离线开发**：插件团队不必等中台就绪，也不必连测试环境就能把插件跑起来。
* **自动化测试**：断言「注册请求里到底报了什么」「心跳有没有按时发」
  「被摘除后有没有重新注册」这类行为——这些在真中台上没法按需触发。

它实现插件面（``PluginRegistry``）、网关面（``PluginGateway`` 的 ``Invoke`` /
``DescribeMessage`` 替身，见下），**不实现**状态面（``HubState``，与
``sdk/go/mockhub`` 相比少的正是这一块）——插件调它时会拿到 ``UNIMPLEMENTED``。
少实现的那部分不假装成功：给个假实现会让用到 HubState 的插件在 mock 上跑绿、
到真中台才发现写不进去。

网关面的替身只做三件事：**鉴权**（与注销同一套规则，凭证不符一律
``UNAUTHENTICATED``）、**记录**收到的请求（供测试断言「客户端到底发了什么」）、
**转发**给 ``Options.invoke_handler`` 注入的下游替身。它**不模拟**中台的治理
（链校验、配额、subject 覆盖、deadline 夹紧）——那些要对着真中台验证；缺省
（没注入 handler）一律回 ``outcome=ERROR`` 而不是假装调用成功，同样是「mock
不该比真中台宽松」这条纪律。

一条纪律（照抄 Go 侧）：**mock 不该比真中台宽松**。离线的 mock 一旦比真环境宽容，
问题就会推迟到上线才爆。
"""

from __future__ import annotations

import socket
import threading
import time
from concurrent import futures
from dataclasses import dataclass
from typing import Any, Callable

import grpc

from ..proto.hubv1 import (
    gateway_pb2,
    gateway_pb2_grpc,
    plugin_pb2,
    plugin_pb2_grpc,
    registry_pb2,
    registry_pb2_grpc,
)

# mock 固定下发的状态凭证。真中台每次注册轮换，mock 不轮换（插件侧感知不到差别，
# 而固定值让测试能直接断言「注销带的就是注册时下发的那张」）。
#
# mockhub 不实现状态面，所以它不做 HubState 的认证；但**注销要认它**——见
# ``Hub._on_unregister``：中台凭注册时下发的凭证认出「你是这一行的主人」。
MOCK_STATE_TOKEN = "mock-state-token"


@dataclass
class Options:
    """控制 mock 中台的行为。"""

    # 中台指定的心跳周期（秒），缺省 1（测试里要快）。
    heartbeat_interval_seconds: int = 1

    # 非 None 时用它来拒绝注册。
    #
    # 用来验证插件侧对拒绝的处理：拒绝原因有没有打全、会不会一直重试。
    # 入参是 ``RegisterRequest``，返回 ``Rejection`` 列表（空列表表示放行）。
    reject_register: Callable[[registry_pb2.RegisterRequest], list[Any]] | None = None

    # 大于 0 时，收到这么多拍心跳后开始要求插件重新注册。
    #
    # 用来验证「实例被摘除后插件能自愈」——真中台里要等到心跳超时才会发生。
    reregister_after_beats: int = 0

    # ``Invoke`` 的下游替身。None 时 Invoke 一律回 ``outcome=ERROR``（reason 说明
    # mock 没装配下游）——**不假装成功**。注入的函数收到原始的 ``InvokeRequest``，
    # 返回 ``InvokeResponse``（或直接 ``context.abort`` 模拟基础设施故障）。
    invoke_handler: Callable[[gateway_pb2.InvokeRequest], Any] | None = None


@dataclass
class Registration:
    """一次被记录下来的注册请求。"""

    request: registry_pb2.RegisterRequest
    at: float


@dataclass
class GatewayCall:
    """一次被记录下来的网关请求，供断言用。

    ``token`` 单独记出来而不是让断言去翻 metadata：网关面的身份全凭它反查，
    「带没带、带的是哪张」是最常被测的一件事。
    """

    kind: str
    token: str
    request: Any


class _Service(registry_pb2_grpc.PluginRegistryServicer):
    def __init__(self, hub: "Hub") -> None:
        self._hub = hub

    def Register(
        self, request: registry_pb2.RegisterRequest, context: grpc.ServicerContext
    ) -> registry_pb2.RegisterResponse:
        return self._hub._on_register(request)

    def Heartbeat(
        self, request: registry_pb2.HeartbeatRequest, context: grpc.ServicerContext
    ) -> registry_pb2.HeartbeatResponse:
        return self._hub._on_heartbeat(request)

    def Unregister(
        self, request: registry_pb2.UnregisterRequest, context: grpc.ServicerContext
    ) -> registry_pb2.UnregisterResponse:
        return self._hub._on_unregister(request)


class _Gateway(gateway_pb2_grpc.PluginGatewayServicer):
    """网关面替身。只做鉴权、记录、转发（理由见模块文档），不做治理。"""

    def __init__(self, hub: "Hub") -> None:
        self._hub = hub

    def ListPlugins(
        self, request: gateway_pb2.ListPluginsRequest, context: grpc.ServicerContext
    ) -> gateway_pb2.ListPluginsResponse:
        self._hub._gateway_entry("ListPlugins", request, context)
        return self._hub._on_list_plugins(request)

    def DescribeMessage(
        self, request: gateway_pb2.DescribeMessageRequest, context: grpc.ServicerContext
    ) -> gateway_pb2.DescribeMessageResponse:
        self._hub._gateway_entry("DescribeMessage", request, context)
        return self._hub._on_describe_message(request)

    def GetContract(
        self, request: gateway_pb2.GetContractRequest, context: grpc.ServicerContext
    ) -> gateway_pb2.GetContractResponse:
        self._hub._gateway_entry("GetContract", request, context)
        return self._hub._on_get_contract(request, context)

    def Invoke(
        self, request: gateway_pb2.InvokeRequest, context: grpc.ServicerContext
    ) -> gateway_pb2.InvokeResponse:
        self._hub._gateway_entry("Invoke", request, context)
        return self._hub._on_invoke(request, context)


class Hub:
    """运行中的 mock 中台。"""

    def __init__(self, opts: Options | None = None) -> None:
        self.opts = opts or Options()
        if self.opts.heartbeat_interval_seconds <= 0:
            self.opts.heartbeat_interval_seconds = 1

        self._lock = threading.Lock()
        self._registrations: list[Registration] = []
        self._heartbeats = 0
        # 收到的**每一次**注销请求（含被拒的）都记下来，且记整个请求而不只是
        # instance_id——注销还要看凭证（见 unregister_tokens）。「发过没有」与
        # 「删成功了没有」是两件事：真中台拒了也不告诉插件（响应是个空消息）。
        self._unregisters: list[registry_pb2.UnregisterRequest] = []
        # 「实例行」：instance_id → 注册时下发给那一行的状态凭证。
        #
        # 真中台这张表在 Postgres 里，注销时「判定属主」与「删行」在**同一条 SQL**
        # 里完成；这里是够用的内存版，规则照抄：**空凭证或不符一律不删任何行**。
        self._instances: dict[str, str] = {}
        # 网关面收到的请求（含被拒的——「发过没有」与「成没成」是两件事）。
        self._gateway_calls: list[GatewayCall] = []

        self._server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
        registry_pb2_grpc.add_PluginRegistryServicer_to_server(_Service(self), self._server)
        gateway_pb2_grpc.add_PluginGatewayServicer_to_server(_Gateway(self), self._server)

        # 绑 127.0.0.1:0 让系统分配端口：测试并行跑时写死端口一定撞车。
        port = self._server.add_insecure_port("127.0.0.1:0")
        if port == 0:
            raise RuntimeError("mockhub: 监听失败")
        self.addr = f"http://127.0.0.1:{port}"
        self._server.start()

    # ------------------------------------------------------------ 生命周期

    def close(self) -> None:
        """停掉 mock 中台。"""
        self._server.stop(grace=None)

    def __enter__(self) -> "Hub":
        return self

    def __exit__(self, *exc_info: Any) -> None:
        self.close()

    # ------------------------------------------------------------ 断言用

    @property
    def registrations(self) -> list[Registration]:
        """收到的全部注册请求（含被拒的）。"""
        with self._lock:
            return list(self._registrations)

    @property
    def last_registration(self) -> registry_pb2.RegisterRequest | None:
        with self._lock:
            return self._registrations[-1].request if self._registrations else None

    @property
    def heartbeats(self) -> int:
        with self._lock:
            return self._heartbeats

    @property
    def unregisters(self) -> list[str]:
        """收到的注销请求里的实例 id，**含被拒的**。

        被拒的也算「收到过」：真中台的拒绝是个空响应，插件侧分不出成没成，所以
        「发过没有」只能看这里；「删掉了没有」看 :attr:`instances`。
        """
        with self._lock:
            return [request.instance_id for request in self._unregisters]

    @property
    def unregister_tokens(self) -> list[str]:
        """与 :attr:`unregisters` 同序的注销凭证。

        中台靠它认出「你注销的是不是自己那一行」——``instance_id`` 是插件自报的、
        可以撞，凭证才是属主证明。
        """
        with self._lock:
            return [request.state_token for request in self._unregisters]

    @property
    def instances(self) -> dict[str, str]:
        """当前还在的实例行：``instance_id`` → 那一行持有的状态凭证。

        注销有没有真的摘掉实例，只有看这里才算数——响应是空消息，
        :attr:`unregisters` 里那条请求可能刚刚被拒掉。
        """
        with self._lock:
            return dict(self._instances)

    @property
    def gateway_calls(self) -> list[GatewayCall]:
        """网关面收到的全部请求（含被拒的），按到达顺序。"""
        with self._lock:
            return list(self._gateway_calls)

    def wait_for_registration(self, count: int, timeout: float = 5.0) -> None:
        """等到至少收到 ``count`` 次注册，超时抛 ``TimeoutError``。"""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if len(self.registrations) >= count:
                return
            time.sleep(0.01)
        raise TimeoutError(
            f"mockhub: 等待第 {count} 次注册超时（已收到 {len(self.registrations)} 次）"
        )

    def wait_for_heartbeats(self, count: int, timeout: float = 5.0) -> None:
        """等到至少收到 ``count`` 拍心跳，超时抛 ``TimeoutError``。"""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.heartbeats >= count:
                return
            time.sleep(0.01)
        raise TimeoutError(f"mockhub: 等待第 {count} 拍心跳超时（已收到 {self.heartbeats} 拍）")

    # ------------------------------------------------------------ 服务端实现

    def _on_register(self, request: registry_pb2.RegisterRequest) -> registry_pb2.RegisterResponse:
        with self._lock:
            self._registrations.append(Registration(request=request, at=time.time()))

        if self.opts.reject_register is not None:
            rejections = self.opts.reject_register(request) or []
            if rejections:
                return registry_pb2.RegisterResponse(accepted=False, rejections=list(rejections))

        # 只有放行的注册才写下实例行——被拒的一方手里不该有凭证，中台那边也没有
        # 它这一行（这正是插件侧「没凭证就不发注销」那条规则的由来）。
        with self._lock:
            self._instances[request.instance_id] = MOCK_STATE_TOKEN

        return registry_pb2.RegisterResponse(
            accepted=True,
            instance_id=request.instance_id,
            heartbeat_interval_seconds=self.opts.heartbeat_interval_seconds,
            state_token=MOCK_STATE_TOKEN,
        )

    def _on_heartbeat(
        self, request: registry_pb2.HeartbeatRequest
    ) -> registry_pb2.HeartbeatResponse:
        with self._lock:
            self._heartbeats += 1
            beats = self._heartbeats

        require_reregister = self.opts.reregister_after_beats > 0 and beats > self.opts.reregister_after_beats

        return registry_pb2.HeartbeatResponse(
            accepted=not require_reregister,
            heartbeat_interval_seconds=self.opts.heartbeat_interval_seconds,
            reregister_required=require_reregister,
        )

    def _on_unregister(
        self, request: registry_pb2.UnregisterRequest
    ) -> registry_pb2.UnregisterResponse:
        with self._lock:
            self._unregisters.append(request)

            # 照真中台的规则：**空凭证、凭证不符、行不存在，一律不删任何行**。
            #
            # 这条不能宽松。`instance_id` 是插件自报的、可以跨插件相撞（缺省
            # 「主机名-PID」，同一 host 网络下容器 PID 又都是 1，必然撞），只按
            # instance_id 删行的话，谁先退出谁就把**对方**那一行删掉——而 mock 比
            # 真中台宽松的后果是：插件在本地跑绿，到线上才发现注销一直被拒。
            if request.state_token and self._instances.get(request.instance_id) == request.state_token:
                del self._instances[request.instance_id]

        return registry_pb2.UnregisterResponse()

    # ------------------------------------------------------------ 网关面

    def _gateway_entry(self, kind: str, request: Any, context: grpc.ServicerContext) -> None:
        """网关面各 RPC 的公共头：记录 + 鉴权。**先记录再判**——被拒的请求也算
        「收到过」（与注销同款：响应是 abort，插件侧分不出成没成）。

        mock 只下发过一张固定凭证，所以「凭证等于它」就等于「来自注册过的实例」；
        更细的 per-instance 区分 mock 没有（真中台按凭证反查插件名，mock 单租户
        单 token 的简化在 ``MOCK_STATE_TOKEN`` 的注释里）。凭证不符回
        ``UNAUTHENTICATED``，与真中台同强度。
        """
        token = ""
        for key, value in context.invocation_metadata():
            if key == "x-hub-state-token":
                token = value
                break
        with self._lock:
            self._gateway_calls.append(GatewayCall(kind=kind, token=token, request=request))
        if token != MOCK_STATE_TOKEN:
            context.abort(grpc.StatusCode.UNAUTHENTICATED, "状态凭证无效或已失效")

    def _on_invoke(
        self, request: gateway_pb2.InvokeRequest, context: grpc.ServicerContext
    ) -> gateway_pb2.InvokeResponse:
        handler = self.opts.invoke_handler
        if handler is None:
            # 缺省不假装成功：reason 把「这是 mock、没有下游」说清楚，
            # 插件在 mock 上跑互调时一眼能看出差的是哪一环
            return gateway_pb2.InvokeResponse(
                outcome=gateway_pb2.ERROR,
                reason="mock 中台没有装配 Invoke 的下游替身（Options.invoke_handler）",
            )
        return handler(request)

    def _on_describe_message(
        self, request: gateway_pb2.DescribeMessageRequest
    ) -> gateway_pb2.DescribeMessageResponse:
        # mock 的注册表只有「谁注册过」，没有契约反查索引——空答案是诚实答案
        return gateway_pb2.DescribeMessageResponse()

    def _on_list_plugins(
        self, request: gateway_pb2.ListPluginsRequest
    ) -> gateway_pb2.ListPluginsResponse:
        # 从注册记录聚合：mock 没有独立的「在线探测」，注册行还在（instances 里
        # 有那一行）就算在线。数据全部来自插件自己注册时报的内容，不是编造的
        with self._lock:
            records = list(self._registrations)
            instances = dict(self._instances)
        by_plugin: dict[str, registry_pb2.RegisterRequest] = {}
        online_of: dict[str, int] = {}
        for record in records:
            row = record.request
            by_plugin[row.plugin_name] = row  # 同名多次注册取最后一条（最近的自述）
            if row.instance_id in instances:
                online_of[row.plugin_name] = online_of.get(row.plugin_name, 0) + 1

        plugins = []
        for name, row in by_plugin.items():
            online = online_of.get(name, 0) > 0
            if not request.include_offline and not online:
                continue
            manifest = row.manifest or plugin_pb2.PluginManifest()
            plugins.append(
                gateway_pb2.PluginSummary(
                    name=name,
                    latest_version=row.version,
                    online=online,
                    instance_count=online_of.get(name, 0),
                    description=manifest.description,
                )
            )
        return gateway_pb2.ListPluginsResponse(plugins=plugins)

    def _on_get_contract(
        self, request: gateway_pb2.GetContractRequest, context: grpc.ServicerContext
    ) -> gateway_pb2.GetContractResponse:
        with self._lock:
            rows = [
                record.request
                for record in self._registrations
                if record.request.plugin_name == request.plugin
            ]
        row = None
        for candidate in rows:  # version 空 = 取最后一次注册的版本；否则精确匹配
            if request.version and candidate.version != request.version:
                continue
            row = candidate
        # 与真中台同语义：「要调的对象不存在」是 NOT_FOUND，不是空答案
        if row is None:
            context.abort(
                grpc.StatusCode.NOT_FOUND,
                f"插件 {request.plugin} 没有版本 {request.version or '(最新)'}",
            )
        manifest = row.manifest or plugin_pb2.PluginManifest()
        # schema_json 恒空：摊平 descriptor 是真中台 hub-contract 的能力，mock
        # 不复刻——「这里看不到 schema」与「schema 为空」是两件事，文档里写明了
        return gateway_pb2.GetContractResponse(
            name=row.plugin_name,
            version=row.version,
            produces=list(manifest.produces),
            consumes=list(manifest.consumes),
            invokes=list(manifest.invokes),
            tools=list(manifest.tools),
        )


# ---------------------------------------------------------------- 调用插件


class PluginClient:
    """指向某个插件运行时（``hub.v1.PluginRuntime``）的客户端。

    供契约一致性自检与调试工具复用——与 ``sdk/go/mockhub.Client`` 对应。
    """

    def __init__(self, channel: grpc.Channel) -> None:
        self._channel = channel
        self._stub = plugin_pb2_grpc.PluginRuntimeStub(channel)

    def close(self) -> None:
        self._channel.close()

    def __enter__(self) -> "PluginClient":
        return self

    def __exit__(self, *exc_info: Any) -> None:
        self.close()

    def describe(self, timeout: float = 5.0) -> Any:
        return self._stub.Describe(plugin_pb2.DescribeRequest(), timeout=timeout)

    def validate(self, envelope: Any, timeout: float = 5.0) -> Any:
        return self._stub.Validate(plugin_pb2.ValidateRequest(envelope=envelope), timeout=timeout)

    def handle(self, envelope: Any, timeout: float = 5.0) -> Any:
        response = self._stub.Handle(plugin_pb2.HandleRequest(envelope=envelope), timeout=timeout)
        if not response.HasField("envelope"):
            raise ValueError("mockhub: 插件未返回信封")
        return response.envelope

    def health(self, timeout: float = 5.0) -> Any:
        return self._stub.Health(plugin_pb2.HealthRequest(), timeout=timeout)


def dial_plugin(addr: str) -> PluginClient:
    """连上一个插件的 gRPC 地址（如 ``http://127.0.0.1:9000``）。"""
    target = addr
    for prefix in ("https://", "http://"):
        if target.startswith(prefix):
            target = target[len(prefix):]
            break
    return PluginClient(grpc.insecure_channel(target))


def free_addr() -> str:
    """返回一个当前空闲的 ``127.0.0.1`` 地址。

    测试里插件要先知道自己的对外地址（中台会连它做可达性探测），而监听端口由
    系统分配时拿不到真实端口——用它先探一个再显式指定。存在极小的竞态窗口，
    测试场景可接受。
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return f"127.0.0.1:{sock.getsockname()[1]}"
