"""测试用的「两副面孔」中台：插件面 + 状态面。

``hubkit.mockhub`` 只实现插件面，**刻意不实现状态面**（见它的模块文档：给个假的
HubState 会让用到状态的插件在 mock 上跑绿、到真中台才发现写不进去）。所以这里另写
一个最小实现，而不是往 mockhub 里塞——

* 状态客户端的单测需要**状态面**（真实地走 metadata、走状态码、走超时）；
* 「凭证被拒 → 重新注册」那条链路需要**同一个进程里既有注册又有状态**，
  而且需要能按需触发拒绝（真中台要等凭证真的失效，等不到）。

复刻的语义以 ``crates/hub-grpc/src/state.rs`` 为准：键前缀、`found` 与空值的区分、
`deleted` 的语义、Scan 的前缀与上限。**键名规则在这里用独立写法实现**
（正则，不是 ``hubkit.valid_state_segment``）：测试替身要是复用了被测实现，
规则写错了会两边一起错，测了等于没测。
"""

from __future__ import annotations

import re
import threading
import time
from concurrent import futures
from dataclasses import dataclass

import grpc

from hubkit.proto.hubv1 import (
    registry_pb2,
    registry_pb2_grpc,
    state_pb2,
    state_pb2_grpc,
)

# 中台键前缀，与 crates/hub-grpc/src/state.rs 的 KEY_PREFIX 一致。
KEY_PREFIX = "hub:state"

# 与契约文件 sdk/go/hubkit/testdata/hub-rules.json 的 stateSegment 一致，
# 但**故意独立写成一行正则**（理由见模块文档）。
_SEGMENT = re.compile(r"[A-Za-z0-9_.-]{1,200}\Z")


@dataclass
class Options:
    """控制这个替身的行为。"""

    # 注册时下发的凭证。
    state_token: str = "state-token-1"

    # 每次注册换一张凭证（真中台就是这样：重复注册视为进程重启，旧凭证立即失效）。
    rotate_token: bool = False

    # 凭证照发、状态面的校验全拒（对应 Go mockhub 的 ``DenyStateToken``）。
    deny_state: bool = False

    # 状态面一律以这个状态码失败；用来验「非 401 的错误不该触发重新注册」。
    # 它的判定**在凭证校验之前**，所以和 deny_state 不冲突。
    state_error: grpc.StatusCode | None = None

    # 状态面每次调用先睡这么久；用来验客户端自己的超时。
    state_delay: float = 0.0

    # 非空时 Publish 一律回 accepted=False + 这条 reason（模拟中台的防环/配额拒绝），
    # 用来验「被拒的原因有没有原样暴露给调用方」。
    publish_reject_reason: str = ""

    # 中台指定的心跳周期（秒）。
    heartbeat_interval_seconds: int = 1

    # 大于 0 时，收到这么多拍心跳后开始要求重新注册。
    reregister_after_beats: int = 0


@dataclass
class StateCall:
    """记下来的一次状态请求，供断言用。"""

    method: str
    token: str
    namespace: str = ""
    key: str = ""
    prefix: str = ""
    limit: int = 0
    value: bytes = b""
    ttl_seconds: int = 0
    target: str = ""  # 仅 Publish：投递目标


class _Registry(registry_pb2_grpc.PluginRegistryServicer):
    def __init__(self, hub: "FakeHub") -> None:
        self._hub = hub

    def Register(self, request, context):
        return self._hub._on_register(request)

    def Heartbeat(self, request, context):
        return self._hub._on_heartbeat(request)

    def Unregister(self, request, context):
        return self._hub._on_unregister(request)


