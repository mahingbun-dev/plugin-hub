"""GatewayClient：发现与互调的往返、凭证、信封整备、业务结果到异常的映射。

对着 mockhub 的网关替身跑（真实 gRPC 往返：metadata、状态码都是真的，只有中台
那一侧是替身）。互调的治理行为（链校验、配额、subject 覆盖、deadline 夹紧）发生
在真中台，mock 不复刻——这里验的是 SDK 这一半：信封怎么构造、链怎么原样复制、
业务结果怎么翻成类型化异常、凭证怎么带上去。

与 Go 侧未来的 gateway 测试覆盖同一批行为；当前 Go 门尚未实现，本文件是
``crates/hub-grpc/tests/gateway.rs``（中台侧集成测试）在插件侧的对应面。
"""

from __future__ import annotations

import time

import grpc
import pytest
from hubkit import mockhub
from hubkit.envelope import new_ulid, payload_json, with_payload_json
from hubkit.gateway import (
    CALL_CHAIN_META,
    DEFAULT_INVOKE_TIMEOUT_MS,
    MAX_INVOKE_DEPTH,
    GatewayClient,
    InvokeFailed,
    InvokeFailure,
    InvokeRejected,
)
from hubkit.mockhub import MOCK_STATE_TOKEN
from hubkit.proto.hubv1 import (
    envelope_pb2,
    gateway_pb2,
    plugin_pb2,
    registry_pb2,
    registry_pb2_grpc,
)
from hubkit.state import STATE_TOKEN_METADATA

# ULID 的字符表。与 hubkit.envelope._ULID_ENCODING 同值——这里**故意再写一遍**：
# 测试替身要是复用了被测实现的常量，字符表写错了会两边一起错，测了等于没测
# （同 fake_hub 里独立写键名正则的理由）。
_ULID_CHARS = set("0123456789ABCDEFGHJKMNPQRSTVWXYZ")

CALLEE = "ping-callee"


def _is_ulid(text: str) -> bool:
    return len(text) == 26 and set(text) <= _ULID_CHARS


def _echo_handler(request: gateway_pb2.InvokeRequest) -> gateway_pb2.InvokeResponse:
    """最简单的下游替身：把收到的 JSON 载荷回显出去（在下游信封上回）。"""
    payload, is_json = payload_json(request.envelope)
    text = (payload or {}).get("text", "") if is_json else ""
    out = with_payload_json(request.envelope, {"echo": text})
    return gateway_pb2.InvokeResponse(outcome=gateway_pb2.HANDLED, envelope=out)


def _register(channel: grpc.Channel, name: str, version: str = "1.0.0", **manifest_kw):
    """走一次真注册，返回中台下发的凭证。manifest 可按需带（发现类用例要它）。"""
    manifest_kw.setdefault("name", name)
    manifest_kw.setdefault("version", version)
    stub = registry_pb2_grpc.PluginRegistryStub(channel)
    response = stub.Register(
        registry_pb2.RegisterRequest(
            plugin_name=name,
            version=version,
            instance_id=f"i-{name}",
            advertise_addr="http://127.0.0.1:1",
            manifest=plugin_pb2.PluginManifest(**manifest_kw),
        )
    )
    assert response.accepted
    return response.state_token


@pytest.fixture()
def hub():
    with mockhub.Hub(mockhub.Options(invoke_handler=_echo_handler)) as hub:
        yield hub


@pytest.fixture()
def client(hub):
    channel = grpc.insecure_channel(hub.addr.removeprefix("http://"))
    try:
        client = GatewayClient(channel)
        client.set_token(MOCK_STATE_TOKEN)
        yield client
    finally:
        channel.close()


def _invoke_request_of(hub: mockhub.Hub) -> gateway_pb2.InvokeRequest:
    invokes = [call.request for call in hub.gateway_calls if call.kind == "Invoke"]
    assert invokes, "互调请求没有到达中台"
    return invokes[-1]


# ---------------------------------------------------------------- 凭证


def test_网关请求都带状态凭证(client, hub):
    client.list_plugins()
    client.describe_message("a.v1.A")
    client.invoke_plugin(CALLEE, {"text": "hi"})

    assert hub.gateway_calls, "网关请求没有到达中台"
    assert {call.token for call in hub.gateway_calls} == {MOCK_STATE_TOKEN}
    # 键名的事实来源与状态面是同一个：同一个凭证、同一个 metadata 键
    assert STATE_TOKEN_METADATA == "x-hub-state-token"


