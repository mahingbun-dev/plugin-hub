"""服务端骨架：起 gRPC 服务、自注册、心跳、被摘除后自愈、优雅退出。

与 ``sdk/go/hubkit/run.go`` 行为对齐。几个刻意的行为（与 Go 侧逐条相同）：

* **注册会一直重试**：中台可能比插件晚起来，插件先启动是常态。
* **被摘除后自动重新注册**：心跳响应里带 ``reregister_required`` 时重走注册流程，
  这是实例掉线后能自愈的关键。
* **优雅退出时主动注销**：中台据此立刻摘掉实例，不必等心跳超时。注销要**凭注册时
  下发的状态凭证**认属主；没拿到过凭证（注册从没成功过）就整个跳过，不发这个注定
  被拒的请求——理由见 :meth:`Registrar.unregister`。
* **状态凭证失效后自动重新注册**：HubState 调用撞上 ``UNAUTHENTICATED`` 时重走注册
  流程换新凭证（中台重启轮换凭证、实例被摘除）。这是防御层，不是主恢复路径——
  中台把凭证落了库，重启对插件是透明的。
* **注册成功后把状态客户端注入插件**（:meth:`hubkit.Plugin.set_state`）：凭证只有
  中台知道，插件作者不该自己管 token。每次重新注册都会再注入一次。
* **健康探测默认恒报健康、不问插件**；插件定义了 ``health()`` 才调它（可选覆写，
  取舍见 :meth:`_RuntimeService.Health`）。

差异只有一处，是被迫的：Go 用 ``context.Context`` 传播取消，Python 用
:class:`threading.Event`。因此对外暴露两个入口——:func:`run` 装信号处理、
:func:`serve` 由调用方决定何时结束（测试与嵌入式场景用）。
"""

from __future__ import annotations

import signal
import threading
import time
from concurrent import futures
from typing import Any, Iterator

import grpc

from .config import (
    HEARTBEAT_FALLBACK_INTERVAL,
    Config,
    config_from_env,
)
from .envelope import invalid, is_well_known_fq_name, issue
from .gateway import GatewayClient
from .log import JsonLogger
from .plugin import Plugin
from .proto.hubv1 import plugin_pb2, plugin_pb2_grpc, registry_pb2, registry_pb2_grpc
from .state import DenialSignal, StateClient

# 注销时给中台的原因。中台的审计日志里会带上它，写清楚比写 "bye" 有用。
UNREGISTER_REASON = "插件优雅退出"

# 注销调用的时间上限（秒）。比常规调用短：这时候进程已经在退出路径上，
# 中台不响应也不该让插件卡在这里不退出——心跳超时兜底本来就会摘掉实例。
UNREGISTER_TIMEOUT = 3.0

# gRPC 服务的线程数上限。
#
# Go 那边每个 RPC 起一个 goroutine（无上限）；Python 的线程是真实的操作系统线程，
# 无上限地开会在中台突发重试时把内存吃光。取 16 而不是更大：插件的 Handle 大多
# 是「等下游 IO」，而真正的并发度由中台侧的调用并发决定，这里的数字只影响
# 「一次能同时接多少个」的上限。
DEFAULT_WORKERS = 16

# 心跳循环里等待的分片上限（秒）。
#
# Python 没有 Go 的 ``select``，等不了「下一拍心跳」与「状态凭证被拒」里先到的那个，
# 所以按片轮询。心跳的节拍用**绝对时刻**算，分片不会让它漂移；被拒信号与 stop 的
# 响应延迟上界就是这一片，相对心跳周期（秒级以上）可以忽略。
HEARTBEAT_WAKE_SLICE = 0.25


class RegistrationRejected(Exception):
    """中台拒绝了注册。

    拒绝原因是结构化的；``str()`` 会把它们逐条排开——插件作者照着改就行，
    不必去翻中台的日志。
    """

    def __init__(self, rejections: Any) -> None:
        self.rejections = list(rejections or [])
        super().__init__(self._describe())

    def _describe(self) -> str:
        if not self.rejections:
            return "中台拒绝了注册（未给出原因）"
        lines = ["中台拒绝了注册："]
        for rejection in self.rejections:
            lines.append(f"  - {reject_code_name(rejection.code)}: {rejection.message}")
            if rejection.detail:
                lines.append(f"      {rejection.detail}")
        return "\n".join(lines)


