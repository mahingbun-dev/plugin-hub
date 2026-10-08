"""中台外置状态（HubState）的客户端。

插件被**强制无状态**——实例内存不保证跨调用保留，因此热切换零代价、水平扩展无障碍、
重放与测试都简单。所有需要跨调用保留的东西都走这里。

契约在 ``crates/hub-proto/proto/hub/v1/state.proto``；本文件与
``sdk/go/hubkit/state.go`` **行为对齐**（方法集、凭证处理、被拒时的反应），
只是按 Python 的习惯落成同步 API：

===============================  ==========================================
Go                               Python
===============================  ==========================================
``hubkit.StateAware`` 接口       :meth:`hubkit.Plugin.set_state` 钩子
``hubkit.StateClient``           :class:`hubkit.StateClient`
``hubkit.StateEntry``            :class:`hubkit.StateEntry`
``Get`` / ``Put`` / ``Delete``   :meth:`StateClient.get` / :meth:`~StateClient.put` / :meth:`~StateClient.delete`
``Scan``                         :meth:`StateClient.scan`
``hubkit.StateTokenMetadata``    :data:`hubkit.STATE_TOKEN_METADATA`
===============================  ==========================================

三条必须知道的事：

* **凭证由骨架注入，插件作者不管 token**。注册回执里的
  ``RegisterResponse.state_token`` 由 ``run`` 在注册成功后交给这里，之后每个状态
  请求都带在 gRPC metadata 的 ``x-hub-state-token`` 上（字段名的事实来源是
  ``sdk/go/hubkit/testdata/hub-rules.json`` 的 ``stateTokenMetadata``）。
* **中台按凭证反查插件名并强制加前缀 ``hub:state:{插件名}:``**，插件自报的
  ``namespace`` 只是子空间，前缀由中台拼——客户端不拼，也拼不了。
* **键前缀不含版本号**：同一插件的所有版本**共用一个状态空间**。升版本不会清空
  状态（对登录缓存这类状态正是要的），但两个版本往同一个 namespace 写就是**互相
  覆盖**。要按版本隔离，请自己把版本号写进 namespace。

**Publish 已封装**（:meth:`StateClient.publish`）：中台侧的防环、链长上限 8、
每插件每分钟 60 次配额照常生效，并把 `subject` 无条件覆盖成调用插件的身份。
被拒（`accepted=False`）时原因在 ``PublishReceipt.reason`` 里——它是**返回值**
而不是异常：防环拒绝是调用方该分支处理的常规结果，不是故障，包成异常反而会诱导
调用方写「抓到就算失败重试」的逻辑。

**隔离强度不是承诺**：凭证只证明注册方自称是某个插件名，不证明它就是那个插件
（插件面按设计不鉴权）。见 proto 文件头与 ``docs/design.md`` 风险 11。
"""

from __future__ import annotations

import threading
from dataclasses import dataclass

import grpc

from .config import STATE_CALL_TIMEOUT
from .proto.hubv1 import envelope_pb2, state_pb2, state_pb2_grpc
from .rules import valid_state_segment

# 状态凭证的 metadata 键。必须与中台侧一致
# （``crates/hub-grpc/src/state.rs`` 的 ``STATE_TOKEN_METADATA``）。
#
# 导出它而不是让各处各写一份字面量：写错的话中台会判成「无凭证」，而插件侧只会
# 看到一个 401，很难查。
STATE_TOKEN_METADATA = "x-hub-state-token"

# 中台侧的上限，数值取自中台的 ``crate::state``（``crates/hub-core/src/state.rs``
# 的 ``MAX_VALUE_BYTES`` / ``MAX_SCAN_LIMIT``，由 ``crates/hub-grpc/src/state.rs``
# re-export 给服务端与 mock 中台共用）——**不要在这里另编一个数**。
#
# 客户端拿它们做**本地预检**：中台对同样的输入回的是 ``INVALID_ARGUMENT`` 加一句
# 服务端视角的描述，而超限的值还可能先在网络上被序列化/传输一遍。本地先拦一道，
# 错误信息能直接指到调用点，也不产生任何往返。
#
# 与 ``rules.valid_state_segment`` 一样，这几个数与中台之间**没有自动化的漂移检测**
# （中台的常量是私有的，不进那份跨语言契约文件）。中台调了上限而这里没跟上，
# 表现是「本地放行、中台回 INVALID_ARGUMENT」——那种错总能到中台，不会静默写坏数据。
MAX_VALUE_BYTES = 1024 * 1024
MAX_SCAN_LIMIT = 1000


