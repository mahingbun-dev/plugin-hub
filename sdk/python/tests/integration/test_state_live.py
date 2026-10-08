"""对着**真中台**跑通 HubState。

单测与 mock 能验的是「客户端自己做得对不对」；真中台才验得了那三件只有它说了算的事：
凭证反查出的插件名真的被拼进了键前缀、状态真的落在 Redis 里、注销后凭证真的失效。

**默认跳过**——它需要一个真中台在跑（PG + Redis 齐全），在中台不在时跑它只是红得没有
意义（不是「测试失败」）。要跑就给它地址：

    HUB_INTEGRATION_ADDR=http://127.0.0.1:8093 python -m pytest tests/integration -q -s

``-s`` 是给证据留的：这个文件里的 print 就是「确实往返成功」的现场记录。
插件会以 ``py-state-it`` 的名字真注册进那台中台，跑完主动注销（实例行随即被删）。
"""

from __future__ import annotations

import io
import json
import os
import threading
import time

import grpc
import pytest

from hubkit import Plugin, STRUCT_FQ_NAME, mockhub, serve
from hubkit.config import Config
from hubkit.log import JsonLogger
from hubkit.proto.hubv1 import plugin_pb2, registry_pb2, registry_pb2_grpc
from hubkit.state import MAX_VALUE_BYTES, StateClient

HUB_ADDR = os.environ.get("HUB_INTEGRATION_ADDR", "").strip()

pytestmark = pytest.mark.skipif(
    not HUB_ADDR,
    reason="需要真中台：设 HUB_INTEGRATION_ADDR（例如 http://127.0.0.1:8093）",
)

PLUGIN_NAME = "py-state-it"


class LivePlugin(Plugin):
    """最小插件 + 状态钩子。manifest 保持稳定：同一个版本号重复注册要被接受。"""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._state: StateClient | None = None

    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(
            name=PLUGIN_NAME,
            version="1.0.0",
            description="Python SDK 的 HubState 集成验证",
            consumes=[plugin_pb2.MessageContract(fq_name=STRUCT_FQ_NAME)],
            tools=[plugin_pb2.ToolDecl(name="state-it", description="状态往返")],
        )

    def validate(self, ctx, env):
        from hubkit import valid

        return valid()

    def handle(self, ctx, env):
        return env

    def set_state(self, state: StateClient) -> None:
        with self._lock:
            self._state = state

    @property
    def state(self) -> StateClient | None:
        with self._lock:
            return self._state


class Live:
    """跑一个真插件，直到退出。日志同时留一份在内存里，注册不上时能看出为什么。"""

    def __init__(self, retry_interval: float | None = None) -> None:
        self.addr = mockhub.free_addr()
        self.stream = io.StringIO()
        self.stop = threading.Event()
        self.plugin = LivePlugin()
        # instance_id 带上 pid：与别的进程/别的测试跑在同一台中台上时互不顶掉
        self.instance_id = f"{PLUGIN_NAME}-{os.getpid()}"
        cfg = Config(
            hub_addr=HUB_ADDR,
            advertise_addr=f"http://{self.addr}",
            listen_addr=self.addr,
            instance_id=self.instance_id,
            logger=JsonLogger(stream=self.stream),
            **({} if retry_interval is None else {"retry_interval": retry_interval}),
        )
        self.thread = threading.Thread(
            target=serve, args=(self.plugin, cfg, self.stop), daemon=True
        )

    def __enter__(self) -> "Live":
        self.thread.start()
        return self

    def __exit__(self, *exc_info) -> None:
        self.stop.set()
        self.thread.join(timeout=15)

    def state(self, timeout: float = 30.0) -> StateClient:
        """等注册成功并注入状态客户端；超时就把插件日志一起抛出来。"""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.plugin.state is not None:
                return self.plugin.state
            time.sleep(0.02)
        raise AssertionError(f"等不到状态客户端注入。插件日志：\n{self.log_text()}")

    def log_text(self) -> str:
        return self.stream.getvalue()

    def unregister(self) -> None:
        """停掉插件（会走优雅退出那条路，主动注销）。"""
        self.stop.set()
        self.thread.join(timeout=15)


def dial(hub_addr: str) -> grpc.Channel:
    return grpc.insecure_channel(hub_addr.removeprefix("http://").removeprefix("https://"))