def reject_code_name(code: int) -> str:
    """把拒绝码翻成人话。

    取的是 proto 里那个枚举的**名字**而不是自己维护一张表：自己维护的表会在
    proto 加码时无声地过期，而枚举名是随生成代码一起更新的。
    """
    try:
        name = registry_pb2.RejectCode.Name(code)
    except ValueError:
        return f"未知拒绝码({code})"
    return name.removeprefix("REJECT_CODE_")


# ---------------------------------------------------------------- gRPC 服务


class _RuntimeService(plugin_pb2_grpc.PluginRuntimeServicer):
    """把 :class:`Plugin` 适配成 ``hub.v1.PluginRuntime`` 服务。

    这一层负责的是**协议层的形状**（缺信封怎么办、插件抛异常怎么变成状态码），
    业务语义一概不碰——那是 :class:`Plugin` 的事。
    """

    def __init__(self, plugin: Plugin, logger: JsonLogger) -> None:
        self._plugin = plugin
        self._log = logger

    def Describe(self, request: Any, context: grpc.ServicerContext) -> plugin_pb2.PluginManifest:
        manifest = self._plugin.manifest()
        # 中台拿这个响应跟注册时提交的 manifest 核对；返回 None 会变成
        # 一个「插件描述了自己是空的」的响应，中台判成 MANIFEST_INVALID——
        # 比在这里直接炸更难查。
        return manifest if manifest is not None else plugin_pb2.PluginManifest()

    def Validate(
        self, request: plugin_pb2.ValidateRequest, context: grpc.ServicerContext
    ) -> plugin_pb2.ValidateResponse:
        if not request.HasField("envelope"):
            # 空信封是合法请求：中台在可达性探测里就可能发一个。返回一条
            # 可定位的问题，而不是抛异常——抛异常会让探测判成「插件不健康」。
            return invalid(issue("envelope", "缺少信封"))

        try:
            return self._plugin.validate(context, request.envelope)
        except Exception as err:  # noqa: BLE001 —— 插件抛什么都得转成状态码
            self._log.error("校验器执行失败", err=str(err))
            context.abort(grpc.StatusCode.INTERNAL, f"校验器执行失败: {err}")

    def Handle(
        self, request: plugin_pb2.HandleRequest, context: grpc.ServicerContext
    ) -> plugin_pb2.HandleResponse:
        if not request.HasField("envelope"):
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, "缺少信封")

        try:
            out = self._plugin.handle(context, request.envelope)
        except Exception as err:  # noqa: BLE001
            # 插件自己的错误原样上报：中台会把它归到「插件调用失败」，调用方据此重试。
            # 在这里吞掉改成「返回空信封」会让调用方拿到一个看似成功的响应。
            self._log.error("插件处理失败", err=str(err))
            context.abort(grpc.StatusCode.INTERNAL, f"插件处理失败: {err}")

        if out is None:
            # 插件体契约上必须返回信封。返回 None 时直接构造 HandleResponse 会抛
            # TypeError，落到 grpcio 那里变成一个没有 message 的 UNKNOWN 状态——
            # 中台侧只看到「插件异常」，而插件侧什么都没打出来。这里明确报出来。
            self._log.error("插件体返回了空信封")
            context.abort(
                grpc.StatusCode.INTERNAL,
                "插件体返回了空信封 —— handle() 必须返回 envelope（中台会把它当成插件异常）",
            )

        return plugin_pb2.HandleResponse(envelope=out)

    def HandleStream(
        self, request: plugin_pb2.HandleRequest, context: grpc.ServicerContext
    ) -> Iterator[plugin_pb2.HandleResponse]:
        # 与 Go 侧一致地返回 UNIMPLEMENTED，而不是给个假实现：中台对
        # server-streaming 的处理尚未落地（见 docs/design.md 的载荷边界），
        # 给个「能跑但语义不对」的流式版本会让中台侧按错的前提去接线。
        context.abort(
            grpc.StatusCode.UNIMPLEMENTED,
            "流式处理尚未实现（见 docs/design.md 的载荷边界，随 M3 落地）",
        )
        yield  # 让本函数成为**生成器函数**：grpcio 要求流式处理器返回迭代器

    def Health(
        self, request: plugin_pb2.HealthRequest, context: grpc.ServicerContext
    ) -> plugin_pb2.HealthResponse:
        # 缺省**恒报健康，而且根本不问插件**（与 Go 侧的 ``runtimeService.Health``
        # 逐字一致）。这是刻意的取舍：中台在**注册期**就会探测这个接口，一次不健康
        # 会让注册被拒——于是「启动瞬间下游还没连上」就变成「这个插件压根注册不上」，
        # 得等下一轮重试才可能好。把「健康不健康」留给中台对业务接口的观测，比让插件
        # 自证健康更不容易把一次抖动放大成注册失败。
        #
        # 但有些插件确实有值得探测的依赖，所以留一个**可选覆写**的口子：插件定义了
        # ``health()`` 就调它。探测用 ``getattr`` 而不是往基类加一个默认实现——与
        # ``Registrar._inject_state`` 探 ``set_state`` 同一条路子，鸭子类型、不继承
        # :class:`Plugin` 的插件也享受得到；这里还多一条非这么写不可的理由：基类一旦
        # 有了 ``health()``，「本插件没覆写」（应当恒健康、**根本不去探测**）与
        # 「覆写了」就分不出来，而那正是这个口子的全部语义。
        #
        # 插件必须返回 :class:`plugin_pb2.HealthResponse`。
        #
        # **代价写在前面，用之前先想清楚**：覆写后返回 ``healthy=False`` 会让注册期的
        # 健康探测失败、进而**注册被拒**，重试会一直撞同一面墙。所以默认不调它；
        # 真要用，就得自己保证「探测不通过」只发生在确实不该接流量的时候。
        health = getattr(self._plugin, "health", None)
        if not callable(health):
            return plugin_pb2.HealthResponse(healthy=True, message="ok")

        try:
            response = health()
        except Exception as err:  # noqa: BLE001 —— 探测炸了也得给出一个明确答复
            # 不吞成 healthy=True：插件自己写的探测有 bug 时报健康，等于把「探测不可信」
            # 伪装成「依赖都好」，而覆写 health() 恰恰就是为了不再这样。也不让它抛出去
            # ——那会变成一个没有 message 的 UNKNOWN，中台侧只看到「RPC 失败」，
            # 插件侧什么都没打出来。
            self._log.error("健康探测失败", err=str(err))
            return plugin_pb2.HealthResponse(healthy=False, message=f"健康探测失败: {err}")

        if response is None:
            # 忘了 return 时，直接交给 grpcio 会变成一个没有 message 的 UNKNOWN；
            # 明确报出来，插件作者一眼能看出是自己少写了一行。
            self._log.error("health() 没有返回 HealthResponse")
            return plugin_pb2.HealthResponse(
                healthy=False, message="health() 必须返回 plugin_pb2.HealthResponse"
            )
        return response


