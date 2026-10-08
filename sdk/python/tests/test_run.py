"""骨架行为：注册、心跳、被摘除后自愈、优雅退出。

这里跑的是 Go 侧 ``sdk/go/hubkit/run_test.go`` 的同一批场景。**自愈那一条是重点**：
它对应的 ``reregister_required`` 分支只能靠 mock 中台按需触发，在中台上永远
不会在你想验的时候发生（真中台要等心跳超时摘除，那要几分钟）。
"""

from __future__ import annotations

import io
import json
import threading
import time

import pytest
from echo_plugin import EchoPlugin
from hubkit import mockhub, serve
from hubkit.config import HEARTBEAT_FALLBACK_INTERVAL, Config
from hubkit.log import JsonLogger
from hubkit.proto.hubv1 import registry_pb2
from hubkit.run import RegistrationRejected, Registrar, reject_code_name, _HubConnection, check_manifest


class _Recorder:
    """跑一个插件，并把它打出来的 JSON 日志逐行收好。

    日志是**断言对象**而不是调试副产物：接入的四道关里，L3 的判据就是
    「插件日志里出现『已注册到中台』」——那么这句话本身就必须被测。
    """

    def __init__(self, plugin=None, hub=None, **cfg_kwargs):
        self.hub = hub or mockhub.Hub()
        self.stream = io.StringIO()
        self.stop = threading.Event()
        self.addr = mockhub.free_addr()

        cfg = Config(
            hub_addr=self.hub.addr,
            advertise_addr=f"http://{self.addr}",
            listen_addr=self.addr,
            instance_id="py-run-test",
            logger=JsonLogger(stream=self.stream),
            **cfg_kwargs,
        )
        self.thread = threading.Thread(
            target=serve, args=(plugin or EchoPlugin(), cfg, self.stop), daemon=True
        )

    def __enter__(self) -> "_Recorder":
        self.thread.start()
        return self

    def __exit__(self, *exc_info) -> None:
        self.stop.set()
        self.thread.join(timeout=15)
        self.hub.close()

    def lines(self) -> list[dict]:
        return [json.loads(line) for line in self.stream.getvalue().splitlines() if line.strip()]

    def find(self, msg: str) -> list[dict]:
        return [line for line in self.lines() if line.get("msg") == msg]

    def wait_for_log(self, msg: str, count: int = 1, timeout: float = 15.0) -> None:
        """等到插件日志里出现至少 `count` 条 `msg`，等不到就报错。

        **不能拿 `hub.wait_for_registration(n)` 当它的替代品**：mock 是在**收到注册
        请求的那一刻**就登记的，而插件要等那个 RPC 返回之后才打「已注册到中台」。
        两者之间有一小段窗口，`wait_for_registration` 返回时插件那一行可能还没落盘
        ——机器一忙窗口就张开，于是测试偶发失败，而失败信息（「没等到已注册」）看着
        像插件真的没注册上，会把注意力引到完全错误的方向。

        这类断言的对象是**异步才成立的条件**，就该等它，而不是赌它在下一行之前成立。
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if len(self.find(msg)) >= count:
                return
            time.sleep(0.02)
        raise AssertionError(
            f"等 {timeout}s 也没等到 {count} 条「{msg}」；实际日志：{self.lines()}"
        )


def test_注册到中台并出现已注册日志():
    """L3 的判据：日志里出现「已注册到中台」，且中台那边真的收到了注册请求。"""
    with _Recorder() as rec:
        rec.hub.wait_for_registration(1)
        rec.wait_for_log("已注册到中台")

        registered = rec.find("已注册到中台")
        assert registered, f"没有出现『已注册到中台』：{rec.lines()}"
        assert registered[0]["plugin"] == "echo"
        assert registered[0]["version"] == "0.1.0"
        assert registered[0]["level"] == "INFO"

        request = rec.hub.last_registration
        assert request.plugin_name == "echo"
        assert request.instance_id == "py-run-test"
        # 中台按这个地址回调，注册时会先探测连通性——报错了插件就永远接不进来
        assert request.advertise_addr == f"http://{rec.addr}"


def test_持续发心跳():
    """心跳会让 mock 那边的心跳计数一直涨。"""
    with _Recorder() as rec:
        rec.hub.wait_for_registration(1)
        rec.hub.wait_for_heartbeats(3, timeout=10)
        assert rec.hub.heartbeats >= 3


def test_被摘除后重新注册():
    """中台回 ``reregister_required`` 时，插件必须重走注册流程。

    这是「实例掉线后自愈」的全部内容：中台摘掉实例（真中台上是心跳超时，
    这里由 mock 按拍数触发）之后，插件不能就这么哑下去。
    """
    hub = mockhub.Hub(mockhub.Options(heartbeat_interval_seconds=1, reregister_after_beats=2))
    with _Recorder(hub=hub) as rec:
        hub.wait_for_registration(1)

        # 第 3 拍起 mock 开始要求重注册；插件应当立刻重走注册流程
        hub.wait_for_registration(2, timeout=15)
        # 日志那一侧要单独等：mock 先登记再回响应，插件要等响应回来才打这一行
        rec.wait_for_log("已注册到中台", 2)

        assert len(rec.find("中台要求重新注册（实例可能已被摘除）")) >= 1


def test_注册被拒时逐条打原因并重试():
    """中台拒绝时必须把每条结构化原因单独打一行，并且继续重试。"""

    def reject(_request):
        return [
            registry_pb2.Rejection(
                code=registry_pb2.REJECT_CODE_UNREACHABLE,
                message="插件地址不可达",
                detail="请确认 advertise_addr 填的是中台视角下可达的地址",
            )
        ]

    hub = mockhub.Hub(mockhub.Options(reject_register=reject))
    with _Recorder(hub=hub, retry_interval=0.2) as rec:
        hub.wait_for_registration(2, timeout=10)
        rec.wait_for_log("中台拒绝了注册")

        rejected = rec.find("中台拒绝了注册")
        assert rejected, f"没有打出拒绝原因：{rec.lines()}"
        assert rejected[0]["code"] == "UNREACHABLE"
        assert rejected[0]["message"] == "插件地址不可达"
        assert "advertise_addr" in rejected[0]["detail"]

        summary = rec.find("注册未通过，稍后重试")
        assert summary[0]["reasons"] == 1
        assert summary[0]["retry_in"] == "0.2s"

        # 被拒之后不能就这么停下——中台晚起来是常态
        assert len(hub.registrations) >= 2


def test_优雅退出时主动注销():
    """收到停止信号后必须调 ``Unregister``，否则实例要等心跳超时才消失。

    注销请求还要带上**注册响应里下发的那份凭证**：中台凭它认出「你是这一行的
    主人」。只按 ``instance_id`` 删行的话，撞了 id 的两个插件里谁先退出谁就把
    对方那一行删掉。

    断言分两层：中台**收到**了带对凭证的请求（``unregisters`` / ``unregister_tokens``），
    以及实例行**真的被摘掉了**（``instances``）——响应是个空消息，插件侧分不出
    成没成，所以「删干净了」只有中台那一侧看得见。
    """
    rec = _Recorder()
    rec.thread.start()
    rec.hub.wait_for_registration(1)
    # 先确认这一行**存在过**：不然「退出后 instances 是空的」在中台根本没写行时
    # 也成立，那条断言就白写了
    assert rec.hub.instances == {"py-run-test": mockhub.MOCK_STATE_TOKEN}

    rec.stop.set()
    rec.thread.join(timeout=15)

    assert rec.hub.unregisters == ["py-run-test"]
    assert rec.hub.unregister_tokens == [mockhub.MOCK_STATE_TOKEN], "注销要带上注册时下发的凭证"
    assert rec.hub.instances == {}, "实例行应当被摘掉"
    assert rec.find("已主动注销（中台据此立刻摘除本实例）"), "成功也该在日志里留下一条"
    assert not rec.find("注销失败（中台会在心跳超时后自行摘除）")
    assert rec.find("插件已退出")
    rec.hub.close()


def test_注册从未成功时退出不发注销():
    """注册被拒的插件手里**没有凭证**，退出时一个请求都不该发。

    这条路径正是「删掉属主那一行」的来源：`instance_id` 是插件自报的、可以跟别的
    插件撞（缺省「主机名-PID」，同一 host 网络下容器 PID 又都是 1），而注册被拒的
    插件恰恰是最可能撞上的那种（它进不来，配置多半就是抄的）。它发一次不带身份的
    注销，中台若只按 instance_id 删行，删掉的就是**属主**那一行。
    """

    def reject(_request):
        return [
            registry_pb2.Rejection(
                code=registry_pb2.REJECT_CODE_INSTANCE_CONFLICT,
                message="instance_id 已被其它插件占用",
                detail="换个 instance_id",
            )
        ]

    hub = mockhub.Hub(mockhub.Options(reject_register=reject))
    rec = _Recorder(hub=hub, retry_interval=0.2)
    rec.thread.start()
    try:
        hub.wait_for_registration(1, timeout=10)
        rec.wait_for_log("中台拒绝了注册")

        rec.stop.set()
        rec.thread.join(timeout=15)

        assert hub.unregisters == [], "没注册成功就没有可摘除的实例行，不该发注销"
        assert rec.find("本实例没有注册凭证，跳过主动注销（没注册成功就没有可摘除的实例行）"), (
            rec.lines()
        )
    finally:
        hub.close()


def test_心跳周期由中台指定():
    """注册回执里的 ``heartbeat_interval_seconds`` 必须被采纳，而不是用兜底值。"""
    hub = mockhub.Hub(mockhub.Options(heartbeat_interval_seconds=1))
    with _Recorder(hub=hub) as rec:
        hub.wait_for_registration(1)
        # 兜底值是 10s；如果没采纳中台的值，这里 5s 内拍数会停在 0
        hub.wait_for_heartbeats(2, timeout=5)
        assert HEARTBEAT_FALLBACK_INTERVAL > 1


def test_注册失败会一直重试():
    """中台没起来时插件不能退出——它可能比插件晚起来。

    顺带钉住「还没注册上就退出」那一支：这种插件手里同样没有凭证，退出时不该发
    注销（它连实例行都没有）。与「注册被拒」是同一条代码路径，但两者在真实环境里
    的成因完全不同（中台没起来 vs 中台明确拒了），哪一支坏掉都该被测到。
    """
    # 指向一个没人监听的端口
    stream = io.StringIO()
    cfg = Config(
        hub_addr="http://127.0.0.1:1",
        advertise_addr="http://127.0.0.1:1",
        listen_addr=mockhub.free_addr(),
        instance_id="retry-test",
        retry_interval=0.1,
        logger=JsonLogger(stream=stream),
    )
    stop = threading.Event()
    thread = threading.Thread(target=serve, args=(EchoPlugin(), cfg, stop), daemon=True)
    thread.start()
    try:
        time.sleep(0.5)
        # 关键断言：进程还活着（线程没抛异常退出），注册循环还在转
        assert thread.is_alive(), "注册失败不该让插件退出"
    finally:
        stop.set()
        thread.join(timeout=10)

    messages = [json.loads(line)["msg"] for line in stream.getvalue().splitlines() if line.strip()]
    assert "本实例没有注册凭证，跳过主动注销（没注册成功就没有可摘除的实例行）" in messages, messages


def test_缺必填配置时报出可照做的提示():
    cfg = Config(hub_addr="", advertise_addr="", listen_addr=mockhub.free_addr())
    with pytest.raises(ValueError) as err:
        serve(EchoPlugin(), cfg, threading.Event())
    assert "HUB_ADDR" in str(err.value)
    assert "HUB_ADVERTISE_ADDR" in str(err.value)


def test_manifest缺版本号在本地就被挡住():
    class _NoVersion(EchoPlugin):
        def manifest(self):
            manifest = super().manifest()
            manifest.version = ""
            return manifest

    with pytest.raises(ValueError) as err:
        check_manifest(_NoVersion())
    assert "版本号" in str(err.value)


def test_拒绝码翻译去掉前缀():
    assert reject_code_name(registry_pb2.REJECT_CODE_UNREACHABLE) == "UNREACHABLE"
    assert reject_code_name(registry_pb2.REJECT_CODE_INSTANCE_CONFLICT) == "INSTANCE_CONFLICT"
    assert reject_code_name(9999).startswith("未知拒绝码")


def test_RegistrationRejected把原因排开():
    exc = RegistrationRejected(
        [
            registry_pb2.Rejection(
                code=registry_pb2.REJECT_CODE_BREAKING_CHANGE,
                message="相对版本 1.0.0 存在破坏性契约变更",
                detail="wms.v1.Order.sku 已被删除",
            )
        ]
    )
    text = str(exc)
    assert "BREAKING_CHANGE" in text
    assert "wms.v1.Order.sku 已被删除" in text


def test_中台地址带scheme也能连上():
    """``HUB_ADDR`` 允许写 ``http://host:port``（与 Go 侧同款）。

    直接把 URL 交给 grpc 会变成 ``DNS resolution failed``——完全看不出是
    多写了个前缀。
    """
    hub = mockhub.Hub()
    try:
        cfg = Config(hub_addr=hub.addr).with_defaults()
        assert cfg.grpc_target() == hub.addr.removeprefix("http://")
        assert not cfg.use_tls()

        conn = _HubConnection(cfg)
        try:
            # 真的走一次 RPC 才说明目标拼对了
            conn.registry.Register(
                registry_pb2.RegisterRequest(plugin_name="x", version="1", instance_id="i"),
                timeout=2.0,
            )
        finally:
            conn.close()
        assert len(hub.registrations) == 1
    finally:
        hub.close()


def test_Registrar可被单独驱动():
    """``Registrar`` 从 ``serve`` 里拆出来就是为了能被这样驱动。"""
    hub = mockhub.Hub(mockhub.Options(heartbeat_interval_seconds=1))
    try:
        cfg = Config(
            hub_addr=hub.addr,
            advertise_addr="http://127.0.0.1:1",
            instance_id="registrar-only",
            retry_interval=0.1,
            logger=JsonLogger(stream=io.StringIO()),
        ).with_defaults()

        conn = _HubConnection(cfg)
        try:
            registrar = Registrar(conn, EchoPlugin(), cfg, cfg.logger)
            stop = threading.Event()
            thread = threading.Thread(target=registrar.loop, args=(stop,), daemon=True)
            thread.start()

            hub.wait_for_registration(1)
            # 与前面几处同一个竞态：mock 先登记再回响应，registrar 要等响应回来才 +1
            deadline = time.monotonic() + 10
            while registrar.register_count < 1 and time.monotonic() < deadline:
                time.sleep(0.02)
            assert registrar.register_count == 1
            assert registrar.heartbeat_interval == 1.0

            stop.set()
            thread.join(timeout=10)
        finally:
            conn.close()
    finally:
        hub.close()