class _State(state_pb2_grpc.HubStateServicer):
    def __init__(self, hub: "FakeHub") -> None:
        self._hub = hub

    def KvGet(self, request, context):
        plugin = self._hub._authenticate(context)
        self._hub._record(
            StateCall(
                method="KvGet",
                token=self._hub._token_of(context),
                namespace=request.key.namespace,
                key=request.key.key,
            )
        )
        self._hub._check_segments(request.key.namespace, request.key.key)
        value = self._hub._store_get(plugin, request.key.namespace, request.key.key)
        return state_pb2.KvGetResponse(found=value is not None, value=value or b"")

    def KvPut(self, request, context):
        plugin = self._hub._authenticate(context)
        self._hub._record(
            StateCall(
                method="KvPut",
                token=self._hub._token_of(context),
                namespace=request.key.namespace,
                key=request.key.key,
                value=request.value,
                ttl_seconds=request.ttl_seconds,
            )
        )
        self._hub._check_segments(request.key.namespace, request.key.key)
        if len(request.value) > 1024 * 1024:
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, "value 超过上限")
        if request.ttl_seconds < 0:
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, "ttl_seconds 不能为负")
        self._hub._store_put(
            plugin, request.key.namespace, request.key.key, request.value, request.ttl_seconds
        )
        return state_pb2.KvPutResponse()

    def KvDelete(self, request, context):
        plugin = self._hub._authenticate(context)
        self._hub._record(
            StateCall(
                method="KvDelete",
                token=self._hub._token_of(context),
                namespace=request.key.namespace,
                key=request.key.key,
            )
        )
        self._hub._check_segments(request.key.namespace, request.key.key)
        return state_pb2.KvDeleteResponse(
            deleted=self._hub._store_delete(plugin, request.key.namespace, request.key.key)
        )

    def KvScan(self, request, context):
        plugin = self._hub._authenticate(context)
        self._hub._record(
            StateCall(
                method="KvScan",
                token=self._hub._token_of(context),
                namespace=request.namespace,
                prefix=request.prefix,
                limit=request.limit,
            )
        )
        if not _SEGMENT.match(request.namespace or ""):
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, "namespace 非法")
        if request.prefix and not _SEGMENT.match(request.prefix):
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, "prefix 非法")
        if not 1 <= request.limit <= 1000:
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, "limit 越界")
        entries = self._hub._store_scan(
            plugin, request.namespace, request.prefix, request.limit
        )
        return state_pb2.KvScanResponse(
            entries=[state_pb2.KvEntry(key=k, value=v) for k, v in entries]
        )

    def Publish(self, request, context):
        plugin = self._hub._authenticate(context)
        self._hub._record(
            StateCall(
                method="Publish",
                token=self._hub._token_of(context),
                target=request.target,
            )
        )
        if self._hub.opts.publish_reject_reason:
            return state_pb2.PublishResponse(
                accepted=False,
                reason=self._hub.opts.publish_reject_reason,
            )
        # run_id 只要「受理方给了个能查的凭据」这个语义；替身用递增号就够断言用
        with self._hub._lock:
            self._hub._run_seq += 1
            run_id = f"fake-run-{self._hub._run_seq}"
        del plugin  # 中台侧会覆盖信封 subject；替身不模拟投递，只认身份
        return state_pb2.PublishResponse(accepted=True, run_id=run_id)


