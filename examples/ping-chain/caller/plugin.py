"""ping-caller —— 演示「插件间发现 + 互调」的一方。

``chain_ping`` 工具做三件事，每件都**经过中台**（A→hub→B，插件之间从不直连——
直连会绕过中台的鉴权、审计、熔断与限额）：

1. **发现**：经 ``ListPlugins`` 查在线插件清单，确认 ``ping-callee`` 在线；
2. **互调**：经 ``invoke_plugin`` 同步调用它的 ``echo`` 工具；
3. **回传**：把下游结果连同 trace 与调用链一起返回给最终调用方。

权限声明见 :meth:`PingCaller.manifest` 的 ``invokes``：互调是「我这个插件能调
谁」的授权名单，声明了它，中台在 ``HUB_PLUGIN_CALL_POLICY=declared`` 下按名单
放行；缺省的 ``allow`` 策略下不声明也能调，但声明让互调关系在注册表里可审计。
"""

from __future__ import annotations

import threading

from hubkit import (
    CALL_CHAIN_META,
    STRUCT_FQ_NAME,
    GatewayClient,
    InvokeFailure,
    Plugin,
    invalid,
    issue,
    payload_json,
    valid,
    with_payload_json,
)
from hubkit.proto.hubv1 import envelope_pb2, plugin_pb2

# 互调目标。真实业务里通常不写死：先用发现接口（ListPlugins / DescribeMessage /
# GetContract）按「谁提供这个能力」找，再调。这里写死是为了让示例一句话能讲清。
TARGET = "ping-callee"


class PingCaller(Plugin):
    """发现 ``ping-callee`` 并经中台调用它的 ``echo``。"""

    def __init__(self) -> None:
        # 网关客户端由骨架在**每次注册成功后**注入（见 set_gateway）。
        # 注入发生在注册循环线程，而 handle 跑在 gRPC 工作线程上，过锁与模板里
        # 对 state 客户端的处理是同一条理由。
        self._gateway: GatewayClient | None = None
        self._gateway_lock = threading.Lock()

    def set_gateway(self, gateway: GatewayClient) -> None:
        """骨架注入网关客户端。每次重新注册都会再注入一次（同一个对象，凭证在它内部轮换）。"""
        with self._gateway_lock:
            self._gateway = gateway

    def gateway(self) -> GatewayClient | None:
        """当前网关客户端；``None`` 表示还没注册成功过。"""
        with self._gateway_lock:
            return self._gateway

    def manifest(self) -> plugin_pb2.PluginManifest:
        """manifest 用代码构造（对齐 ``examples/auth-plugin`` 的 Go 做法）。

        与其他插件唯一的差别是 ``invokes``：它声明「本插件要调用哪些插件」。
        中台在 ``declared`` 策略下按这份名单放行互调；名单之外一律拒绝。
        """
        return plugin_pb2.PluginManifest(
            name="ping-caller",
            version="0.1.0",
            description="ping-chain 示例的上游：发现 ping-callee 并经中台互调它的 echo",
            consumes=[plugin_pb2.MessageContract(fq_name=STRUCT_FQ_NAME)],
            tools=[
                plugin_pb2.ToolDecl(
                    name="chain_ping",
                    description="先经中台发现 ping-callee，再经中台调用它的 echo，返回下游结果与 trace",
                    input_schema_json=(
                        '{"type":"object","properties":'
                        '{"text":{"type":"string","description":"要沿链传递的文本"}},'
                        '"required":["text"]}'
                    ),
                )
            ],
            # 插件互调的授权声明（PluginManifest.invokes，proto 字段 9）。
            invokes=[TARGET],
        )

    def validate(self, ctx, env: envelope_pb2.Envelope) -> plugin_pb2.ValidateResponse:
        payload, is_json = payload_json(env)
        if not is_json:
            return invalid(issue("payload", "需要 JSON 对象载荷"))
        text = payload.get("text")
        if not isinstance(text, str) or not text:
            return invalid(issue("payload.text", "缺少必填字段 text"))
        return valid()

    def handle(self, ctx, env: envelope_pb2.Envelope) -> envelope_pb2.Envelope:
        payload, _ = payload_json(env)
        text = (payload or {}).get("text") or ""

        gateway = self.gateway()
        if gateway is None:
            # 互调是本插件的**主业**而不是增强：客户端不在（注册没成功过）就明确
            # 失败，不做 fail-open——静默降级会把「链路没打通」伪装成业务结果。
            # 能走到这里说明中台拨通了这个实例，而注入先于任何调用，所以这基本
            # 只在「手工 new 插件类直接调 handle」时发生。
            raise RuntimeError("网关客户端尚未注入（注册还没成功过），无法做插件间调用")

        # ── 1. 发现 ─────────────────────────────────────────────────────
        # 缺省只列有健康实例的插件。查到不到就给出可行动的原因返回，而不是把
        # 「下游没起来」抛成插件异常——调用方能据此分辨「去启动 callee」。
        online = {summary.name: summary for summary in gateway.list_plugins()}
        if TARGET not in online:
            return with_payload_json(
                env,
                {
                    "ok": False,
                    "reason": f"{TARGET} 不在线（ListPlugins 没看到它）：确认它已启动并注册成功",
                },
            )

        # ── 2. 互调 ─────────────────────────────────────────────────────
        # current_envelope 传「我正在处理的信封」：SDK 复制 trace_id/run_id/node_id
        # 让整条链落进同一个 trace，并把 meta["hub.call_chain"] **原样**带上。
        # 不要自己往链里追加自己——链的语义是「已处理过该消息的插件序列」，
        # 追加 caller 发生在中台代调时（那时 caller 才真正「处理过」）。
        try:
            downstream = gateway.invoke_plugin(TARGET, {"text": text}, current_envelope=env)
        except InvokeFailure as err:
            # REJECTED（下游校验拒绝）/ ERROR（未声明授权、配额打满、成环、链深超限、
            # 下游出错）都是**业务结果**而不是 gRPC 错误：翻译成结构化返回，让调用方
            # 能按 outcome 分支。原样抛出去会让中台把这次调用记成插件故障，那是两回事。
            # 真正的基础设施故障（grpc.RpcError）不在这里捕获——那该让中台记账。
            return with_payload_json(
                env, {"ok": False, "outcome": err.outcome, "reason": err.reason}
            )

        reply, _ = payload_json(downstream)
        return with_payload_json(
            env,
            {
                "ok": True,
                "reply": reply,
                # 与 callee 返回里的 trace_id 一致——同一条 trace 的证据
                "trace_id": downstream.trace_id,
                # 中台代调时把 caller 追加进链，所以这里看到的是 "ping-caller"
                "call_chain": downstream.meta.get(CALL_CHAIN_META, ""),
            },
        )
