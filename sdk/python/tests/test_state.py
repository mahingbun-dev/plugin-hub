"""HubState 客户端：读写往返、凭证、本地预检、被拒时的反应。

对着 ``fake_hub`` 跑（不是对着 mockhub——它按设计没有状态面），走的是**真实的 gRPC
往返**：metadata、状态码、deadline 都是真的，只有中台那一侧是替身。

与 Go 的 ``sdk/go/hubkit/state_test.go`` 覆盖同一批行为；额外的部分是**本地预检**
（Go 没有，Python 侧刻意加的）与**超时不该触发重新注册**这一条。
"""

from __future__ import annotations

import threading

import grpc
import pytest
from fake_hub import FakeHub, Options

from hubkit.proto.hubv1 import envelope_pb2, registry_pb2, registry_pb2_grpc
from hubkit.state import (
    MAX_SCAN_LIMIT,
    MAX_VALUE_BYTES,
    PublishReceipt,
    STATE_TOKEN_METADATA,
    DenialSignal,
    StateClient,
)

PLUGIN_NAME = "fake-plugin"


def _channel(hub: FakeHub) -> grpc.Channel:
    return grpc.insecure_channel(hub.addr.removeprefix("http://"))


def _register(channel: grpc.Channel, hub: FakeHub, plugin_name: str = PLUGIN_NAME) -> str:
    """走一次真注册，返回中台下发的凭证（状态面的身份就是这么来的）。"""
    stub = registry_pb2_grpc.PluginRegistryStub(channel)
    response = stub.Register(
        registry_pb2.RegisterRequest(
            plugin_name=plugin_name,
            version="1.0.0",
            instance_id="fake-instance",
            advertise_addr="http://127.0.0.1:1",
        )
    )
    assert response.accepted
    return response.state_token


@pytest.fixture()
def hub():
    with FakeHub() as hub:
        yield hub


@pytest.fixture()
def client(hub):
    """已拿到凭证的客户端——等价于「注册成功之后」的状态。"""
    channel = _channel(hub)
    try:
        client = StateClient(channel, denied=DenialSignal())
        client.set_token(_register(channel, hub))
        yield client
    finally:
        channel.close()


@pytest.fixture()
def registered(hub):
    """(channel, client) 对，供需要自己造客户端（换超时/换信号）的用例复用。"""
    channel = _channel(hub)
    try:
        yield channel, _register(channel, hub)
    finally:
        channel.close()


# ---------------------------------------------------------------- 读写往返


def test_读写往返(client):
    assert client.get("s", "k1") is None  # 还没写

    client.put("s", "k1", b"v1")
    assert client.get("s", "k1") == b"v1"

    assert client.delete("s", "k1") is True
    assert client.get("s", "k1") is None
    # 删不存在的键不是错误，与 Go 一致
    assert client.delete("s", "k1") is False


def test_空值与不存在的键是两回事(client):
    """``found`` 与「值是空字节」不是一回事：Go 用两个返回值表达，Python 用 None。"""
    client.put("s", "empty", b"")
    assert client.get("s", "empty") == b""
    assert client.get("s", "missing") is None


def test_字符串值按utf8编码(client):
    client.put("s", "k", "值")
    assert client.get("s", "k") == "值".encode("utf-8")


def test_扫描按前缀与上限(client):
    client.put("scan", "p1", b"v1")
    client.put("scan", "p2", b"v2")
    client.put("scan", "other", b"v3")

    entries = client.scan("scan", "p", 10)
    assert [(e.key, e.value) for e in entries] == [("p1", b"v1"), ("p2", b"v2")]

    assert len(client.scan("scan", "p", 1)) == 1  # 上限生效
    assert len(client.scan("scan")) == 3  # 空 prefix = 扫整个命名空间
    assert client.scan("other") == []  # 命名空间之间互不串门


def test_删除后扫描不再返回(client):
    client.put("scan", "p1", b"v1")
    client.delete("scan", "p1")
    assert client.scan("scan", "p") == []