# ---------------------------------------------------------------- 连接与注册


class _HubConnection:
    """指向中台插件面的一条 gRPC 连接。

    注册、心跳、注销、以及状态调用（HubState）共用**同一条**连接：前三条是低频控制流，
    状态调用虽然频繁但每次都很小，各开一条的话中台侧看到的连接数会成倍增长，
    而它们之间没有任何隔离上的好处。
    """

    def __init__(self, cfg: Config) -> None:
        target = cfg.grpc_target()
        if cfg.use_tls():
            # 中台证书若由内网 CA 签发，用 GRPC_DEFAULT_SSL_ROOTS_FILE_PATH 指到
            # CA bundle；grpc-python 在这点上与 Go 不同——Go 在 macOS 上不读
            # SSL_CERT_FILE，而 Python 这边读的是环境变量（见 README）。
            self._channel = grpc.secure_channel(target, grpc.ssl_channel_credentials())
        else:
            self._channel = grpc.insecure_channel(target)

        self.registry = registry_pb2_grpc.PluginRegistryStub(self._channel)

    @property
    def channel(self) -> grpc.Channel:
        """底层连接。状态客户端挂在同一条上（与 Go 的 ``dial`` 复用 conn 一致）。"""
        return self._channel

    def close(self) -> None:
        self._channel.close()