def unique_namespace(tag: str) -> str:
    """每次跑一个**不重名**的命名空间。

    同一个插件的状态空间是持久的（中台按插件名加前缀，Redis 里的键不会随实例消失），
    所以写死 namespace 的测试第二次跑就会看到上一次留下的键——扫描类断言会莫名其妙
    地失败。加上 pid 与毫秒时间戳即可：字符集仍是 [A-Za-z0-9_.-]。
    """
    return f"{tag}.{os.getpid()}.{int(time.time() * 1000)}"


def test_读写往返_扫描_删除与TTL():
    ns = unique_namespace("orders")
    other_ns = unique_namespace("other")

    with Live() as live:
        state = live.state()
        print(f"\n[注册回执] state_token={state.token!r} namespace={ns}")

        try:
            # ---- Put → Get
            state.put(ns, "SO-1", b"payload-1")
            got = state.get(ns, "SO-1")
            print(f"[Get] {ns}/SO-1 → {got!r}")
            assert got == b"payload-1"

            # ---- 不存在的键与空值是两回事
            assert state.get(ns, "SO-404") is None
            state.put(ns, "empty", b"")
            assert state.get(ns, "empty") == b""
            print("[Get] 不存在的键 → None；空值 → b''（两者可区分）")

            # ---- Scan 命中前缀，且只在自己的命名空间里
            state.put(ns, "SO-2", b"payload-2")
            state.put(other_ns, "SO-9", b"payload-9")
            entries = state.scan(ns, "SO-", 10)
            print(f"[Scan] {ns}/prefix=SO- → {sorted((e.key, e.value) for e in entries)}")
            # Redis 的 SCAN 不保证顺序，所以比集合而不是列表
            assert sorted((e.key, e.value) for e in entries) == [
                ("SO-1", b"payload-1"),
                ("SO-2", b"payload-2"),
            ]

            keys = sorted(e.key for e in state.scan(ns))
            print(f"[Scan] {ns}（空 prefix）→ {keys}")
            # 只能扫到自己的命名空间：另一个命名空间里的 SO-9 不在内
            assert keys == ["SO-1", "SO-2", "empty"]

            # ---- 1MB 的值（中台上限）真的能写进去、原样读回来
            big = b"x" * MAX_VALUE_BYTES
            state.put(ns, "big", big)
            assert state.get(ns, "big") == big
            print(f"[Get] {ns}/big → {len(state.get(ns, 'big'))} 字节（正好是上限）")

            # ---- Delete
            assert state.delete(ns, "SO-1") is True
            assert state.get(ns, "SO-1") is None
            assert state.delete(ns, "SO-1") is False  # 删不存在的键不是错误
            print(f"[Delete] {ns}/SO-1 → True，再删一次 → False，Get → None")

            # ---- TTL：1 秒后真的没了（中台按 ttl_seconds 写 Redis）
            state.put(ns, "ttl", b"v", ttl_seconds=1)
            assert state.get(ns, "ttl") == b"v"
            time.sleep(1.5)
            assert state.get(ns, "ttl") is None
            print("[TTL] ttl_seconds=1 的键：立刻读得到，1.5 秒后读不到")

            # ---- 换个假凭证：真中台必须判 401
            with dial(HUB_ADDR) as channel:
                rogue = StateClient(channel)
                rogue.set_token("not-a-real-token")
                with pytest.raises(grpc.RpcError) as err:
                    rogue.get(ns, "SO-2")
                print(f"[认证] 假凭证 → {err.value.code()}（{err.value.details()}）")
                assert err.value.code() == grpc.StatusCode.UNAUTHENTICATED

            # ---- 本地预检在中台面前也是对的（中台对这些输入同样回 INVALID_ARGUMENT）
            with pytest.raises(ValueError):
                state.put(ns, "SO-3", b"x" * (MAX_VALUE_BYTES + 1))
            print("[本地预检] 超限的值在客户端就被挡住，没有产生往返")
        finally:
            # 收尾：状态空间是按插件名持久的，测试自己造的数据自己清掉
            for namespace in (ns, other_ns):
                for entry in state.scan(namespace, "", 1000):
                    state.delete(namespace, entry.key)
            print(f"[清理] 两个命名空间已清空：{state.scan(ns)} / {state.scan(other_ns)}")


