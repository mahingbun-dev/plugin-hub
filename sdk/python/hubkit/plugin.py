"""插件接口：插件作者只需要实现这里定义的四件事。

与 ``sdk/go/hubkit/plugin.go`` 的 ``Plugin`` 接口一一对应，只是按 Python 的习惯
用下划线命名（``Manifest()`` → :meth:`Plugin.manifest`）。映射关系在下面每个方法的
文档里逐条写清楚了——两门语言的插件作者要读同一份接入指南，名字对不上的地方
必须能一眼查到。

第五个方法 :meth:`Plugin.set_state` 有缺省实现，**不需要实现**：它只在插件要用
外置状态（HubState）时才覆写，对应用 Go 侧的可选接口 ``hubkit.StateAware``。
第六个方法 :meth:`Plugin.set_gateway` 同理：只在插件要做**插件间发现与互调**时才
覆写，骨架在注册成功后把 :class:`hubkit.GatewayClient` 注入进来（时机与语义和
``set_state`` 完全一致）。

``health()`` 同理是**可选覆写**：基类里刻意没有这个名字，骨架用 ``getattr`` 探测它。
默认恒报健康是有代价的取舍，想自己探测依赖请先读过 ``hubkit/run.py`` 里
``_RuntimeService.Health`` 的说明再动手——那里的取舍写清楚了。
"""

from __future__ import annotations

from typing import Any

from google.protobuf import descriptor_pb2

from .envelope import valid
from .gateway import GatewayClient
from .proto.hubv1 import envelope_pb2, plugin_pb2
from .state import StateClient


