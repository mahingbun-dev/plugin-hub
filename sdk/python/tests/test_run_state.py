"""骨架与状态客户端的接线：注入时机、凭证轮换、被拒后重新注册。

跑的是 ``serve`` 的**完整循环**（真起 gRPC 服务、真注册、真心跳），中台那一侧是
``fake_hub``——它同时装插件面与状态面，所以「插件侧撞上 401 → 心跳循环醒来 →
重走注册」这条链路能端到端验掉。真中台上这条链路要等凭证真的失效，等不到。

与 Go 的 ``sdk/go/hubkit/state_test.go`` 里那几个用例一一对应：
``TestStateAware被注入且能往返``、``Test状态凭证被拒时触发重新注册``、
``Test冷却窗口内的denial被忽略``、``Test冷却窗口过后denial仍能触发重注册``。
"""

from __future__ import annotations

import io
import json
import threading
import time

import grpc
import pytest
from echo_plugin import EchoPlugin
from fake_hub import FakeHub, Options

from hubkit import mockhub, serve
from hubkit.config import Config
from hubkit.log import JsonLogger
from hubkit.proto.hubv1 import plugin_pb2
from hubkit.run import Registrar, _HubConnection


class StatePlugin(EchoPlugin):
    """实现了 ``set_state`` 的插件：注册成功后应当收到注入。

    注入来自注册循环那个线程，测试在主线程上读，所以用锁护住——裸字段在这种
    「一个线程写、另一个线程读」的场景里拿到的可能是半成品。
    """

    def __init__(self, name: str = "state-plugin") -> None:
        self.name = name
        self._lock = threading.Lock()
        self._injected: list = []

    def manifest(self) -> plugin_pb2.PluginManifest:
        manifest = super().manifest()
        manifest.name = self.name
        manifest.version = "1.0.0"
        return manifest

    def set_state(self, state) -> None:
        with self._lock:
            self._injected.append(state)

    @property
    def injected(self) -> list:
        with self._lock:
            return list(self._injected)

    @property
    def state(self):
        injected = self.injected
        return injected[-1] if injected else None


class Runner:
    """跑一个插件，并把它的 JSON 日志逐行收好（与 test_run.py 的 _Recorder 同款）。"""

    def __init__(self, hub: FakeHub, plugin=None, **cfg_kwargs):
        self.hub = hub
        self.stream = io.StringIO()
        self.stop = threading.Event()
        self.addr = mockhub.free_addr()
        self.plugin = plugin if plugin is not None else StatePlugin()

        cfg = Config(
            hub_addr=hub.addr,
            advertise_addr=f"http://{self.addr}",
            listen_addr=self.addr,
            instance_id="py-state-test",
            logger=JsonLogger(stream=self.stream),
            **cfg_kwargs,
        )
        self.thread = threading.Thread(
            target=serve, args=(self.plugin, cfg, self.stop), daemon=True
        )

    def __enter__(self) -> "Runner":
        self.thread.start()
        return self

    def __exit__(self, *exc_info) -> None:
        self.stop.set()
        self.thread.join(timeout=15)

    def lines(self) -> list[dict]:
        return [json.loads(line) for line in self.stream.getvalue().splitlines() if line.strip()]

    def find(self, msg: str) -> list[dict]:
        return [line for line in self.lines() if line.get("msg") == msg]

    def find_partial(self, needle: str) -> list[dict]:
        return [line for line in self.lines() if needle in line.get("msg", "")]


def _wait_until(predicate, timeout: float = 5.0, what: str = ""):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.01)
    raise AssertionError(f"等待超时：{what}")


# ---------------------------------------------------------------- 注入


def test_注册成功后注入状态客户端():
    """核心行为：凭证由 run 拿到并交给状态客户端，插件作者不该自己管 token。"""
    with FakeHub() as hub:
        with Runner(hub) as runner:
            hub.wait_for_registration(1)

            state = _wait_until(
                lambda: runner.plugin.state, what="插件应收到状态客户端注入"
            )
            assert state.token == hub.opts.state_token

            # 拿到就能用：注入的凭证真的通了
            state.put("s", "k", b"v")
            assert state.get("s", "k") == b"v"