class FakeHub:
    """运行中的替身。用法与 ``mockhub.Hub`` 一致（含上下文管理器）。"""

    def __init__(self, opts: Options | None = None) -> None:
        self.opts = opts or Options()
        if self.opts.heartbeat_interval_seconds <= 0:
            self.opts.heartbeat_interval_seconds = 1

        self._lock = threading.Lock()
        self._registrations: list = []
        # 记整个请求而不只是 instance_id：注销还要看凭证（理由同 mockhub）
        self._unregisters: list = []
        self._heartbeats = 0
        self._calls: list[StateCall] = []
        self._store: dict[str, tuple[bytes, float]] = {}
        self._plugin_of_token: dict[str, str] = {}
        self._run_seq = 0  # Publish 受理后发的 run_id 递增号

        self._server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
        registry_pb2_grpc.add_PluginRegistryServicer_to_server(_Registry(self), self._server)
        state_pb2_grpc.add_HubStateServicer_to_server(_State(self), self._server)
        port = self._server.add_insecure_port("127.0.0.1:0")
        if port == 0:
            raise RuntimeError("fake_hub: 监听失败")
        self.addr = f"http://127.0.0.1:{port}"
        self._server.start()

    # ------------------------------------------------------------ 生命周期

    def close(self) -> None:
        self._server.stop(grace=None)

    def __enter__(self) -> "FakeHub":
        return self

    def __exit__(self, *exc_info) -> None:
        self.close()

    # ------------------------------------------------------------ 断言用

    @property
    def registrations(self) -> list:
        with self._lock:
            return list(self._registrations)

    @property
    def registration_count(self) -> int:
        with self._lock:
            return len(self._registrations)

    @property
    def state_calls(self) -> list[StateCall]:
        with self._lock:
            return list(self._calls)

    @property
    def unregisters(self) -> list[str]:
        """收到的注销请求里的实例 id，**含被拒的**（与 ``mockhub.Hub`` 同名同义）。"""
        with self._lock:
            return [request.instance_id for request in self._unregisters]

    @property
    def unregister_tokens(self) -> list[str]:
        """与 :attr:`unregisters` 同序的注销凭证。"""
        with self._lock:
            return [request.state_token for request in self._unregisters]

    @property
    def current_token(self) -> str:
        with self._lock:
            return self._current_token_locked()

    def store_snapshot(self) -> dict[str, bytes]:
        """底层存储的原始内容（键含中台前缀）——用来验证前缀是**中台**拼的。"""
        with self._lock:
            return {k: v for k, (v, _expires) in self._store.items()}

    def wait_for_registration(self, count: int, timeout: float = 5.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.registration_count >= count:
                return
            time.sleep(0.01)
        raise TimeoutError(
            f"fake_hub: 等待第 {count} 次注册超时（已收到 {self.registration_count} 次）"
        )

    def wait_for_heartbeats(self, count: int, timeout: float = 5.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self._lock:
                if self._heartbeats >= count:
                    return
            time.sleep(0.01)
        raise TimeoutError("fake_hub: 等待心跳超时")

    # ------------------------------------------------------------ 服务端实现

    def _current_token_locked(self) -> str:
        if self.opts.rotate_token:
            return f"state-token-{len(self._registrations)}"
        return self.opts.state_token

    def _on_register(self, request):
        with self._lock:
            self._registrations.append(request)
            token = self._current_token_locked()
            if token:
                self._plugin_of_token[token] = request.plugin_name
        return registry_pb2.RegisterResponse(
            accepted=True,
            instance_id=request.instance_id,
            heartbeat_interval_seconds=self.opts.heartbeat_interval_seconds,
            state_token=token,
        )

    def _on_heartbeat(self, request):
        with self._lock:
            self._heartbeats += 1
            beats = self._heartbeats
        required = (
            self.opts.reregister_after_beats > 0 and beats > self.opts.reregister_after_beats
        )
        return registry_pb2.HeartbeatResponse(
            accepted=not required,
            heartbeat_interval_seconds=self.opts.heartbeat_interval_seconds,
            reregister_required=required,
        )

    def _on_unregister(self, request):
        with self._lock:
            self._unregisters.append(request)
            # 真中台的注销凭注册时下发的凭证认属主：**空凭证或不符一律不删任何行**，
            # 只有凭证对得上才摘掉实例——而摘掉之后凭证立即失效（凭证明细随实例行
            # 一起没了），所以这里清掉 token→插件名的映射。
            #
            # 替身比真中台宽松的话，本 SDK 的测试就测不出「撞了 instance_id 的一方
            # 退出会删掉对方那一行」这类行为（那正是中台这次修复的东西）。
            if request.state_token and self._plugin_of_token.get(request.state_token) is not None:
                self._plugin_of_token.clear()
        return registry_pb2.UnregisterResponse()

    # ------------------------------------------------------------ 状态面

    def _token_of(self, context) -> str:
        for key, value in context.invocation_metadata():
            if key == "x-hub-state-token":
                return value
        return ""

    def _authenticate(self, context) -> str:
        if self.opts.state_delay:
            time.sleep(self.opts.state_delay)
        if self.opts.state_error is not None:
            context.abort(self.opts.state_error, "状态面按配置失败")
        token = self._token_of(context)
        with self._lock:
            plugin = self._plugin_of_token.get(token)
        if plugin is None or self.opts.deny_state:
            context.abort(grpc.StatusCode.UNAUTHENTICATED, "状态凭证无效或已失效")
        return plugin

    def _record(self, call: StateCall) -> None:
        with self._lock:
            self._calls.append(call)

    def _check_segments(self, namespace: str, key: str) -> None:
        for field, text in (("namespace", namespace), ("key", key)):
            if not _SEGMENT.match(text or ""):
                raise AssertionError(f"fake_hub: 客户端送来了非法的 {field}={text!r}")

    def _full_key(self, plugin: str, namespace: str, key: str) -> str:
        return f"{KEY_PREFIX}:{plugin}:{namespace}:{key}"

    def _store_get(self, plugin: str, namespace: str, key: str) -> bytes | None:
        with self._lock:
            entry = self._store.get(self._full_key(plugin, namespace, key))
        if entry is None:
            return None
        value, expires = entry
        if expires and expires <= time.monotonic():
            return None
        return value

    def _store_put(
        self, plugin: str, namespace: str, key: str, value: bytes, ttl_seconds: int
    ) -> None:
        expires = time.monotonic() + ttl_seconds if ttl_seconds > 0 else 0.0
        with self._lock:
            self._store[self._full_key(plugin, namespace, key)] = (value, expires)

    def _store_delete(self, plugin: str, namespace: str, key: str) -> bool:
        with self._lock:
            return self._store.pop(self._full_key(plugin, namespace, key), None) is not None

    def _store_scan(
        self, plugin: str, namespace: str, prefix: str, limit: int
    ) -> list[tuple[str, bytes]]:
        head = f"{KEY_PREFIX}:{plugin}:{namespace}:{prefix}"
        head_without_prefix = f"{KEY_PREFIX}:{plugin}:{namespace}:"
        with self._lock:
            items = sorted(self._store.items())
        out: list[tuple[str, bytes]] = []
        for full, (value, expires) in items:
            if len(out) >= limit:
                break
            if not full.startswith(head):
                continue
            if expires and expires <= time.monotonic():
                continue
            out.append((full[len(head_without_prefix):], value))
        return out