class Registrar:
    """维持「注册 → 心跳 → 被摘除则重新注册」的循环。

    单独成类不是为了好看：-「心跳响应里带 ``reregister_required`` 时会重新注册」
    这条**只能**通过驱动这个循环来验证，而它在 Go 侧是一个跑在 goroutine 里的
    私有方法。放在这里，测试可以直接用 mock 中台把它跑起来断言。
    """

    def __init__(self, conn: _HubConnection, plugin: Plugin, cfg: Config, logger: JsonLogger) -> None:
        self._conn = conn
        self._plugin = plugin
        self._cfg = cfg
        self._log = logger
        self._interval = HEARTBEAT_FALLBACK_INTERVAL
        # 注册次数。测试与运维都用得上：数值不涨就说明注册循环卡住了。
        self.register_count = 0

        # denied 是状态客户端与注册循环之间的那根线：插件侧撞上 401 时，心跳循环
        # 会收到它并立刻重走注册流程换新凭证。
        self._denied = DenialSignal()
        self._state = StateClient(
            conn.channel,
            call_timeout=cfg.state_call_timeout,
            denied=self._denied,
        )
        # 网关客户端与状态客户端共用同一条 channel、同一张凭证、同一个 401 信号
        # ——中台就是拿同一张 x-hub-state-token 认「状态面的写入者」与「网关面的
        # 调用方」，客户端这边分成两个对象只是职责分开，生命周期必须绑在一起。
        self._gateway = GatewayClient(
            conn.channel,
            call_timeout=cfg.state_call_timeout,
            denied=self._denied,
        )
        # 最近一次注册成功的时刻（单调钟），用于给「denial 触发的强制重注册」设速率下限。
        # 用 monotonic 而不是 time.time()：后者会被系统对时拨动，冷却窗口跟着乱跳。
        self._last_register = 0.0

    @property
    def state(self) -> StateClient:
        """本次运行唯一的状态客户端（注册成功后注入给插件的就是它）。"""
        return self._state

    @property
    def gateway(self) -> GatewayClient:
        """本次运行唯一的网关客户端（发现与互调；注入时机同状态客户端）。"""
        return self._gateway

    @property
    def heartbeat_interval(self) -> float:
        """当前生效的心跳周期（秒）。注册成功后由中台指定。"""
        return self._interval

    def loop(self, stop: threading.Event) -> None:
        """跑到 ``stop`` 被设置为止。"""
        self._interval = HEARTBEAT_FALLBACK_INTERVAL

        while not stop.is_set():
            try:
                self.register_once()
            except Exception as err:  # noqa: BLE001 —— 注册失败的种类都要重试
                if stop.is_set():
                    return
                self._log_register_failure(err)
                if not _sleep(stop, self._cfg.retry_interval):
                    return
                continue

            # 成功注册后进心跳循环。
            # **它返回即表示需要重新注册**（或 stop 被设置），这里不做延迟——
            # 被摘除后的自愈速度就取决于这一点：多一个 retry_interval 的等待
            # 意味着实例掉线后要空转 5 秒才回来。
            self.heartbeat_loop(stop)

    def register_once(self) -> None:
        """走一次注册。失败时抛异常，由 :meth:`loop` 决定重试。

        注册成功还顺带做两件事：**换状态凭证**、**把状态客户端注入插件**。放在这里
        而不是 ``loop`` 里，是因为「被摘除后自愈」「凭证被拒后重注册」两条路径都会
        走回这里——它们要的正是同一件事：拿到此刻有效的那张凭证。
        """
        manifest = self._plugin.manifest()
        request = registry_pb2.RegisterRequest(
            plugin_name=manifest.name,
            version=manifest.version,
            instance_id=self._cfg.instance_id,
            advertise_addr=self._cfg.advertise_addr,
            manifest=manifest,
            descriptor_set=self._plugin.descriptor() or b"",
        )

        try:
            response = self._conn.registry.Register(request, timeout=self._cfg.call_timeout)
        except grpc.RpcError as err:
            raise RuntimeError(f"调用中台注册接口失败: {err}") from err

        if not response.accepted:
            raise RegistrationRejected(response.rejections)

        # 记下成功注册的时刻：denial 触发的强制重注册靠它做速率下限（见 _on_denied）
        self._last_register = time.monotonic()

        # 凭证随每次注册轮换，这里覆盖旧的。
        #
        # 这一句同时是**注销凭证**的来源（见 unregister）：中台认的是「最近一次注册
        # 成功下发的那张凭证」，就是这里写下的这一个。它与上面那行 ``_last_register``
        # 紧挨着、中间没有任何会失败或阻塞的调用，不会出现「时刻已经更新、凭证还是
        # 上一张」的中间态。
        self._state.set_token(response.state_token)
        # 网关客户端与状态客户端持同一张凭证，必须同拍换——只换一边的话，
        # 「状态调用好用、互调被 401」（或反之）就是一行漏改写出来的裂缝
        self._gateway.set_token(response.state_token)
        if not response.state_token:
            self._log.warn("中台未下发状态凭证，HubState 与插件互调将不可用")
        # 每次注册后都注入一次：插件可能还持有上一个凭证时期的客户端引用
        self._inject_state()
        self._inject_gateway()

        if response.heartbeat_interval_seconds > 0:
            self._interval = float(response.heartbeat_interval_seconds)

        for warning in response.warnings:
            self._log.warn("中台提示", warning=warning)

        self.register_count += 1
        self._log.info(
            "已注册到中台",
            plugin=manifest.name,
            version=manifest.version,
            instance=response.instance_id,
        )

    def heartbeat_loop(self, stop: threading.Event) -> None:
        """按中台指定的周期续期；返回即表示需要重新注册（或 ``stop`` 被设置）。

        会让它返回的有三条：中台要求重注册、心跳被拒、插件侧的状态调用撞上 401。
        最后一条受 ``retry_interval`` 这个冷却窗口约束（见 :meth:`_on_denied`）。
        """
        interval = self._interval
        next_beat = time.monotonic() + interval

        while not stop.is_set():
            # 先等「下一拍」或「denial」，哪个先到（Go 那边是 select 两个 case）。
            # 分片轮询的理由见 HEARTBEAT_WAKE_SLICE。
            remaining = next_beat - time.monotonic()
            if remaining > 0 and self._denied.wait(min(remaining, HEARTBEAT_WAKE_SLICE)):
                if self._on_denied():
                    return
                continue
            if stop.is_set():
                return
            if time.monotonic() < next_beat:
                continue

            # 节拍按绝对时刻推进，上面那些分片等待与 denial 处理都不会让它漂移
            next_beat = time.monotonic() + interval

            try:
                response = self._conn.registry.Heartbeat(
                    registry_pb2.HeartbeatRequest(instance_id=self._cfg.instance_id),
                    timeout=self._cfg.call_timeout,
                )
            except grpc.RpcError as err:
                # 网络抖动不该让插件停止心跳，下一拍继续。
                # 这里**不**返回：一次失败就重注册会让中台侧看到注册风暴。
                self._log.warn("心跳失败", err=str(err))
                continue

            if not response.accepted or response.reregister_required:
                self._log.warn("中台要求重新注册（实例可能已被摘除）")
                return

    def _on_denied(self) -> bool:
        """处理一次「状态凭证被拒」信号；返回 True 表示该重新注册了。

        为什么会有这个信号：插件侧的状态调用被中台判了 401——凭证多半已被吊销或轮换。
        心跳本身可能一切正常（实例还在库里），不重注册就会一直哑下去。

        但必须有速率下限：denial 可能连续不断（中台校验滞后、撤销尚未传播，或非凭证
        原因也回 401）。没有它，一次成功注册之后紧接着排空信号就是**零延迟**，注册
        速率等于 Register RPC 的延迟——无限自旋，同时还在反复探测插件自己的地址。
        窗口内到达的信号直接丢掉。
        """
        self._denied.consume()
        since = time.monotonic() - self._last_register
        if since < self._cfg.retry_interval:
            self._log.warn(
                "状态凭证被拒，但距上次注册不足冷却窗口，忽略本次",
                since=f"{since:.3f}s",
                cooldown=f"{self._cfg.retry_interval}s",
            )
            return False
        self._log.warn("状态凭证被拒，重新注册以换取新凭证")
        return True

    def _inject_state(self) -> None:
        """把状态客户端交给插件（若它想要）。

        用 ``getattr`` 探测而不是硬要求 :meth:`hubkit.Plugin.set_state`：鸭子类型的
        插件（只实现四个必需方法、没继承 :class:`hubkit.Plugin`）在这一步不该出问题
        ——对应 Go 的「可选接口，老插件一行不用改」。

        插件自己的 ``set_state`` 抛异常**不会**让注册失败：注册在网络上已经成功了，
        把它当失败会变成一轮又一轮的重注册风暴（日志里还只写着「注册未通过」）。
        注入失败打 ERROR 说清楚，然后继续。
        """
        set_state = getattr(self._plugin, "set_state", None)
        if not callable(set_state):
            return
        try:
            set_state(self._state)
        except Exception as err:  # noqa: BLE001 —— 插件抛什么都只影响注入这一步
            self._log.error("状态客户端注入失败（插件用了 HubState 的话会拿不到）", err=str(err))

    def _inject_gateway(self) -> None:
        """把网关客户端交给插件（若它想要）。取舍与 :meth:`_inject_state` 完全相同
        （鸭子类型探测、注入失败不让注册失败），不重复一遍论证。
        """
        set_gateway = getattr(self._plugin, "set_gateway", None)
        if not callable(set_gateway):
            return
        try:
            set_gateway(self._gateway)
        except Exception as err:  # noqa: BLE001 —— 插件抛什么都只影响注入这一步
            self._log.error("网关客户端注入失败（插件要做互调的话会拿不到）", err=str(err))

    def unregister(self) -> None:
        """主动注销。中台据此立刻摘掉实例，不必等心跳超时。

        **没拿到过凭证就整个跳过**。凭证只在注册成功时下发，为空说明本实例压根没进过
        注册表（被中台拒了、或还没注册上就退出了），没有实例行可摘除。而 ``instance_id``
        是插件自报的、可以跟别的插件撞（缺省「主机名-PID」，同一 host 网络下容器 PID
        又都是 1）——此时发一次不带身份的注销，中台若只按 ``instance_id`` 删行，删掉的
        正是**对方**那一行：实测 auth 重启一次，sql-executor 的工具就从 MCP 工具面上
        全部消失，而它自己的日志停在「已注册到中台」之后毫无异常。

        中台侧现在也会拒（见 ``UnregisterRequest.state_token``），插件侧这一道是别发
        这个注定被拒的请求。

        代价，与 Go 侧一致、也是有意为之：**插件先于中台升级**时，旧中台的
        ``RegisterResponse`` 里没有这个字段，SDK 拿到的是空串，于是注销整个跳过——对
        旧中台也就不再有优雅注销，只能等它心跳超时摘除。窗口是有界的（中台升完就
        恢复），而反过来放行的代价是可能删掉别人的实例行。
        """
        # 凭证直接读状态客户端手里那份，**不另存一份**：「状态凭证」与「注销凭证」
        # 是同一个东西——都是最近一次注册成功时中台下发的
        # ``RegisterResponse.state_token``。中台侧读的也是同一列
        # ``plugin_instances.state_token``（HubState 认证走
        # ``hub_store::instances::plugin_of_state_token``，注销判属主走
        # ``delete_instance_with_token``），客户端这边跟着分成两份反而制造了「两份
        # 都说是最新、却可能不同步」的缝：漂移的表现是「HubState 好用、注销却被拒」
        # （或反之），排查时两边各看一半、都对。
        #
        # 之所以读起来自然：``self._state`` 是本类自己的成员，读它不引入任何额外
        # 耦合，也不需要另一把锁——``StateClient.token`` 自己就是锁着读的。（Go 那边
        # 在注册器上另存了一份 ``stateToken``：那边同样拿得到状态客户端，多存一份是
        # 历史选择而不是约束，没有理由跟着抄。）
        #
        # 唯一的让步：状态客户端会注入给插件，而 ``StateClient.set_token`` 是公开的，
        # 插件乱调会把注销凭证一起改坏。后果有界——中台拒掉那张凭证，注销退化成心跳
        # 超时摘除（删不掉别人的行）——而且会那样写的插件，本来就已经把自己的状态
        # 调用弄坏了。
        token = self._state.token
        if not token:
            self._log.info(
                "本实例没有注册凭证，跳过主动注销（没注册成功就没有可摘除的实例行）"
            )
            return

        try:
            self._conn.registry.Unregister(
                registry_pb2.UnregisterRequest(
                    instance_id=self._cfg.instance_id,
                    reason=UNREGISTER_REASON,
                    state_token=token,
                ),
                timeout=UNREGISTER_TIMEOUT,
            )
        except grpc.RpcError as err:
            # 退不干净不算失败：中台还有心跳超时那条兜底路径。
            self._log.warn("注销失败（中台会在心跳超时后自行摘除）", err=str(err))
            return

        # 成功也打一行，Go 与 main 那份 Python 都没有这一行——这里是刻意的：
        # 「注销成没成」插件侧本来完全不可观测（中台回的是空响应，被拒也一样），
        # 而这正是上个事故里最缺的线索（只有「实例为何消失了」这样一个反过来的
        # 问题，日志里什么都说不上）。写成 INFO 与「已注册到中台」对称，退出路径
        # 的进出两头就都能在日志里对上。
        #
        # 措辞不给结果下定论：中台认不认这张凭证，插件看不到（响应是空的），这里
        # 只说「请求发出去了」这一件确凿的事。
        self._log.info("已主动注销（中台据此立刻摘除本实例）", instance=self._cfg.instance_id)

    def _log_register_failure(self, err: Exception) -> None:
        """把一次注册失败讲成「看一眼就懂」的样子。

        中台拒绝时给的是结构化原因，这里让每条原因各占一行。日志是 JSON 的，
        一整段多行文本会被转义成 \\n 塞进单个字段——一行里糊着 N 条原因，
        得靠人脑反解析才看得出哪条是哪条。拆成 code / message / detail 三列之后，
        每行本身就是完整的一条，读日志不需要任何工具。

        重试间隔单独收尾一行，而不是跟在每条原因后面：它是「接下来会怎样」，
        与「错在哪」不是一回事，逐条重复只会把原因行淹掉。``reasons`` 是原因条数，
        用来兜底——日志被截断时，一眼能看出还有几条没打出来。
        """
        if isinstance(err, RegistrationRejected) and err.rejections:
            for rejection in err.rejections:
                self._log.error(
                    "中台拒绝了注册",
                    code=reject_code_name(rejection.code),
                    message=rejection.message,
                    detail=rejection.detail,
                )
            self._log.error(
                "注册未通过，稍后重试",
                reasons=len(err.rejections),
                retry_in=f"{self._cfg.retry_interval}s",
            )
            return

        # 连不上中台、网络抖动这类错误本身就是单行的，和重试间隔打在同一行里正好。
        self._log.error(
            "注册未通过，稍后重试",
            err=str(err),
            retry_in=f"{self._cfg.retry_interval}s",
        )