class DenialSignal:
    """「状态凭证被中台拒了」的一次性信号，由注册循环消费。

    与 Go 的 ``make(chan struct{}, 1)`` 等价：**缓冲 1 + 非阻塞发送**——并发调用
    一起撞上 401 时只留一个信号，不会把注册循环叫成风暴。

    用条件变量而不是 :class:`threading.Event`：注册循环要在「下一拍心跳」与「这个
    信号」之间选一个先到的，条件变量能带超时地等（``Event.wait`` 分不清「被唤起」
    与「超时」，用它就得自己去比时间）。

    插件作者不必碰它——它由骨架创建并注入给 :class:`StateClient`。
    """

    def __init__(self) -> None:
        self._cond = threading.Condition()
        self._pending = False

    def notify(self) -> None:
        """置位。已有待处理信号时是空操作（等价于向满的 channel 非阻塞发送）。"""
        with self._cond:
            if self._pending:
                return
            self._pending = True
            self._cond.notify_all()

    def wait(self, timeout: float | None = None) -> bool:
        """等到信号到来或超时；返回 True 表示有信号待处理。**不消费**信号。"""
        with self._cond:
            return self._cond.wait_for(lambda: self._pending, timeout)

    def consume(self) -> bool:
        """取走信号；返回 True 表示这次确实取到了一个。"""
        with self._cond:
            pending, self._pending = self._pending, False
            return pending

    @property
    def pending(self) -> bool:
        with self._cond:
            return self._pending


@dataclass(frozen=True)
class StateEntry:
    """一次 :meth:`StateClient.scan` 返回的一项（对应 Go 的 ``StateEntry``）。

    ``key`` 是**不带中台前缀**的键名——前缀里含插件名，插件看不到也不需要看到。
    """

    key: str
    value: bytes


@dataclass(frozen=True)
class PublishReceipt:
    """一次 :meth:`StateClient.publish` 的回执（对应 proto 的 ``PublishResponse``）。

    ``accepted=False`` 时 ``reason`` 说明被拒原因（检测到环、配额打满等），调用方
    照它改逻辑——所以它是返回值的字段而不是异常：这是中台的**业务结果**，与
    网络故障（``grpc.RpcError``）是两类东西。
    """

    accepted: bool
    # 受理后产生的执行 id，供后续查链路；未受理时为空串
    run_id: str
    # 未受理时的原因；受理时为空串
    reason: str


def _check_segment(field: str, text: str) -> None:
    """本地预检一个键名片段，把中台那句 ``INVALID_ARGUMENT`` 换成能直接照做的错。"""
    if valid_state_segment(text):
        return
    detail = "为空" if not text else f"含有 [A-Za-z0-9_.-] 之外的字符：{text!r}"
    raise ValueError(
        f"hubkit: HubState 的 {field} 非法（{detail}）。"
        "namespace / key / prefix 只允许 [A-Za-z0-9_.-]，非空且不超过 200 字节"
        "（中台同样会拒，本地先拦是为了让错误出现在调用点，见 hubkit.valid_state_segment）"
    )