def test_坏凭证被判Unauthenticated且被记录(hub):
    channel = grpc.insecure_channel(hub.addr.removeprefix("http://"))
    try:
        client = GatewayClient(channel)
        client.set_token("stale-token")
        with pytest.raises(grpc.RpcError) as err:
            client.list_plugins()
        assert err.value.code() == grpc.StatusCode.UNAUTHENTICATED
        # 被拒的请求也算「收到过」——排查时能确认客户端确实发了、发的是什么凭证
        assert [call.token for call in hub.gateway_calls] == ["stale-token"]
    finally:
        channel.close()


def test_空凭证同样被拒(client, hub):
    """凭证为空 = 中台还没下发（注册没成功过），调用必然 401。"""
    channel = grpc.insecure_channel(hub.addr.removeprefix("http://"))
    try:
        client = GatewayClient(channel)  # 不 set_token
        with pytest.raises(grpc.RpcError) as err:
            client.describe_message("a.v1.A")
        assert err.value.code() == grpc.StatusCode.UNAUTHENTICATED
    finally:
        channel.close()


def test_401置位denied信号(hub):
    """网关面撞上 401 与状态面撞上 401 是同一件事（同一张凭证），共用同一个信号。"""
    from hubkit import DenialSignal

    channel = grpc.insecure_channel(hub.addr.removeprefix("http://"))
    try:
        denied = DenialSignal()
        client = GatewayClient(channel, denied=denied)
        client.set_token("stale-token")
        with pytest.raises(grpc.RpcError):
            client.list_plugins()
        assert denied.wait(0), "网关面的 401 也必须叫醒注册循环"
    finally:
        channel.close()


# ---------------------------------------------------------------- 发现三件套


def test_发现三件套往返(hub, client):
    channel = grpc.insecure_channel(hub.addr.removeprefix("http://"))
    try:
        _register(
            channel,
            CALLEE,
            description="回声插件",
            produces=[plugin_pb2.MessageContract(fq_name="a.v1.Echoed")],
            consumes=[plugin_pb2.MessageContract(fq_name="google.protobuf.Struct")],
            invokes=["other"],
            tools=[plugin_pb2.ToolDecl(name="echo", input_schema_json='{"type":"object"}')],
        )
        _register(channel, "offline-plugin")  # 注册过但实例已被注销（见下）

        summary = {p.name: p for p in client.list_plugins()}
        assert set(summary) == {CALLEE, "offline-plugin"}
        assert summary[CALLEE].online is True
        assert summary[CALLEE].instance_count == 1
        assert summary[CALLEE].latest_version == "1.0.0"
        assert summary[CALLEE].description == "回声插件"

        # 注销掉 offline-plugin 的实例行（mock 的注销凭凭证认属主），
        # 缺省的清单就只剩在线插件
        stub = registry_pb2_grpc.PluginRegistryStub(channel)
        stub.Unregister(
            registry_pb2.UnregisterRequest(
                instance_id="i-offline-plugin", state_token=MOCK_STATE_TOKEN
            )
        )
        assert [p.name for p in client.list_plugins()] == [CALLEE]
        assert len(client.list_plugins(include_offline=True)) == 2

        described = client.describe_message("a.v1.Echoed")
        # mock 没有契约反查索引（见 mockhub 的说明），空答案即「到达且诚实」
        assert list(described.producers) == []
        assert list(described.consumers) == []

        contract = client.get_contract(CALLEE)
        assert contract.name == CALLEE
        assert contract.version == "1.0.0"
        assert [c.fq_name for c in contract.produces] == ["a.v1.Echoed"]
        assert [c.fq_name for c in contract.consumes] == ["google.protobuf.Struct"]
        assert list(contract.invokes) == ["other"]
        assert [t.name for t in contract.tools] == ["echo"]

        with pytest.raises(grpc.RpcError) as err:
            client.get_contract(CALLEE, version="9.9.9")
        assert err.value.code() == grpc.StatusCode.NOT_FOUND
    finally:
        channel.close()


# ---------------------------------------------------------------- invoke_plugin