def _sleep(stop: threading.Event, seconds: float) -> bool:
    """可被 ``stop`` 打断的等待；返回 False 表示被要求停止。"""
    return not stop.wait(seconds)


# ---------------------------------------------------------------- 入口


def check_manifest(plugin: Plugin) -> None:
    """在启动时就把 manifest 的明显问题挡住。

    这些问题中台也会拒，但那是网络往返之后的事——本地先炸能省一轮排查。
    """
    manifest = plugin.manifest()
    if manifest is None:
        raise ValueError("hubkit: manifest() 返回了 None")
    if not (manifest.name or "").strip():
        raise ValueError("hubkit: manifest 缺少插件名（name）")
    if not (manifest.version or "").strip():
        raise ValueError("hubkit: manifest 缺少版本号（version）——flow 靠它锁定实例")

    # 空 descriptor 本身合法：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的
    # proto。但声明了自有类型的就必须提供，否则中台会拒（声明的类型找不到出处）。
    if not plugin.descriptor():
        for contract in list(manifest.produces) + list(manifest.consumes):
            if not is_well_known_fq_name(contract.fq_name):
                raise ValueError(
                    f"hubkit: manifest 声明了自有类型 {contract.fq_name}，"
                    "但 descriptor() 返回空——要么把它从 produces/consumes 里去掉，"
                    "要么提供它的 proto"
                )


