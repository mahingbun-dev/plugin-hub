"""跨语言规则判定，以及 mock 中台本身的行为。

mock 也要被测：它比真中台宽松的话，问题会推迟到上线才爆——而 mock 的「宽松」
往往表现为**它没记下该记的东西**，那种缺陷在没有测试时完全看不出来。
"""

from __future__ import annotations

import threading

import pytest
from echo_plugin import EchoPlugin
from hubkit import mockhub, serve, valid_plugin_name
from hubkit.config import Config
from hubkit.log import JsonLogger
from hubkit.proto.hubv1 import registry_pb2


# ---------------------------------------------------------------- 插件名规则


@pytest.mark.parametrize(
    "name",
    ["a", "A", "0", "order-reader", "order_reader", "Order-Reader-2", "a" * 64],
)
def test_合法插件名(name):
    """规则与中台的 ``crates/hub-registry/src/validate.rs`` 的 is_valid_plugin_name 等价。"""
    assert valid_plugin_name(name)


@pytest.mark.parametrize(
    "name",
    [
        "",
        "-abc",  # 首字符必须是字母或数字
        "_abc",
        "a.b",  # 点不在白名单里
        "a b",
        "中文名",  # 非 ASCII
        "a" * 65,  # 上限 64 字节
    ],
)
def test_非法插件名(name):
    assert not valid_plugin_name(name)


def test_长度按字节而不是字符():
    """中台用 ``String::len()``（字节）判长。

    「中文名」按字符算是 3、按字节算是 9，两边会给出不同的结论——
    而结论不同的表现是「本地自检过了、注册被拒」。
    """
    # 用 ASCII 边界确认字节语义：64 个 ASCII 字符恰好合法，65 个不合法
    assert valid_plugin_name("a" * 64)
    assert not valid_plugin_name("a" * 65)


# ---------------------------------------------------------------- mock 中台


def test_记录注册请求的全部字段():
    hub = mockhub.Hub()
    try:
        assert hub.last_registration is None

        # 借 Registrar 走一次真实注册，而不是手工构造请求——手工构造的话，
        # 「插件侧到底报了什么」这件事就没被验到
        stop = threading.Event()
        addr = mockhub.free_addr()
        cfg = Config(
            hub_addr=hub.addr,
            advertise_addr=f"http://{addr}",
            listen_addr=addr,
            instance_id="mock-test",
            logger=JsonLogger(),
        )
        thread = threading.Thread(target=serve, args=(EchoPlugin(), cfg, stop), daemon=True)
        thread.start()
        try:
            hub.wait_for_registration(1, timeout=5)
        finally:
            stop.set()
            thread.join(timeout=10)

        request = hub.last_registration
        assert request.plugin_name == "echo"
        assert request.version == "0.1.0"
        assert request.instance_id == "mock-test"
        assert request.advertise_addr == f"http://{addr}"
        assert request.manifest.name == "echo"
        # 只用 Struct 载荷的插件没有自己的 proto，descriptor 就是空
        assert request.descriptor_set == b""
    finally:
        hub.close()


def test_按拍数要求重新注册():
    """``reregister_after_beats`` 就是这条链路的开关，它自己要准。"""
    hub = mockhub.Hub(mockhub.Options(heartbeat_interval_seconds=1, reregister_after_beats=2))
    try:
        for expected in (1, 2):
            response = hub._on_heartbeat(registry_pb2.HeartbeatRequest(instance_id="i"))
            assert expected == hub.heartbeats
            assert response.accepted
            assert not response.reregister_required

        third = hub._on_heartbeat(registry_pb2.HeartbeatRequest(instance_id="i"))
        assert not third.accepted
        assert third.reregister_required
    finally:
        hub.close()


def test_拒绝注册时逐条回结构化原因():
    def reject(_request):
        return [
            registry_pb2.Rejection(
                code=registry_pb2.REJECT_CODE_INSTANCE_CONFLICT,
                message="instance_id 已被其它插件占用",
                detail="换个 instance_id，或复用同一个插件名下注册",
            )
        ]

    hub = mockhub.Hub(mockhub.Options(reject_register=reject))
    try:
        response = hub._on_register(registry_pb2.RegisterRequest(plugin_name="x", version="1"))
        assert not response.accepted
        assert response.rejections[0].code == registry_pb2.REJECT_CODE_INSTANCE_CONFLICT
        assert response.rejections[0].detail
        # 被拒的注册也要被记下来：CI 与开发者都要能机器可读地看到「为什么没进来」
        assert len(hub.registrations) == 1
    finally:
        hub.close()


