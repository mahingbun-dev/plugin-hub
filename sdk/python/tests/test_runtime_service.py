"""``hub.v1.PluginRuntime`` 五个方法的协议层行为。

这一层负责的是**协议层的形状**（缺信封怎么办、插件抛异常怎么变成状态码、流式方法
怎么应答），业务语义一概不碰。它最容易出的错是「把异常吞掉改成一个看似成功的响应」——
所以每个方法都要有一条「出错时确实报错」的用例。
"""

from __future__ import annotations

import threading

import grpc
import pytest
from echo_plugin import EchoPlugin
from hubkit import mockhub, serve
from hubkit.config import Config
from hubkit.log import JsonLogger
from hubkit.plugin import Plugin
from hubkit.proto.hubv1 import envelope_pb2, plugin_pb2, plugin_pb2_grpc


_UNSET = object()


class _Raising(Plugin):
    """plugin 的三条出错路径各来一个。

    ``validate`` / ``handle`` 的取值含义：``_UNSET`` 走父类缺省、``Exception`` 抛出去、
    其它值原样返回（**包括 None**——「返回 None」本身就是要验的一条）。
    """

    def __init__(self, *, validate=_UNSET, handle=_UNSET):
        self._validate = validate
        self._handle = handle

    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(
            name="raising",
            version="0.1.0",
            consumes=[plugin_pb2.MessageContract(fq_name="google.protobuf.Struct")],
        )

    def validate(self, ctx, env):
        if self._validate is _UNSET:
            return super().validate(ctx, env)
        if isinstance(self._validate, Exception):
            raise self._validate
        return self._validate

    def handle(self, ctx, env):
        if self._handle is _UNSET:
            return super().handle(ctx, env)
        if isinstance(self._handle, Exception):
            raise self._handle
        return self._handle


class _Recording(Plugin):
    """把每次调用收到的 ``ctx`` 记下来。

    用来验证骨架**真的**把 grpcio 那个 ``ServicerContext`` 传给了插件，而不是
    自己拼一个看着像的替身——后者一调 ``ctx.abort()`` 就会露馅。
    """

    def __init__(self):
        self.ctx_seen = []

    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(
            name="recording",
            version="0.1.0",
            consumes=[plugin_pb2.MessageContract(fq_name="google.protobuf.Struct")],
        )

    def validate(self, ctx, env):
        self.ctx_seen.append(ctx)
        return plugin_pb2.ValidateResponse(valid=True)

    def handle(self, ctx, env):
        self.ctx_seen.append(ctx)
        return env


class _HealthProbe(Plugin):
    """按构造参数决定 ``health()`` 的行为。

    ``health`` 是**可选覆写**：基类里没有这个名字，骨架靠 ``getattr`` 探测。
    这里三种行为各验一条：正常返回、抛异常、忘了 return。
    """

    def __init__(self, *, healthy=True, boom=None, return_none=False):
        self._healthy = healthy
        self._boom = boom
        self._return_none = return_none
        self.calls = 0

    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(
            name="health-probe",
            version="0.1.0",
            consumes=[plugin_pb2.MessageContract(fq_name="google.protobuf.Struct")],
        )

    def health(self):
        self.calls += 1
        if self._boom is not None:
            raise self._boom
        if self._return_none:
            return None
        return plugin_pb2.HealthResponse(healthy=self._healthy, message="插件自报")

    def handle(self, ctx, env):
        return env


class _Server:
    """起一个插件，并给出指向它的 stub。"""

    def __init__(self, plugin: Plugin):
        self.hub = mockhub.Hub()
        self.stop = threading.Event()
        self.addr = mockhub.free_addr()
        cfg = Config(
            hub_addr=self.hub.addr,
            advertise_addr=f"http://{self.addr}",
            listen_addr=self.addr,
            instance_id="runtime-svc",
            logger=JsonLogger(),
        )
        self.thread = threading.Thread(target=serve, args=(plugin, cfg, self.stop), daemon=True)

    def __enter__(self):
        self.thread.start()
        channel = grpc.insecure_channel(self.addr)
        grpc.channel_ready_future(channel).result(timeout=5)
        self._channel = channel
        return plugin_pb2_grpc.PluginRuntimeStub(channel)

    def __exit__(self, *exc_info):
        self._channel.close()
        self.stop.set()
        self.thread.join(timeout=10)
        self.hub.close()


def test_Describe给出manifest():
    with _Server(EchoPlugin()) as stub:
        manifest = stub.Describe(plugin_pb2.DescribeRequest(), timeout=5)
    assert manifest.name == "echo"
    assert manifest.version == "0.1.0"


def test_Health恒为健康():
    """注册期中台就会探测它，一次不健康会变成「注册被拒」。

    ``EchoPlugin`` 没有 ``health()``，走的正是缺省路径：**根本不问插件**。
    """
    with _Server(EchoPlugin()) as stub:
        response = stub.Health(plugin_pb2.HealthRequest(), timeout=5)
    assert response.healthy
    assert response.message == "ok"