# ---------------------------------------------------------------- 凭证


def test_凭证带在metadata上(client, hub):
    client.put("s", "k", b"v")
    client.get("s", "k")
    client.scan("s")

    assert hub.state_calls, "状态请求没有到达中台"
    assert {call.token for call in hub.state_calls} == {hub.opts.state_token}


def test_metadata键名与中台一致():
    """写错这个键中台会判成「无凭证」，而插件侧只看到一个 401——所以钉死它。"""
    assert STATE_TOKEN_METADATA == "x-hub-state-token"


def test_前缀由中台拼客户端不拼(client, hub):
    """插件自报的 namespace 只是子空间，前缀取自凭证反查出的插件名。"""
    client.put("ns", "k", b"v")
    assert hub.state_calls[-1].namespace == "ns"
    assert list(hub.store_snapshot()) == [f"hub:state:{PLUGIN_NAME}:ns:k"]


def test_换凭证之后旧的就用不了了(hub, registered):
    """中台每次注册都换凭证：注册循环换了新的，客户端必须用新的。"""
    channel, token = registered
    client = StateClient(channel, denied=DenialSignal())

    client.set_token("stale-token")
    with pytest.raises(grpc.RpcError) as err:
        client.get("s", "k")
    assert err.value.code() == grpc.StatusCode.UNAUTHENTICATED

    client.set_token(token)
    assert client.get("s", "k") is None  # 换上新凭证就通了


def test_空凭证被中台判成无凭证(hub, registered):
    """凭证为空时（迁移前登记的旧实例）调用必然 401——插件侧要能看清原因。"""
    channel, _token = registered
    client = StateClient(channel, denied=DenialSignal())

    with pytest.raises(grpc.RpcError) as err:
        client.get("s", "k")
    assert err.value.code() == grpc.StatusCode.UNAUTHENTICATED


# ---------------------------------------------------------------- 本地预检


def _bad_segments():
    return ["", "a b", "a:b", "a*", "a/b", "a@b", "中文", "a中", "a\u0000b", "a" * 201, " "]


@pytest.mark.parametrize("bad", _bad_segments())
def test_非法键名在本地就被挡住(client, hub, bad):
    """中台也会拒，但只有一句 INVALID_ARGUMENT；本地拦下来能直接指到调用点。"""
    before = len(hub.state_calls)

    calls = [
        lambda: client.get(bad, "k"),
        lambda: client.get("s", bad),
        lambda: client.put(bad, "k", b"v"),
        lambda: client.delete("s", bad),
        lambda: client.scan(bad),
    ]
    if bad:  # 空的 prefix 是**合法**的（扫整个命名空间），只验非空时的不合法
        calls.append(lambda: client.scan("s", bad))

    for call in calls:
        with pytest.raises(ValueError) as err:
            call()
        assert "hubkit" in str(err.value)

    assert len(hub.state_calls) == before, "本地预检不该产生任何往返"


def test_两百字节的键名放行(client):
    """契约文件里那两条 case：200 字节放行、201 拒绝。"""
    segment = "a" * 199 + "-"
    client.put("s", segment, b"v")
    assert client.get("s", segment) == b"v"


def test_值超限在本地就被挡住(client, hub):
    before = len(hub.state_calls)

    with pytest.raises(ValueError) as err:
        client.put("s", "k", b"x" * (MAX_VALUE_BYTES + 1))
    assert str(MAX_VALUE_BYTES) in str(err.value)

    assert len(hub.state_calls) == before


def test_值正好等于上限可以写(client):
    client.put("s", "big", b"x" * MAX_VALUE_BYTES)
    assert len(client.get("s", "big")) == MAX_VALUE_BYTES


def test_上限只有一个来源():
    """这两个数取自 crates/hub-grpc/src/state.rs，不能在这里悄悄改小/改大。"""
    assert MAX_VALUE_BYTES == 1024 * 1024
    assert MAX_SCAN_LIMIT == 1000