def test_顶层发起_新trace不带链(client, hub):
    before = time.time_ns() // 1_000_000
    out = client.invoke_plugin(CALLEE, {"text": "你好"})
    after = time.time_ns() // 1_000_000

    payload, is_json = payload_json(out)
    assert is_json and payload == {"echo": "你好"}, "返回的是下游的处理结果信封"

    sent = _invoke_request_of(hub).envelope
    assert _is_ulid(sent.message_id) and _is_ulid(sent.trace_id)
    assert sent.message_id != sent.trace_id, "两个 id 各自独立生成"
    assert sent.type == envelope_pb2.PAYLOAD_TYPE_REQUEST
    assert CALL_CHAIN_META not in sent.meta, "顶层发起没有链可继承，不该凭空带一个"
    # deadline 用与中台一致的缺省预算起算；允许生成耗时带来的小偏移
    assert before + DEFAULT_INVOKE_TIMEOUT_MS <= sent.deadline_ms <= after + DEFAULT_INVOKE_TIMEOUT_MS
    assert _invoke_request_of(hub).timeout_ms == DEFAULT_INVOKE_TIMEOUT_MS
    payload_sent, is_json_sent = payload_json(sent)
    assert is_json_sent and payload_sent == {"text": "你好"}


def test_带上当前信封_复制trace与链且原样(client, hub):
    current = envelope_pb2.Envelope(
        message_id="old-message", trace_id="t-current", run_id="r-current", node_id="n-current"
    )
    # 链里**故意放上调用方自己的名字**：客户端要是自作主张追加自己，这里立刻露馅；
    # 原样复制才是对的——追加 caller 是中台在代调时做的事
    current.meta[CALL_CHAIN_META] = "fake-caller,ping-callee"

    client.invoke_plugin(CALLEE, {"text": "hi"}, current_envelope=current)

    sent = _invoke_request_of(hub).envelope
    assert sent.trace_id == "t-current", "trace 要贯通到下游"
    assert sent.run_id == "r-current"
    assert sent.node_id == "n-current"
    assert sent.meta[CALL_CHAIN_META] == "fake-caller,ping-callee", "链必须原样、不追加自己"
    assert sent.message_id != "old-message", "数据幂等键是新调用新的，不沿用上游的"


def test_当前信封没有链时不带链(client, hub):
    current = envelope_pb2.Envelope(trace_id="t-1", run_id="r-1", node_id="n-1")
    client.invoke_plugin(CALLEE, {"text": "hi"}, current_envelope=current)
    assert CALL_CHAIN_META not in _invoke_request_of(hub).envelope.meta


def test_超时预算原样下发(client, hub):
    before = time.time_ns() // 1_000_000
    client.invoke_plugin(CALLEE, {"text": "hi"}, timeout_ms=5_000)
    sent = _invoke_request_of(hub)
    assert sent.timeout_ms == 5_000
    assert before + 5_000 <= sent.envelope.deadline_ms <= before + 5_000 + 50


def test_整体预算更早时以其为准(client, hub):
    # 父信封的剩余预算比本次预算更紧：deadline 取较早者，不能让下游活得比父处理更久
    # （与其余四门 SDK 同语义）
    now = time.time_ns() // 1_000_000
    current = envelope_pb2.Envelope(
        message_id="m", trace_id="t", run_id="r", node_id="n", deadline_ms=now + 1_200
    )
    client.invoke_plugin(CALLEE, {"text": "hi"}, timeout_ms=60_000, current_envelope=current)
    assert _invoke_request_of(hub).envelope.deadline_ms == current.deadline_ms


def test_父信封没有deadline时按本次预算起算(client, hub):
    # deadline_ms=0 表示父信封没带预算：不能拿 0 当「最早的期限」用
    before = time.time_ns() // 1_000_000
    current = envelope_pb2.Envelope(message_id="m", trace_id="t", run_id="r", node_id="n")
    client.invoke_plugin(CALLEE, {"text": "hi"}, timeout_ms=5_000, current_envelope=current)
    assert before + 5_000 <= _invoke_request_of(hub).envelope.deadline_ms <= before + 5_000 + 100


def test_REJECTED映射为InvokeRejected且issues携带(client, hub):
    issue = plugin_pb2.ValidationIssue(
        path="payload.text", message="不能为空", severity=plugin_pb2.SEVERITY_ERROR
    )

    def rejecting_handler(request):
        return gateway_pb2.InvokeResponse(
            outcome=gateway_pb2.REJECTED, issues=[issue], reason="目标插件校验未通过，见 issues"
        )

    hub.opts.invoke_handler = rejecting_handler
    with pytest.raises(InvokeRejected) as err:
        client.invoke_plugin(CALLEE, {"text": ""})

    exc = err.value
    assert isinstance(exc, InvokeFailure), "两类业务失败要能被同一个基类抓住"
    assert exc.outcome == "REJECTED"
    assert exc.reason == "目标插件校验未通过，见 issues"
    assert isinstance(exc.issues, tuple) and len(exc.issues) == 1
    assert exc.issues[0].path == "payload.text"
    assert "payload.text" in str(exc), "异常消息要能直接读出是哪个字段的问题"


