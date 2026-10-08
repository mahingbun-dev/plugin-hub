"""SDK 自身的测试。

这些测试**不是**给插件作者的模板测试——插件工程里那份是 ``plugin_test.py``，
只跑 L1 契约自检。这里跑的是 SDK 的行为：注册、心跳、被摘除后自愈、优雅退出。
"""

from __future__ import annotations

from hubkit import Plugin, STRUCT_FQ_NAME, invalid, issue, payload_json, valid, with_payload_json
from hubkit.proto.hubv1 import envelope_pb2, plugin_pb2


class EchoPlugin(Plugin):
    """一个最小但完整、且与 ``templates/plugin/plugin.py.tmpl`` 同形的插件。

    刻意**不**从模板文件里 import：模板是要被替换占位符的文本，测试去读它会变成
    「测试模板渲染」而不是「测试 SDK 行为」。模板自己那份测试在生成出来的工程里。
    """

    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(
            name="echo",
            version="0.1.0",
            description="测试用的回显插件",
            consumes=[plugin_pb2.MessageContract(fq_name=STRUCT_FQ_NAME)],
            tools=[plugin_pb2.ToolDecl(name="echo", description="回显")],
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
        return with_payload_json(env, {"echo": f"echo 收到: {text}", "length": len(text)})