def test_注入的就是注册循环持有的那个客户端():
    """单独驱动 ``Registrar`` 才看得到这一点：注入的是**同一个对象**，凭证在它内部
    轮换——插件不必在重新注册后去换引用（换了也不会错，因为还是同一个）。
    """
    with FakeHub(Options(rotate_token=True)) as hub:
        cfg = Config(
            hub_addr=hub.addr,
            advertise_addr="http://127.0.0.1:1",
            instance_id="registrar-state",
            retry_interval=0.1,
            logger=JsonLogger(stream=io.StringIO()),
        ).with_defaults()

        conn = _HubConnection(cfg)
        try:
            plugin = StatePlugin()
            registrar = Registrar(conn, plugin, cfg, cfg.logger)
            stop = threading.Event()
            thread = threading.Thread(target=registrar.loop, args=(stop,), daemon=True)
            thread.start()
            try:
                hub.wait_for_registration(1)
                state = _wait_until(lambda: plugin.state, what="注入")
                assert state is registrar.state
                assert state.token == hub.current_token
            finally:
                stop.set()
                thread.join(timeout=10)
        finally:
            conn.close()


def test_不实现set_state的插件照常注册():
    """对应 Go 的「可选接口，老插件一行不用改」——继承 Plugin 的结果就是这个。"""
    with FakeHub() as hub:
        with Runner(hub, plugin=EchoPlugin()) as runner:
            hub.wait_for_registration(1)
            hub.wait_for_heartbeats(2, timeout=10)
            assert not runner.find("状态客户端注入失败")


def test_鸭子类型的插件也能被注入():
    """没继承 ``hubkit.Plugin``、只实现了四个方法的插件同样拿得到状态客户端。"""

    class Duck:
        def manifest(self):
            return plugin_pb2.PluginManifest(
                name="duck", version="1.0.0", consumes=[], produces=[], tools=[]
            )

        def descriptor(self):
            return b""

        def validate(self, ctx, env):
            from hubkit import valid

            return valid()

        def handle(self, ctx, env):
            return env

        def set_state(self, state):
            self.state = state

    duck = Duck()
    with FakeHub() as hub:
        with Runner(hub, plugin=duck) as _runner:
            hub.wait_for_registration(1)
            state = _wait_until(lambda: getattr(duck, "state", None), what="注入")
            assert state.token == hub.opts.state_token


def test_每次重新注册都再注入一次且换凭证():
    """中台要求重注册（实例被摘除）后，插件拿到的新客户端里必须是新凭证。

    与 Go 一致：注入的是**同一个对象**，凭证在它内部轮换——所以断言的是
    「又注入了一次」+「同一个对象的 token 变了」。
    """
    hub = FakeHub(Options(rotate_token=True, heartbeat_interval_seconds=1, reregister_after_beats=1))
    with hub:
        with Runner(hub, retry_interval=0.2) as runner:
            hub.wait_for_registration(1)
            _wait_until(lambda: runner.plugin.state, what="首次注入")
            first_token = runner.plugin.state.token

            hub.wait_for_registration(2, timeout=15)

            _wait_until(
                lambda: len(runner.plugin.injected) >= 2, what="重新注册后应再次注入"
            )
            assert runner.plugin.state is runner.plugin.injected[0]
            assert _wait_until(
                lambda: runner.plugin.state.token != first_token,
                what="重新注册后应换成新凭证",
            )


def test_插件自己的set_state抛异常不影响注册():
    """注册在网络上已经成功了。把它当失败会变成一轮又一轮的重注册风暴。"""

    class Broken(StatePlugin):
        def set_state(self, state):
            raise RuntimeError("插件自己的 bug")

    with FakeHub() as hub:
        with Runner(hub, plugin=Broken()) as runner:
            hub.wait_for_registration(1)
            failures = runner.find_partial("状态客户端注入失败")
            assert failures, runner.lines()
            assert "插件自己的 bug" in failures[0]["err"], "要把插件自己的异常原样带出来"
            # 注册没有失败，心跳照常
            hub.wait_for_heartbeats(2, timeout=10)
            assert hub.registration_count == 1


def test_中台没下发凭证时给出提示():
    """凭证为空（迁移前登记的旧实例）——客户端还是注入，但日志要说清楚。"""
    with FakeHub(Options(state_token="")) as hub:
        with Runner(hub) as runner:
            hub.wait_for_registration(1)
            state = _wait_until(lambda: runner.plugin.state, what="注入")
            assert state.token == ""
            assert runner.find("中台未下发状态凭证，HubState 与插件互调将不可用"), runner.lines()


# ---------------------------------------------------------------- 被拒后重新注册