class Plugin:
    """插件必须实现的接口。

    ``manifest`` 与 ``descriptor`` 是「我给中台什么」，``validate`` 与 ``handle``
    是「我干什么」——四者缺一不可：契约校验、编排连线、MCP 工具聚合都建立在它们之上。

    最小插件长这样::

        class MyPlugin(hubkit.Plugin):
            def manifest(self):
                return plugin_pb2.PluginManifest(
                    name="example", version="0.1.0",
                    consumes=[plugin_pb2.MessageContract(fq_name=hubkit.STRUCT_FQ_NAME)],
                )

            def validate(self, ctx, env):
                payload, is_json = hubkit.payload_json(env)
                if not is_json:
                    return hubkit.invalid(hubkit.issue("payload", "需要 JSON 对象载荷"))
                return hubkit.valid()

            def handle(self, ctx, env):
                return hubkit.with_payload_json(env, {"ok": True})

    插件被强制无状态：实例内存不保证跨调用保留（见 docs/design.md），
    需要跨调用保留的东西走中台的 HubState 接口。
    """

    def manifest(self) -> plugin_pb2.PluginManifest:
        """声明插件的身份、契约（消费/生产哪些消息类型）与暴露给 agent 的工具。

        同一版本号的 manifest 不可变更：中台会拒绝「同号不同契约」的注册，
        改了东西请升版本号。
        """
        raise NotImplementedError("插件必须实现 manifest()")

    def descriptor(self) -> bytes:
        """返回本插件的 FileDescriptorSet，中台据此建立契约基线并做字段级兼容检查。

        只用 ``google.protobuf.Struct`` 承载 JSON 载荷的插件没有自己的 proto，
        返回空即可——中台允许空 descriptor，只要 manifest 里没声明自有类型。

        有自有 proto 的插件用 :func:`descriptor_of` 从生成代码的 ``DESCRIPTOR`` 打包。
        """
        return b""

    def validate(
        self, ctx: Any, env: envelope_pb2.Envelope
    ) -> plugin_pb2.ValidateResponse:
        """数据校验规则。

        中台在把数据交给 :meth:`handle` 之前**一定**先调用它；返回
        ``valid=False`` 时链路短路，``handle`` 不会被调用。校验规则与插件体
        同版本发布，因此规则不可能与实现漂移。

        ``ctx`` 是本次调用的 gRPC ``ServicerContext``（与 Go 侧把
        ``context.Context`` 一路传下来是同一件事）。常用的两件事：``ctx.abort()``
        直接把这次调用判成失败、``ctx.time_remaining()`` 读剩余时间。类型标成
        ``Any`` 是因为测试里会直接调本方法、那时并没有真的 gRPC 上下文——传
        ``None`` 是常规做法，所以**别在 ``ctx`` 上无判空地取属性**。

        缺省实现是「一律放行」——比缺省实现抛 NotImplementedError 更安全：
        没写校验器的插件会照常工作，而不是在第一次真实调用时才炸。
        """
        return valid()

    def handle(self, ctx: Any, env: envelope_pb2.Envelope) -> envelope_pb2.Envelope:
        """插件体：自主实现的数据输入输出。

        输入信封的载荷用 :func:`hubkit.payload_json` 取（直接调用场景），
        输出用 :func:`hubkit.with_payload_json` 或直接设置 ``envelope.payload``。

        ``ctx`` 的含义与 :meth:`validate` 里那份说明完全相同：gRPC 的
        ``ServicerContext``，直接调用本方法时可以传 ``None``。

        **必须返回一个信封**：返回 ``None`` 会被中台当成插件异常，
        而不是「这次没有输出」。
        """
        raise NotImplementedError("插件必须实现 handle()")

    def set_state(self, state: StateClient) -> None:
        """收到骨架注入的 HubState 客户端（缺省什么都不做）。

        这是 Go 侧 ``hubkit.StateAware`` 接口在 Python 里的对应形态：那边必须用可选
        接口，是因为给 ``Plugin`` 接口加方法会让所有老插件编译不过；Python 这边基类
        可以直接给一个**空实现**，于是「老插件一行不用改、新插件想用才覆写」这个语义
        一样成立，还省掉了 ``isinstance`` 探测。

        调用时机与语义（与 Go 逐条对齐）：

        * **注册成功后**由骨架调用，且**每次重新注册后都会再调一次**（中台重启、
          实例被摘除后自愈、凭证被中台拒绝后重注册）。因此不要在这里假设"只调一次"，
          也不要把客户端存下来就不管了——重新注入的**是同一个对象**，凭证在它内部
          轮换。
        * 骨架从**注册循环那个线程**调用它，而 ``handle`` 跑在 gRPC 的工作线程上，
          所以**同步是实现方的责任**：用互斥锁护住那个字段，或者每次用的时候从
          线程安全的存储里取。裸赋值给一个会被其它线程读取的字段在 CPython 下不会
          崩，但拿到的可能是半成品状态（比如刚构造的插件还没填完字段）。
        * ``state_token`` 为空时（迁移前登记的旧实例）也会注入：客户端在，但调用
          必然被回 ``UNAUTHENTICATED``。骨架会在日志里给出提示。

        最小用法::

            class MyPlugin(hubkit.Plugin):
                def __init__(self):
                    self._state = None
                    self._lock = threading.Lock()

                def set_state(self, state):
                    with self._lock:
                        self._state = state

                def handle(self, ctx, env):
                    with self._lock:
                        state = self._state
                    if state is None:
                        return env          # 还没注册上，按 fail-open 处理
                    cached = state.get("orders", "SO-1")
                    ...
        """
        return None

    def set_gateway(self, gateway: GatewayClient) -> None:
        """收到骨架注入的网关客户端（缺省什么都不做）。

        用它做**插件间发现与互调**（谁在线、别人吃什么吐什么、同步调用下游插件），
        详见 :mod:`hubkit.gateway`。注入时机与同步责任同 :meth:`set_state`：

        * 注册成功后调用，**每次重新注册后都会再调一次**（注入的是同一个对象，
          凭证在它内部轮换）；
        * 从注册循环线程调用，``handle`` 跑在 gRPC 工作线程上，**同步是实现方的责任**；
        * 没注册上之前它手里的凭证是空的，调用必然被回 ``UNAUTHENTICATED``——
          按 fail-open 处理（与上面 ``set_state`` 例子里对 ``state is None`` 的
          处置一致）。

        最小用法::

            class MyPlugin(hubkit.Plugin):
                def set_gateway(self, gateway):
                    self._gateway = gateway   # 见 set_state：并发场景要加锁

                def handle(self, ctx, env):
                    payload, _ = hubkit.payload_json(env)
                    # 互调要把「我正在处理的信封」带上，trace 才能贯通、链才能防环
                    downstream = self._gateway.invoke_plugin(
                        "other-plugin", {"text": payload["text"]},
                        current_envelope=env,
                    )
                    return hubkit.with_payload_json(env, {"reply": ...})
        """
        return None


def descriptor_of(*file_descriptors: Any) -> bytes:
    """把生成代码里的 proto 文件描述符打包成 FileDescriptorSet。

    中台要的是编译产物而不是源码——它需要拿 descriptor 做字段级兼容检查，
    而不是去解析 .proto 文本。

    用法（自己的 proto 生成出的模块里）::

        from myplugin.proto.v1 import order_pb2

        def descriptor(self):
            return hubkit.descriptor_of(order_pb2.DESCRIPTOR)

    只打包列出的文件，不含它们的 import：中台侧只关心本插件自己定义的消息，
    跨文件引用的类型名仍会原样出现在字段描述里，兼容性判断不受影响。
    """
    file_set = descriptor_pb2.FileDescriptorSet()
    for descriptor in file_descriptors:
        if descriptor is None:
            continue
        proto = file_set.file.add()
        descriptor.CopyToProto(proto)
    return file_set.SerializeToString()
