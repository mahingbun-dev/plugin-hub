"""契约一致性套件（``hubkit.conformance``）的测试。

重点验的是「它能抓住该抓的东西」——一个从不报错的检查套件比没有套件更糟，
因为它会给人「已经验过了」的错觉。
"""

from __future__ import annotations

import threading
import time

from echo_plugin import EchoPlugin
from hubkit import Plugin, conformance, mockhub, serve
from hubkit.config import Config
from hubkit.log import JsonLogger
from hubkit.proto.hubv1 import plugin_pb2


class _NoName(Plugin):
    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(version="1.0.0")


class _BadName(Plugin):
    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(name="-中文名", version="1.0.0")


class _NoVersion(Plugin):
    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(name="ok-name")


class _NoContract(Plugin):
    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(name="no-contract", version="1.0.0")


class _DeclaresOwnType:
    """声明了自有类型却给不出 descriptor —— 中台注册期会拒的那一种。"""

    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(
            name="own-type",
            version="1.0.0",
            produces=[plugin_pb2.MessageContract(fq_name="wms.v1.OrderCreated")],
        )

    def descriptor(self) -> bytes:
        return b""


class _DuplicateTool(EchoPlugin):
    def manifest(self) -> plugin_pb2.PluginManifest:
        manifest = super().manifest()
        manifest.tools.append(plugin_pb2.ToolDecl(name="echo"))
        return manifest


def _check(report: conformance.Report, name: str) -> conformance.Check:
    for check in report.checks:
        if check.name == name:
            return check
    raise AssertionError(f"报告里没有名为 {name} 的检查项：{report}")


def test_合规插件全部通过():
    report = conformance.local(EchoPlugin())
    assert report.passed(), str(report)
    assert report.subject == "echo@0.1.0"


def test_缺少插件名被抓住():
    report = conformance.local(_NoName())
    assert not report.passed()
    assert "缺少插件名" in _check(report, "插件名合法").detail


def test_非法插件名被抓住():
    report = conformance.local(_BadName())
    assert not report.passed()
    assert "只允许字母数字" in _check(report, "插件名合法").detail


def test_缺少版本号被抓住():
    report = conformance.local(_NoVersion())
    assert not report.passed()
    assert "flow 靠它锁定实例" in _check(report, "版本号存在").detail


def test_没有声明任何契约被抓住():
    report = conformance.local(_NoContract())
    assert not report.passed()
    assert "至少声明一个" in _check(report, "声明了契约").detail


def test_声明自有类型却没有descriptor被抓住():
    report = conformance.local(_DeclaresOwnType())
    assert not report.passed()
    assert "wms.v1.OrderCreated" in _check(report, "声明的类型都在 descriptor 中").detail


def test_工具名重复被抓住():
    report = conformance.local(_DuplicateTool())
    assert not report.passed()
    assert "重复声明" in _check(report, "工具声明合法").detail


def test_manifest为None时报告仍可用():
    class _NoneManifest(Plugin):
        def manifest(self):  # type: ignore[override]
            return None

    report = conformance.local(_NoneManifest())
    assert not report.passed()
    assert report.subject == "（未命名插件）"


def test_well_known类型不要求在descriptor里():
    """``google.protobuf.Struct`` 是中台豁免的，不该被判成「声明了自有类型」。"""
    report = conformance.local(EchoPlugin())
    assert _check(report, "descriptor 可用").detail == "无自有 proto（只用 well-known 载荷）"
    assert _check(report, "声明的类型都在 descriptor 中").passed


def test_运行时检查对真实插件全绿():
    """跑一个真的插件起来，用 ``runtime()`` 检查它。

    这一条是 L2 的自动化版本——它验的是「gRPC 起得来、五个方法应答正常」，
    而注册那一关（L3）在 ``test_run.py`` 里。
    """
    hub = mockhub.Hub()
    stop = threading.Event()
    addr = mockhub.free_addr()
    cfg = Config(
        hub_addr=hub.addr,
        advertise_addr=f"http://{addr}",
        listen_addr=addr,
        instance_id="conformance-runtime",
        logger=JsonLogger(),
    )

    thread = threading.Thread(target=serve, args=(EchoPlugin(), cfg, stop), daemon=True)
    thread.start()
    try:
        _wait_for_port(addr)
        report = conformance.runtime(f"http://{addr}")
        assert report.passed(), str(report)
        assert _check(report, "插件体返回信封").detail == "已返回 JSON 载荷"
    finally:
        stop.set()
        thread.join(timeout=10)
        hub.close()


def test_运行时检查对不通的地址给出可读原因():
    """连不通时报告必须给出「错在哪」，而不是抛异常。

    注意失败落在 ``Health 可应答`` 而不是 ``插件地址可用``：grpc 建通道是**惰性**的，
    地址不通这件事要到第一次 RPC 才暴露——Go 侧行为相同（``grpc.NewClient`` 也不连）。
    """
    report = conformance.runtime("http://127.0.0.1:1", timeout=1.0)
    assert not report.passed()

    failures = report.failures()
    assert failures[0].name == "Health 可应答"
    assert failures[0].detail, "失败项必须带上原因，否则使用者无从下手"


def _wait_for_port(addr: str, timeout: float = 5.0) -> None:
    """等插件真的开始监听。

    直接连会比 ``server.start()`` 晚一拍——``serve`` 里起来之后测试线程才拿到
    控制权，中间那段窗口里连过去是 connection refused。
    """
    import grpc

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        channel = grpc.insecure_channel(addr)
        try:
            grpc.channel_ready_future(channel).result(timeout=0.5)
            return
        except Exception:  # noqa: BLE001
            time.sleep(0.05)
        finally:
            channel.close()
    raise AssertionError(f"插件 {addr} 在 {timeout}s 内没有开始监听")