def test_ERROR映射为InvokeFailed且reason携带(client, hub):
    def failing_handler(request):
        return gateway_pb2.InvokeResponse(outcome=gateway_pb2.ERROR, reason="未声明对 other 的调用授权")

    hub.opts.invoke_handler = failing_handler
    with pytest.raises(InvokeFailed) as err:
        client.invoke_plugin("other", {"a": 1})

    exc = err.value
    assert exc.outcome == "ERROR"
    assert exc.reason == "未声明对 other 的调用授权"
    assert exc.issues == ()
    assert not isinstance(exc, InvokeRejected)


def test_未装配下游替身时mock回ERROR而不是假装成功():
    """mock 的缺省 Invoke 是 ERROR——「mock 不比真中台宽松」在网关面的落点。"""
    with mockhub.Hub() as hub:  # 没注入 invoke_handler
        channel = grpc.insecure_channel(hub.addr.removeprefix("http://"))
        try:
            client = GatewayClient(channel)
            client.set_token(MOCK_STATE_TOKEN)
            with pytest.raises(InvokeFailed) as err:
                client.invoke_plugin(CALLEE, {"text": "hi"})
            assert "mock" in err.value.reason, "reason 要把「差的是替身」说清楚"
        finally:
            channel.close()


# ---------------------------------------------------------------- 载荷形态


def test_三种载荷形态都能发(client, hub):
    client.invoke_plugin(CALLEE, {"text": "hi"})
    message_payload = plugin_pb2.MessageContract(fq_name="b.v1.B")
    client.invoke_plugin(CALLEE, message_payload)
    client.invoke_plugin(CALLEE, b"\x1a\x03abc", payload_type="hub.v1.MessageContract")

    invokes = [call.request for call in hub.gateway_calls if call.kind == "Invoke"]
    assert [r.envelope.payload.type_url for r in invokes] == [
        "type.googleapis.com/google.protobuf.Struct",
        "type.googleapis.com/hub.v1.MessageContract",
        "type.googleapis.com/hub.v1.MessageContract",
    ]
    assert invokes[2].envelope.payload.value == b"\x1a\x03abc"

    payload, is_json = payload_json(invokes[0].envelope)
    assert is_json and payload == {"text": "hi"}


def test_裸字节没给类型时在本地就被挡住(client, hub):
    with pytest.raises(ValueError) as err:
        client.invoke_plugin(CALLEE, b"raw")
    assert "payload_type" in str(err.value)
    assert hub.gateway_calls == [], "本地预检不该产生任何往返"


def test_不支持的载荷类型被挡住(client):
    with pytest.raises(ValueError):
        client.invoke_plugin(CALLEE, ["不是 dict 也不是 message"])
    with pytest.raises(ValueError):
        client.invoke_plugin(CALLEE, 123)


# ---------------------------------------------------------------- 常量与事实源


def test_网关常量与契约文件一致():
    """事实源是 ``sdk/go/hubkit/testdata/hub-rules.json`` 的 gateway 小节，
    Rust 侧 ``hub_grpc::gateway`` 的同名常量与之对齐。这里钉住不漂移。"""
    assert CALL_CHAIN_META == "hub.call_chain"
    assert MAX_INVOKE_DEPTH == 8
    assert DEFAULT_INVOKE_TIMEOUT_MS == 30_000


def test_ULID的形状与唯一性():
    ids = {new_ulid() for _ in range(1000)}
    assert len(ids) == 1000, "连发 1000 个不该撞（80 位随机数）"
    assert all(_is_ulid(value) for value in ids), "26 字符、Crockford Base32 字符表"


def test_ULID按时间有序():
    """ULID 的价值在「排序即时间序」：后生成的 id 不该排在早生成的前面。"""
    first = new_ulid()
    time.sleep(0.005)  # 跨过毫秒界，让时间戳部分必然前进
    second = new_ulid()
    assert first < second


# ---------------------------------------------------------------- 导出面


def test_导出的名字():
    import hubkit

    for name in (
        "GatewayClient",
        "InvokeFailure",
        "InvokeRejected",
        "InvokeFailed",
        "PublishReceipt",
        "CALL_CHAIN_META",
        "DEFAULT_INVOKE_TIMEOUT_MS",
        "GATEWAY_CALL_TIMEOUT",
        "new_ulid",
    ):
        assert name in hubkit.__all__, f"{name} 必须出现在 hubkit.__all__ 里"
        assert getattr(hubkit, name) is not None