@pytest.mark.parametrize("limit", [0, -1, MAX_SCAN_LIMIT + 1, 10**9])
def test_非法的scan上限在本地就被挡住(client, hub, limit):
    before = len(hub.state_calls)

    with pytest.raises(ValueError) as err:
        client.scan("s", "p", limit)
    assert "limit" in str(err.value)

    assert len(hub.state_calls) == before


@pytest.mark.parametrize("limit", [1, MAX_SCAN_LIMIT])
def test_合法的scan上限能过(client, hub, limit):
    assert client.scan("s", "", limit) == []
    assert hub.state_calls[-1].limit == limit


@pytest.mark.parametrize("ttl", [-1, -60])
def test_负ttl被挡住(client, hub, ttl):
    before = len(hub.state_calls)
    with pytest.raises(ValueError):
        client.put("s", "k", b"v", ttl_seconds=ttl)
    assert len(hub.state_calls) == before


@pytest.mark.parametrize("ttl", [0.5, 1.5, 2.7])
def test_不足一秒的ttl被挡住而不是静默变成永不过期(client, ttl):
    """Go 会把 0.5s 截断成 0，也就是**永不过期**——与「很快消失」正好相反。

    Python 侧直接报错：这种错要很久以后才看得出来，让它出现在调用点上。
    """
    with pytest.raises(ValueError) as err:
        client.put("s", "k", b"v", ttl_seconds=ttl)
    assert "整数秒" in str(err.value)


def test_ttl按秒传给中台(client, hub):
    client.put("s", "k", b"v", ttl_seconds=60)
    assert hub.state_calls[-1].ttl_seconds == 60

    client.put("s", "k", b"v")  # 缺省 0 = 不过期
    assert hub.state_calls[-1].ttl_seconds == 0


def test_ttl到期后读不到(client):
    client.put("s", "k", b"v", ttl_seconds=1)
    assert client.get("s", "k") == b"v"


# ---------------------------------------------------------------- Publish


def test_publish受理并带回执(client, hub):
    receipt = client.publish("orders-flow", envelope_pb2.Envelope(message_id="m-1"))

    assert receipt.accepted is True
    assert receipt.run_id, "受理回执要带执行 id（后续查链路用）"
    assert receipt.reason == ""
    call = hub.state_calls[-1]
    assert call.method == "Publish"
    assert call.target == "orders-flow"
    assert call.token == hub.opts.state_token, "Publish 与读写一样凭状态凭证认证"


def test_publish被拒时reason原样暴露():
    """accepted=False 不是异常：防环/配额拒绝是调用方该分支处理的业务结果，
    reason 必须原样到达调用方（而不是被吞进一个空回执里）。"""
    with FakeHub(Options(publish_reject_reason="检测到投递环")) as hub:
        channel = _channel(hub)
        try:
            client = StateClient(channel)
            client.set_token(_register(channel, hub))
            receipt = client.publish("loop", envelope_pb2.Envelope(message_id="m-1"))

            assert receipt.accepted is False
            assert receipt.run_id == ""
            assert receipt.reason == "检测到投递环"
        finally:
            channel.close()


# ---------------------------------------------------------------- 被拒时的反应


def test_被拒时把Unauthenticated交给调用方并置位信号():
    """401 必须原样抛给插件（不吞、不改码），同时**叫醒注册循环**换新凭证。"""
    with FakeHub(Options(deny_state=True)) as hub:
        denied = DenialSignal()
        channel = _channel(hub)
        try:
            client = StateClient(channel, denied=denied)
            client.set_token(_register(channel, hub))

            with pytest.raises(grpc.RpcError) as err:
                client.get("s", "k")

            assert err.value.code() == grpc.StatusCode.UNAUTHENTICATED
            assert err.value.details()  # 中台给的原因不能被丢掉
            assert denied.wait(0), "撞上 401 后必须置位 denied 信号"
            assert denied.consume() is True
            assert denied.pending is False
        finally:
            channel.close()