def test_记录注销():
    """被拒的注销也要被记下来。

    真中台拒绝时返回的是**空响应**（``UnregisterResponse`` 没有字段），插件侧分不出
    成没成——所以「到底发过没有」只能看 mock 记的这份；mock 要是只记成功的，测试就
    断言不出「插件根本没发」这件事。
    """
    hub = mockhub.Hub()
    try:
        hub._on_unregister(registry_pb2.UnregisterRequest(instance_id="i-1", reason="插件优雅退出"))
        assert hub.unregisters == ["i-1"]
        assert hub.unregister_tokens == [""]
    finally:
        hub.close()


def test_注销凭证不符时不删任何行():
    """mock 不该比真中台宽松：注销凭**注册时下发的凭证**认属主。

    中台侧的判定与删除在同一条 SQL 里，空凭证或不符一律不删任何行。mock 松一格
    （比如只要带了凭证就删、或者干脆不看）的后果是：插件在本地跑绿，到线上才发现
    注销一直被拒——而这条链路的失败表现只是「实例多活了 ≤40s」，没人会去查。
    """
    hub = mockhub.Hub()
    try:
        hub._on_register(
            registry_pb2.RegisterRequest(plugin_name="a", version="1", instance_id="i-1")
        )
        row = {"i-1": mockhub.MOCK_STATE_TOKEN}
        assert hub.instances == row

        # 1) 没带凭证——注册从没成功过的插件就是这个样子
        hub._on_unregister(registry_pb2.UnregisterRequest(instance_id="i-1", reason="插件优雅退出"))
        assert hub.instances == row, "空凭证不得删任何行"

        # 2) 带了凭证但对不上——撞了 instance_id 的另一个插件手里是另一张
        hub._on_unregister(
            registry_pb2.UnregisterRequest(instance_id="i-1", state_token="别人的凭证")
        )
        assert hub.instances == row, "凭证不符不得删任何行"

        # 3) 带对了才摘掉
        hub._on_unregister(
            registry_pb2.UnregisterRequest(
                instance_id="i-1", state_token=mockhub.MOCK_STATE_TOKEN
            )
        )
        assert hub.instances == {}

        # 被拒的那两次也留了痕，否则「插件到底发没发」无从断言
        assert hub.unregisters == ["i-1", "i-1", "i-1"]
        assert hub.unregister_tokens == ["", "别人的凭证", mockhub.MOCK_STATE_TOKEN]
    finally:
        hub.close()


def test_注册被拒时不写实例行():
    """被拒的一方手里没有凭证，中台那边也没有它这一行——插件侧「没凭证就不发注销」
    这条规则成立的前提就是这个。"""
    hub = mockhub.Hub(
        mockhub.Options(reject_register=lambda _request: [registry_pb2.Rejection(message="不行")])
    )
    try:
        response = hub._on_register(
            registry_pb2.RegisterRequest(plugin_name="a", version="1", instance_id="i-1")
        )
        assert not response.accepted
        assert response.state_token == ""
        assert hub.instances == {}
    finally:
        hub.close()


def test_注册回执里带上心跳周期与状态凭证():
    """两个字段都不能漏：漏了周期插件会退化成兜底值，漏了凭证 HubState 用不了。"""
    hub = mockhub.Hub(mockhub.Options(heartbeat_interval_seconds=7))
    try:
        response = hub._on_register(registry_pb2.RegisterRequest(plugin_name="x", version="1"))
        assert response.heartbeat_interval_seconds == 7
        assert response.state_token == mockhub.MOCK_STATE_TOKEN
    finally:
        hub.close()


def test_free_addr给出可绑定的地址():
    import socket

    host, _, port = mockhub.free_addr().partition(":")
    assert host == "127.0.0.1"
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind((host, int(port)))


def test_等不到时抛超时而不是永久挂住():
    """等待助手挂死会把 CI 拖到超时上限才红，那比直接失败难查得多。"""
    hub = mockhub.Hub()
    try:
        with pytest.raises(TimeoutError):
            hub.wait_for_registration(1, timeout=0.2)
        with pytest.raises(TimeoutError):
            hub.wait_for_heartbeats(1, timeout=0.2)
    finally:
        hub.close()


def test_mock的心跳周期可配且缺省是1秒():
    """缺省 1 秒是为了测试要快——真中台的 10s 周期会让每条涉及心跳的测试多等十几秒。"""
    hub = mockhub.Hub()
    try:
        assert hub.opts.heartbeat_interval_seconds == 1
    finally:
        hub.close()

    # 显式给 0 或负数时退回缺省，而不是把 0 当成「不心跳」——那会让插件静默地
    # 一直心跳失败，而 mock 这边什么都看不出来
    hub = mockhub.Hub(mockhub.Options(heartbeat_interval_seconds=0))
    try:
        assert hub.opts.heartbeat_interval_seconds == 1
    finally:
        hub.close()

    hub = mockhub.Hub(mockhub.Options(heartbeat_interval_seconds=5))
    try:
        assert hub.opts.heartbeat_interval_seconds == 5
    finally:
        hub.close()