def test_注销后旧凭证失效():
    """注销（实例被摘除）后凭证立即失效——这正是「401 要触发重新注册」的理由。

    中台把凭证落在实例行上，实例行没了凭证也就没了。插件在**另一个**实例上还拿着
    旧凭证时，唯一的自救路径就是重新注册换一张。
    """
    ns = unique_namespace("session")

    live = Live()
    live.thread.start()
    token = ""
    try:
        state = live.state()
        token = state.token
        state.put(ns, "k", b"v")
        assert state.get(ns, "k") == b"v"
        print(f"\n[注销前] token={token} 可用")
        # 自己造的数据自己清掉（注销之后就没有可用凭证了，只能在这里删）
        state.delete(ns, "k")
    finally:
        live.unregister()

    with dial(HUB_ADDR) as channel:
        stale = StateClient(channel)
        stale.set_token(token)
        with pytest.raises(grpc.RpcError) as err:
            stale.get(ns, "k")
        print(f"[注销后] 同一个 token → {err.value.code()}（{err.value.details()}）")
        assert err.value.code() == grpc.StatusCode.UNAUTHENTICATED


def test_插件日志里能看到凭证下发():
    """顺带钉住「注册成功 = 有凭证」这条链路，以及日志是能读的。"""
    with Live() as live:
        live.state()
        lines = [json.loads(line) for line in live.log_text().splitlines() if line.strip()]
        registered = [line for line in lines if line.get("msg") == "已注册到中台"]
        assert registered, live.log_text()
        assert not [line for line in lines if line.get("msg") == "中台未下发状态凭证，HubState 将不可用"]
        print(f"\n[日志] {json.dumps(registered[0], ensure_ascii=False)}")


def test_实例被摘除后重新注册并换新凭证():
    """真中台上验「401 → 重新注册 → 换新凭证 → 继续可用」这条自愈链。

    摘除用中台自己的 ``Unregister`` 触发（它做的事就是删掉实例行）——凭证明细挂在
    实例行上，行没了凭证立刻失效，这与心跳超时被摘除的后果一模一样，但不用等分钟级。
    插件侧对此**毫无察觉**，唯一的信号就是下一次状态调用的 401。
    """
    ns = unique_namespace("heal")
    live = Live(retry_interval=0.5)  # 也是 denial 触发的重注册的冷却窗口
    live.thread.start()
    try:
        state = live.state()
        old_token = state.token
        state.put(ns, "k", b"v")
        assert state.get(ns, "k") == b"v"
        print(f"\n[摘除前] token={old_token} 可用")

        time.sleep(0.7)  # 睡过冷却窗口，否则那次 denial 会被速率下限挡掉

        with dial(HUB_ADDR) as channel:
            # 摘除要**凭注册时下发的凭证**：中台凭它认属主，空凭证或不符一律拒绝、
            # 一个行都不删（不然撞了 instance_id 的另一个插件就能把这一行删掉）。
            # 这里拿的正是插件此刻手里那张，模拟「中台把这一行摘掉」。
            registry_pb2_grpc.PluginRegistryStub(channel).Unregister(
                registry_pb2.UnregisterRequest(
                    instance_id=live.instance_id,
                    reason="集成测试模拟摘除",
                    state_token=old_token,
                ),
                timeout=5,
            )
        print(f"[摘除] 中台已删掉实例 {live.instance_id}")

        # 旧凭证立即失效——插件侧看到的第一个信号就是这个 401
        with pytest.raises(grpc.RpcError) as err:
            state.get(ns, "k")
        assert err.value.code() == grpc.StatusCode.UNAUTHENTICATED
        print(f"[自愈] 旧凭证 → {err.value.code()}（{err.value.details()}）")

        # 骨架收到 401 → 重走注册 → 同一个客户端对象换上**新**凭证
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and state.token == old_token:
            time.sleep(0.05)
        print(f"[自愈] 重新注册后 token={state.token}")
        assert state.token != old_token, "收到 UNAUTHENTICATED 后应当重新注册换凭证"

        # 换完凭证插件就恢复了，插件作者什么都不用做
        state.put(ns, "after", b"healed")
        assert state.get(ns, "after") == b"healed"
        print("[自愈] 用新凭证继续读写成功")

        for entry in state.scan(ns, "", 100):
            state.delete(ns, entry.key)
    finally:
        live.unregister()