def test_其它错误不置位信号():
    """INVALID_ARGUMENT / INTERNAL 与凭证无关，重注册解决不了——置位只会让循环空转。"""
    for code in (grpc.StatusCode.INVALID_ARGUMENT, grpc.StatusCode.INTERNAL):
        with FakeHub(Options(state_error=code)) as hub:
            denied = DenialSignal()
            channel = _channel(hub)
            try:
                client = StateClient(channel, denied=denied)
                client.set_token(hub.opts.state_token)

                with pytest.raises(grpc.RpcError) as err:
                    client.get("s", "k")
                assert err.value.code() == code
                assert not denied.pending, f"{code} 不该触发重新注册"
            finally:
                channel.close()


def test_超时是DeadlineExceeded且不触发重新注册():
    """超时与凭证无关：中台的一次卡顿不该把注册循环叫醒。"""
    with FakeHub(Options(state_delay=1.0)) as hub:
        denied = DenialSignal()
        channel = _channel(hub)
        try:
            client = StateClient(channel, call_timeout=0.2, denied=denied)
            client.set_token(hub.opts.state_token)

            with pytest.raises(grpc.RpcError) as err:
                client.get("s", "k")
            assert err.value.code() == grpc.StatusCode.DEADLINE_EXCEEDED
            assert not denied.pending
        finally:
            channel.close()


def test_没有信号出口时不炸():
    """测试里自造的客户端可以不给 denied（对应 Go 的 denied 为 nil 时静默丢弃）。"""
    with FakeHub(Options(deny_state=True)) as hub:
        channel = _channel(hub)
        try:
            client = StateClient(channel)
            client.set_token(hub.opts.state_token)
            with pytest.raises(grpc.RpcError):
                client.get("s", "k")
        finally:
            channel.close()


# ---------------------------------------------------------------- 信号本身


def test_信号合并成一次():
    """并发调用一起撞上 401 时只留一个信号（等价于缓冲 1 的 channel）。"""
    signal = DenialSignal()
    for _ in range(100):
        signal.notify()
    assert signal.wait(0)
    assert signal.consume() is True
    assert signal.consume() is False


def test_信号能跨线程唤醒():
    signal = DenialSignal()
    threading.Timer(0.05, signal.notify).start()
    assert signal.wait(2.0) is True


def test_信号等到超时就返回False():
    signal = DenialSignal()
    assert signal.wait(0.05) is False


def test_凭证读写是线程安全的():
    """注册循环在另一个线程上换凭证，调用方在这边读——不能读到半个字符串。"""
    client = StateClient(_NullChannel())
    assert client.token == ""

    stop = threading.Event()

    def rotate(prefix):
        n = 0
        while not stop.is_set():
            n += 1
            client.set_token(f"{prefix}-{n}")

    threads = [threading.Thread(target=rotate, args=(prefix,)) for prefix in ("a", "b")]
    for thread in threads:
        thread.start()
    try:
        for _ in range(2000):
            token = client.token
            assert token == "" or token.split("-")[1].isdigit()
    finally:
        stop.set()
        for thread in threads:
            thread.join(timeout=2)


class _NullChannel:
    """只为构造 StateClient 用的空通道——上面那个用例只碰凭证，不发 RPC。"""

    def unary_unary(self, *args, **kwargs):
        def _never(*_a, **_kw):  # pragma: no cover - 不该被调用
            raise AssertionError("本用例不该发 RPC")

        return _never


# ---------------------------------------------------------------- 导出面


def test_导出的名字():
    import hubkit

    for name in (
        "StateClient",
        "StateEntry",
        "DenialSignal",
        "STATE_TOKEN_METADATA",
        "MAX_SCAN_LIMIT",
        "MAX_VALUE_BYTES",
        "STATE_CALL_TIMEOUT",
        "PublishReceipt",
        "valid_state_segment",
    ):
        assert name in hubkit.__all__, f"{name} 必须出现在 hubkit.__all__ 里"
        assert getattr(hubkit, name) is not None