class StateClient:
    """中台外置状态（HubState）的客户端。

    由 ``run`` 在**注册成功后**注入给实现了 :meth:`hubkit.Plugin.set_state` 的插件
    ——插件作者不该自己构造它，凭证只有中台知道。

    凭证会被重新注册轮换（中台重启、实例被摘除后自愈），而注册循环跑在另一个线程上，
    所以凭证的读写都过锁。

    出错时抛 :class:`grpc.RpcError`（**原样**抛，不包装成别的类型）：插件侧可以看
    ``err.code()`` 决定是 fail-open 还是上报。参数本身不合法时抛 :class:`ValueError`
    ——那是调用点的 bug，不是运行时故障，用异常类型把两者分开。
    """

    def __init__(
        self,
        channel: grpc.Channel,
        *,
        call_timeout: float = STATE_CALL_TIMEOUT,
        denied: DenialSignal | None = None,
    ) -> None:
        """
        :param channel: 与中台的连接。注册、心跳、状态**共用同一条**（见 ``run``）。
        :param call_timeout: 单次调用的时间上限（秒）；``<= 0`` 表示不设上限。
            超时产生的是 ``DEADLINE_EXCEEDED`` 而不是 ``UNAUTHENTICATED``，所以不会
            误触发重新注册——中台的一次卡顿不该把注册循环叫醒。
        :param denied: 凭证被拒时的信号出口；为 ``None``（测试里自造的客户端）时
            静默丢弃，与 Go 的「denied 为 nil 时不发」一致。
        """
        self._stub = state_pb2_grpc.HubStateStub(channel)
        self._call_timeout = call_timeout if call_timeout and call_timeout > 0 else None
        self._denied = denied
        self._mu = threading.Lock()
        self._token = ""

    # ------------------------------------------------------------ 凭证

    def set_token(self, token: str) -> None:
        """换上新凭证。**骨架用**（注册成功后调用），插件作者不用管。

        每次注册都会轮换，所以这里是覆盖而不是"只在为空时写入"。

        这张凭证同时也是**注销凭证**（``Registrar.unregister`` 直接读 :attr:`token`
        发给中台）：「状态凭证」与「注销凭证」是同一个东西——注册时下发的那一份
        就是这一行的属主证明。所以插件别去调它，改坏了连注销都会失败。
        """
        with self._mu:
            self._token = token

    @property
    def token(self) -> str:
        """当前凭证。空串表示中台还没下发（此时调用必然被回 UNAUTHENTICATED）。"""
        with self._mu:
            return self._token

    # ------------------------------------------------------------ 读写

    def get(self, namespace: str, key: str) -> bytes | None:
        """读取一个键；**返回 None 表示键不存在**。

        对应 Go 的 ``Get`` 返回的 ``(value, found)``：Go 用 ``found`` 区分「键不存在」
        与「值是空字节」，Python 里 ``None`` 与 ``b""`` 天然是两回事，所以直接返回
        ``bytes | None``，不必让调用方多解一个元组。
        """
        _check_segment("namespace", namespace)
        _check_segment("key", key)

        response = self._rpc(
            self._stub.KvGet,
            state_pb2.KvGetRequest(key=state_pb2.KvKey(namespace=namespace, key=key)),
        )
        # found 与「值是空字节」是两回事：前者是键在不在，后者是值本身为空
        return response.value if response.found else None

    def put(
        self,
        namespace: str,
        key: str,
        value: bytes | bytearray | memoryview | str,
        ttl_seconds: int | float = 0,
    ) -> None:
        """写入一个键。``ttl_seconds`` 为 0 表示**不过期**。

        ``value`` 收 ``str`` 时按 UTF-8 编码——线上是字节，没有别的合理解释。

        **TTL 的粒度是秒**（线协议是 ``ttl_seconds``，中台按整数秒写 Redis）。
        不足 1 秒的值在这里直接报错，而不是像 Go 那样静默截断成 0：截断的后果是
        「永不过期」，与调用方的意图正好相反，而错误要到很久以后才看得出来。
        """
        _check_segment("namespace", namespace)
        _check_segment("key", key)

        payload = value.encode("utf-8") if isinstance(value, str) else bytes(value)
        if len(payload) > MAX_VALUE_BYTES:
            raise ValueError(
                f"hubkit: HubState 单值上限 {MAX_VALUE_BYTES} 字节，收到 {len(payload)} 字节"
                "——HubState 不是对象存储（中台会回 INVALID_ARGUMENT，这里先拦）"
            )

        try:
            ttl = int(ttl_seconds or 0)
        except (TypeError, ValueError) as err:
            raise ValueError(
                f"hubkit: ttl_seconds 必须是整数秒，收到 {ttl_seconds!r}"
            ) from err
        if ttl != (ttl_seconds or 0):
            raise ValueError(
                f"hubkit: ttl_seconds 只接受整数秒，收到 {ttl_seconds!r}。"
                "线协议以秒为粒度，非整数值会被静默截断（Go 侧截断成 0 = 永不过期，"
                "与调用方的意图正好相反），所以这里直接报错"
            )
        if ttl < 0:
            raise ValueError(f"hubkit: ttl_seconds 不能为负，收到 {ttl}")

        self._rpc(
            self._stub.KvPut,
            state_pb2.KvPutRequest(
                key=state_pb2.KvKey(namespace=namespace, key=key),
                value=payload,
                ttl_seconds=ttl,
            ),
        )

    def delete(self, namespace: str, key: str) -> bool:
        """删除一个键。删不存在的键返回 ``False``，**不是**错误。

        对应 Go 的 ``Delete``：调用方的意图（这键没了）已经达成，把它当失败会逼着
        每个调用方都去写一遍「先 Get 再 Delete」。
        """
        _check_segment("namespace", namespace)
        _check_segment("key", key)

        response = self._rpc(
            self._stub.KvDelete,
            state_pb2.KvDeleteRequest(key=state_pb2.KvKey(namespace=namespace, key=key)),
        )
        return bool(response.deleted)

    def scan(self, namespace: str, prefix: str = "", limit: int = 100) -> list[StateEntry]:
        """扫描一个命名空间下前缀匹配的键，最多 ``limit`` 条。

        ``prefix`` 可以为空（扫整个命名空间）。``limit`` 必须在 ``1..=MAX_SCAN_LIMIT``
        之间——中台对 ``limit=0`` 也回错误，不给它一个「默认全扫」的含义。

        缺省 100 而不是上限 1000：一次扫描会把每个键的**值**一并取回（中台如此，
        免得调用方再查一遍），拿满 1000 条可能已经是可观的数据量。
        """
        _check_segment("namespace", namespace)
        if prefix:
            _check_segment("prefix", prefix)
        if not isinstance(limit, int) or isinstance(limit, bool):
            raise ValueError(f"hubkit: limit 必须是整数，收到 {limit!r}")
        if not 1 <= limit <= MAX_SCAN_LIMIT:
            raise ValueError(
                f"hubkit: limit 必须在 1..{MAX_SCAN_LIMIT} 之间，收到 {limit}"
                "（中台对 0 与超限都回 INVALID_ARGUMENT，不限量扫描会把整个命名空间拉走）"
            )

        response = self._rpc(
            self._stub.KvScan,
            state_pb2.KvScanRequest(namespace=namespace, prefix=prefix, limit=limit),
        )
        return [StateEntry(key=entry.key, value=entry.value) for entry in response.entries]

    def publish(self, target: str, envelope: envelope_pb2.Envelope) -> PublishReceipt:
        """把一条信封投递给目标 flow（``HubState.Publish``），返回受理回执。

        这是**异步**触发：中台受理即返回，拿不到下游的处理结果——需要结果的同步
        互调走 :meth:`hubkit.GatewayClient.invoke_plugin`，两者的分工见
        ``crates/hub-proto/proto/hub/v1/gateway.proto`` 的文件头。

        信封由调用方构造（载荷用 :func:`hubkit.with_payload_json` 装是常规做法）；
        ``subject`` 会被中台无条件覆盖为调用插件的身份，自报无效。被拒时
        （``receipt.accepted is False``）原因在 ``receipt.reason``——防环与配额
        拒绝是业务结果，不是异常，理由见 :class:`PublishReceipt`。
        """
        response = self._rpc(
            self._stub.Publish,
            state_pb2.PublishRequest(target=target, envelope=envelope),
        )
        return PublishReceipt(
            accepted=response.accepted,
            run_id=response.run_id,
            reason=response.reason,
        )

    # ------------------------------------------------------------ 内部

    def _rpc(self, method, request):
        """发一次状态 RPC：统一带上凭证与超时，并把「被拒」翻译成信号。

        超时用 ``grpc`` 自己的 deadline（``timeout=``），而不是手搓一个计时器：
        它管的是「这次调用」的全过程，且超时到了会取消在途请求，不会留下悬挂的
        连接状态。Go 那边是把父 ctx 的 deadline 与 StateCallTimeout 取更早者——
        Python 的插件接口没有 ctx（``handle`` 不接 context），所以这里就是一个
        固定上限，没有"父 deadline 更早"的情形。
        """
        try:
            return method(
                request,
                timeout=self._call_timeout,
                metadata=((STATE_TOKEN_METADATA, self.token),),
            )
        except grpc.RpcError as err:
            self._note_denied(err)
            raise

    def _note_denied(self, err: grpc.RpcError) -> None:
        """凭证被中台拒绝时，叫醒注册循环去换一张新凭证。

        只认 ``UNAUTHENTICATED``：其它错误（网络抖动、参数非法）与凭证无关，重注册
        解决不了，反而会让循环空转。错误仍原样返回给调用方——重注册是后台的补救，
        不该掩盖这一次失败。

        为什么会有这个错误：中台重启（凭证换代）、实例被摘除（旧凭证随行一起没了）、
        或者重复注册把上一张凭证顶掉。心跳此时可能一切正常，不重注册就会一直哑下去。
        """
        if self._denied is None:
            return
        if err.code() != grpc.StatusCode.UNAUTHENTICATED:
            return
        self._denied.notify()


__all__ = [
    "DenialSignal",
    "MAX_SCAN_LIMIT",
    "MAX_VALUE_BYTES",
    "PublishReceipt",
    "STATE_TOKEN_METADATA",
    "StateClient",
    "StateEntry",
]