def run(plugin: Plugin, cfg: Config | None = None) -> None:
    """启动插件，直到收到 SIGINT / SIGTERM。

    插件作者只需要实现 :class:`Plugin`。
    """
    cfg = (cfg or config_from_env()).with_defaults()
    stop = threading.Event()
    _install_signal_handlers(stop, cfg.logger)
    serve(plugin, cfg, stop)


def serve(plugin: Plugin, cfg: Config | None = None, stop: threading.Event | None = None) -> None:
    """:func:`run` 的主体，由调用方决定何时结束。

    测试与嵌入式场景用它：给一个 ``Event`` 就能把插件干净地停掉，不必真的发信号。
    """
    cfg = (cfg or config_from_env()).with_defaults()
    cfg.validate()  # 放在 with_defaults 之后：缺省值补完了才谈得上「缺没缺」
    logger = cfg.logger
    stop = stop or threading.Event()

    check_manifest(plugin)

    server = grpc.server(futures.ThreadPoolExecutor(max_workers=DEFAULT_WORKERS))
    plugin_pb2_grpc.add_PluginRuntimeServicer_to_server(_RuntimeService(plugin, logger), server)

    # 连接先于服务建起来：中台的注册校验里有一道**可达性探测**，插件一旦开始监听就
    # 可能被探到。让「能监听」早于「能注册」的话，中台侧会看到一个探得通、却永远
    # 注册不上的地址——那是最难排查的一种状态。
    #
    # （注意与 Go 不同：Python 的 channel 是惰性的、建的时候不连，所以这里防的不是
    # 「连不上」，而是「中台连得上我、我连不上中台」这种半通。）
    conn = _HubConnection(cfg)
    registrar = Registrar(conn, plugin, cfg, logger)

    listen_target = cfg.grpc_listen_target()
    # ⚠️ 绑定失败时 grpcio **抛 RuntimeError，不返回 0**。
    #
    # 这里原本写的是 `if bound_port == 0: 报错`——那是死代码，从来没执行过：端口被占
    # 时用户看到的是一段 grpcio 的 traceback（`Failed to bind to address …`），而 Go
    # 那侧会把它包成一句能照做的话。同一件事在两门语言里长成两个样子，是这类骨架最
    # 容易留下的裂缝。
    #
    # 实测（起一个占住端口的 socket，再让 grpcio 去绑同一个）：
    #   RuntimeError: Failed to bind to address 127.0.0.1:56711; ...
    try:
        bound_port = server.add_insecure_port(listen_target)
    except RuntimeError as err:
        conn.close()
        raise RuntimeError(
            f"hubkit: 监听 {cfg.listen_addr} 失败——端口可能已被占用，"
            f"检查 HUB_LISTEN_ADDR 是否与别的进程冲突（{err}）"
        ) from err
    # 返回值这一支仍然留着：0 是 grpcio 文档里「没绑上任何地址」的返回值。实测的这个
    # 版本里失败走的是上面那条异常，所以这一支大概率永远不执行——但删掉它省不了什么，
    # 而万一某个版本改成「返回 0 而不抛」，这里会拦住，而不是把一个没监听的插件当成
    # 起来了（那种故障的表现是注册探测失败，很难联想到监听）。
    if bound_port == 0:
        conn.close()
        raise RuntimeError(
            f"hubkit: 监听 {cfg.listen_addr} 失败——没有绑上任何地址，"
            "检查 HUB_LISTEN_ADDR 是否与别的进程冲突"
        )

    server.start()
    logger.info(
        "插件 gRPC 已监听", listen=listen_target, advertise=cfg.advertise_addr
    )

    registrar_thread = threading.Thread(
        target=registrar.loop, args=(stop,), name="hubkit-registrar", daemon=True
    )
    registrar_thread.start()

    logger.info("插件已启动", hub=cfg.hub_addr, instance=cfg.instance_id)

    try:
        # 不用 stop.wait()：那样 Ctrl-C 在有些终端里要按两次才停得下来
        # （第一次只唤起主线程）。轮询短间隔的代价可以忽略。
        while not stop.wait(0.2):
            pass
    finally:
        # 主动注销：中台据此立刻摘掉实例，不必等心跳超时（注册从没成功过时会整个
        # 跳过——那时候没有实例行可摘除，理由见 Registrar.unregister）
        registrar.unregister()
        stop.set()
        registrar_thread.join(timeout=2 * UNREGISTER_TIMEOUT + cfg.call_timeout)
        server.stop(grace=3.0)
        conn.close()
        logger.info("插件已退出")


def _install_signal_handlers(stop: threading.Event, logger: JsonLogger) -> None:
    """把 SIGINT / SIGTERM 接到 ``stop`` 上。

    只在主线程装得上（``signal.signal`` 的硬约束）；测试里从工作线程调 :func:`serve`
    时本函数不会被走到，所以不必额外判断。
    """
    handler = _SignalHandler(stop, logger)
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            signal.signal(sig, handler)
        except ValueError:
            # 非主线程。调用方自己管结束时机，这里不该把整个启动流程搞挂。
            pass


class _SignalHandler:
    """收到信号时置位并打一行日志。

    日志在**信号处理器里**打而不是等主循环醒过来再打：措辞是「收到退出信号」，
    等主循环的话一次程序化的 stop 也会打出这句话，而与事实不符。
    """

    def __init__(self, stop: threading.Event, logger: JsonLogger) -> None:
        self._stop = stop
        self._log = logger

    def __call__(self, signum: int, frame: Any) -> None:
        if not self._stop.is_set():
            self._log.info("收到退出信号，开始优雅退出", signal=signal.Signals(signum).name)
        self._stop.set()


__all__ = [
    "RegistrationRejected",
    "Registrar",
    "check_manifest",
    "reject_code_name",
    "run",
    "serve",
]
