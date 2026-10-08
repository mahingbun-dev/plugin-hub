"""ping-callee —— ping-chain 示例里**被调用**的一方。

它是一个普通插件：既能被 agent（MCP）或外部系统（HTTP ingress）直接调用，
也能被**别的插件**经中台的 PluginGateway 调用（发起方见 ``../caller``）。

注意插件侧对「被别的插件调」**不需要做任何事**：调用方是谁、链上有没有成环、
一分钟调了多少次，全由中台在代调时治理——下游拿到的信封与其他来源没有区别。
这也是「插件之间不直连」这套设计的意图：治理收在中台一处，插件只管业务。
"""

from __future__ import annotations

from hubkit import (
    STRUCT_FQ_NAME,
    Plugin,
    invalid,
    issue,
    payload_json,
    valid,
    with_payload_json,
)
from hubkit.proto.hubv1 import envelope_pb2, plugin_pb2


class PingCallee(Plugin):
    """回声插件：把 ``text`` 原样回显，并带回本条消息的 ``trace_id``。"""

    def manifest(self) -> plugin_pb2.PluginManifest:
        """声明插件的身份、契约与暴露给 agent 的能力。

        同一版本号的 manifest 不可变更，改了请升版本号。
        """
        return plugin_pb2.PluginManifest(
            name="ping-callee",
            version="0.1.0",
            description="ping-chain 示例的下游：把 text 原样回显（echo）",
            # 进出都是 JSON 载荷（google.protobuf.Struct）。它是 well-known 类型，
            # 不必出现在自己的 proto 里，descriptor() 走缺省的空即可。
            consumes=[plugin_pb2.MessageContract(fq_name=STRUCT_FQ_NAME)],
            tools=[
                plugin_pb2.ToolDecl(
                    name="echo",
                    description="回显 text —— ping-chain 示例里被 ping-caller 经中台互调的工具",
                    input_schema_json=(
                        '{"type":"object","properties":'
                        '{"text":{"type":"string","description":"要回显的文本"}},'
                        '"required":["text"]}'
                    ),
                )
            ],
        )

    def validate(self, ctx, env: envelope_pb2.Envelope) -> plugin_pb2.ValidateResponse:
        """校验规则：JSON 载荷、``text`` 必填。被互调时同样先过这里——

        中台对「插件发起的调用」复用与直调同一条 Invoker 链路，Validate 照跑；
        拒绝时 issues 会原样回到发起方的 ``InvokeRejected`` 异常里。
        """
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

        # 把 trace_id 带回载荷：caller 会把它转给最终调用方。用它对账
        # 「caller 与 callee 处理的是同一条 trace」——互调的 trace 贯通
        # 是这个示例要演示的事，得让人看得见。
        return with_payload_json(env, {"echo": text, "trace_id": env.trace_id})