def test_插件定义了health就用插件的结论():
    """覆写是「可选」的：基类里没有这个方法，骨架靠 getattr 探测。"""
    plugin = _HealthProbe(healthy=False)
    with _Server(plugin) as stub:
        response = stub.Health(plugin_pb2.HealthRequest(), timeout=5)

    assert plugin.calls == 1, "定义了 health() 就该被调用"
    assert not response.healthy
    assert response.message == "插件自报"


def test_health抛异常时报不健康而不是假装健康():
    """把「探测有 bug」吞成健康，等于让覆写 health() 的人白写——他要的就是不被骗。"""
    plugin = _HealthProbe(boom=RuntimeError("下游连不上"))
    with _Server(plugin) as stub:
        response = stub.Health(plugin_pb2.HealthRequest(), timeout=5)

    assert not response.healthy
    assert "下游连不上" in response.message


def test_health忘了return时报不健康并说清楚():
    """返回 None 若直接交给 grpcio，会变成一个没有 message 的 UNKNOWN。"""
    plugin = _HealthProbe(return_none=True)
    with _Server(plugin) as stub:
        response = stub.Health(plugin_pb2.HealthRequest(), timeout=5)

    assert not response.healthy
    assert "HealthResponse" in response.message


def test_骨架把grpc上下文原样传给插件():
    """``ctx`` 不是摆设：插件靠它 ``abort()`` 判失败、``time_remaining()`` 读预算。

    所以断言的是「确实是 grpcio 的上下文对象」，而不是「不是 None」——一个自制的
    替身也能过后者，但插件一调 ``abort`` 就会发现语义不对，那时已经到线上了。
    """
    plugin = _Recording()
    with _Server(plugin) as stub:
        stub.Validate(plugin_pb2.ValidateRequest(envelope=envelope_pb2.Envelope()), timeout=5)
        stub.Handle(plugin_pb2.HandleRequest(envelope=envelope_pb2.Envelope()), timeout=5)

    assert len(plugin.ctx_seen) == 2, "validate 与 handle 各该收到一次"
    for ctx in plugin.ctx_seen:
        assert isinstance(ctx, grpc.ServicerContext)
        assert callable(getattr(ctx, "abort", None))
        assert callable(getattr(ctx, "time_remaining", None))


def test_空信封不崩而是回一条可定位的问题():
    """空信封是合法请求（可达性探测就可能发一个）。这里抛异常会让探测判成不健康。"""
    with _Server(EchoPlugin()) as stub:
        response = stub.Validate(plugin_pb2.ValidateRequest(), timeout=5)

    assert not response.valid
    assert response.issues[0].path == "envelope"


def test_校验器抛异常变成INTERNAL():
    with _Server(_Raising(validate=RuntimeError("校验器内部炸了"))) as stub:
        with pytest.raises(grpc.RpcError) as err:
            stub.Validate(plugin_pb2.ValidateRequest(envelope=envelope_pb2.Envelope()), timeout=5)

    assert err.value.code() == grpc.StatusCode.INTERNAL
    assert "校验器执行失败" in err.value.details()


def test_插件体抛异常变成INTERNAL():
    """插件自己的错误必须原样上报——中台会把它归到「插件调用失败」，调用方据此重试。"""
    with _Server(_Raising(handle=RuntimeError("业务上不支持"))) as stub:
        with pytest.raises(grpc.RpcError) as err:
            stub.Handle(plugin_pb2.HandleRequest(envelope=envelope_pb2.Envelope()), timeout=5)

    assert err.value.code() == grpc.StatusCode.INTERNAL
    assert "业务上不支持" in err.value.details()


def test_Handle缺信封是INVALID_ARGUMENT():
    with _Server(EchoPlugin()) as stub:
        with pytest.raises(grpc.RpcError) as err:
            stub.Handle(plugin_pb2.HandleRequest(), timeout=5)
    assert err.value.code() == grpc.StatusCode.INVALID_ARGUMENT


def test_插件体返回None时是明确的INTERNAL():
    """返回 None 时若直接构造 HandleResponse 会抛 TypeError，落到 grpcio 那里变成
    一个没有 message 的 UNKNOWN——中台只看到「插件异常」，插件侧什么也没打出来。
    """
    with _Server(_Raising(handle=None)) as stub:
        with pytest.raises(grpc.RpcError) as err:
            stub.Handle(plugin_pb2.HandleRequest(envelope=envelope_pb2.Envelope()), timeout=5)

    assert err.value.code() == grpc.StatusCode.INTERNAL
    assert "空信封" in err.value.details()


def test_HandleStream明确返回UNIMPLEMENTED():
    """与 Go 侧一致地返回 UNIMPLEMENTED，而不是给个「能跑但语义不对」的假实现——
    中台侧会按错的前提去接线。
    """
    with _Server(EchoPlugin()) as stub:
        with pytest.raises(grpc.RpcError) as err:
            list(stub.HandleStream(plugin_pb2.HandleRequest(envelope=envelope_pb2.Envelope()), timeout=5))

    assert err.value.code() == grpc.StatusCode.UNIMPLEMENTED
    assert "流式处理尚未实现" in err.value.details()