def test_状态凭证被拒时触发重新注册():
    """撞上 401 → 心跳循环醒来 → 重走注册流程换新凭证。

    先睡过冷却窗口（它等于 retry_interval）：刚注册就被拒的那次会被速率下限挡掉，
    本用例要验的是**窗口之外**的那次。
    """
    hub = FakeHub(Options(deny_state=True))
    with hub:
        with Runner(hub, retry_interval=0.3) as runner:
            hub.wait_for_registration(1)
            state = _wait_until(lambda: runner.plugin.state, what="注入")

            time.sleep(0.5)  # 睡过冷却窗口

            # 401 必须原样交给插件（不吞掉、不改码）

            with pytest.raises(grpc.RpcError) as err:
                state.get("s", "k")
            assert err.value.code() == grpc.StatusCode.UNAUTHENTICATED

            hub.wait_for_registration(2, timeout=10)
            assert runner.find("状态凭证被拒，重新注册以换取新凭证"), runner.lines()


def test_冷却窗口内的denial被忽略():
    """速率下限：没有它，一次成功注册之后紧接着排空信号就是零延迟自旋——
    注册速率等于 Register RPC 的延迟，同时还在反复探测插件自己的地址。"""
    hub = FakeHub(Options(deny_state=True))
    with hub:
        with Runner(hub, retry_interval=5.0) as runner:
            hub.wait_for_registration(1)
            state = _wait_until(lambda: runner.plugin.state, what="注入")

            with pytest.raises(grpc.RpcError):
                state.get("s", "k")

            # 给「没有冷却窗口的实现」足够多的时间把第 2 次注册发出来
            time.sleep(0.6)
            assert hub.registration_count == 1, "冷却窗口内的 denial 应被忽略"
            assert runner.find("状态凭证被拒，但距上次注册不足冷却窗口，忽略本次")


def test_冷却窗口过后denial仍能触发重新注册():
    """上一条的反面：该挡的挡、不该挡的不能挡。"""
    hub = FakeHub(Options(deny_state=True))
    with hub:
        with Runner(hub, retry_interval=0.2) as runner:
            hub.wait_for_registration(1)
            state = _wait_until(lambda: runner.plugin.state, what="注入")

            time.sleep(0.4)  # 窗口已过

            with pytest.raises(grpc.RpcError):
                state.get("s", "k")

            hub.wait_for_registration(2, timeout=10)


def test_非凭证错误不会触发重新注册():
    """INVALID_ARGUMENT（中台按配置一律回它）与凭证无关，重注册解决不了。"""

    hub = FakeHub(Options(state_error=grpc.StatusCode.INVALID_ARGUMENT))
    with hub:
        with Runner(hub) as runner:
            hub.wait_for_registration(1)
            state = _wait_until(lambda: runner.plugin.state, what="注入")

            with pytest.raises(grpc.RpcError) as err:
                state.get("s", "k")
            assert err.value.code() == grpc.StatusCode.INVALID_ARGUMENT

            # 心跳继续打（而不是掉回注册循环）
            hub.wait_for_heartbeats(2, timeout=10)
            assert hub.registration_count == 1


# ---------------------------------------------------------------- 注销凭证


def test_注销用的是最近一次注册的凭证():
    """注销带的是**最近一次注册成功**下发的那张凭证，不是最早那张。

    真中台每次注册都轮换凭证、旧凭证立即作废。而 mockhub 下发的凭证是个常量，
    「非空且等于它」这种断言照样能让「把第一张凭证缓存下来」的实现全绿——真机上
    那张早就作废了，注销被静默拒掉，插件侧只看到实例多活了 ≤40s（没人会去查）。

    这里刻意走一次「401 → 重新注册 → 换凭证」，再退出：凭证是随注册轮换的，
    只有这一条能验出注销读的是哪一张。
    """
    hub = FakeHub(Options(rotate_token=True, deny_state=True))
    with hub:
        with Runner(hub, retry_interval=0.2) as runner:
            hub.wait_for_registration(1)
            state = _wait_until(lambda: runner.plugin.state, what="首次注入")
            first_token = state.token

            time.sleep(0.3)  # 睡过冷却窗口（它等于 retry_interval）
            with pytest.raises(grpc.RpcError):
                state.get("s", "k")  # 401 → 心跳循环醒来 → 重新注册换凭证

            hub.wait_for_registration(2, timeout=10)
            latest = _wait_until(
                lambda: state.token if state.token != first_token else None,
                what="重新注册后应换成新凭证",
            )

        # Runner 退出时会优雅退出，注销就在那一刻发出去
        assert hub.unregisters == ["py-state-test"]
        assert hub.unregister_tokens == [latest], "注销要带最新那张凭证，不是最早那张"
