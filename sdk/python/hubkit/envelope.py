"""信封与载荷的读写助手。

与 ``sdk/go/hubkit/envelope.go`` 一一对应：同名的概念在这里也同名，
只有 Python 的命名习惯（下划线）不同——``PayloadJSON`` → :func:`payload_json`。
"""

from __future__ import annotations

import secrets
import time
from typing import Any

from google.protobuf import json_format, struct_pb2

from .proto.hubv1 import envelope_pb2, plugin_pb2

# 「直接调用」载荷的类型标识。
#
# agent 经 MCP、外部系统经 HTTP 调用插件时，中台把 JSON 对象包成
# google.protobuf.Struct 放进信封；flow 内部传递的则是业务类型。
# 插件两种都可能收到，用 :func:`payload_json` 区分。
STRUCT_TYPE_URL = "type.googleapis.com/google.protobuf.Struct"

# 上面那个载荷的全限定消息名。
#
# 在 manifest 里声明 ``consumes=[MessageContract(fq_name=STRUCT_FQ_NAME)]``
# 表示「本插件接受直接调用的 JSON 载荷」。它是 well-known 类型，中台不要求它
# 出现在插件自己的 descriptor 里。
STRUCT_FQ_NAME = "google.protobuf.Struct"

# ULID 的字符表（Crockford Base32：剔除 I/L/O/U，避免手抄时与 1/0 混淆）。
_ULID_ENCODING = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


def new_ulid() -> str:
    """生成一个 ULID：48 位毫秒时间戳 + 80 位随机数，Crockford Base32 编码的 26 字符。

    ``message_id`` 与 ``trace_id`` 都用它。选 ULID 而不是 uuid4 的理由与中台一致
    （中台用 ``ulid`` crate 生成 trace）：时间部分在前，同一批消息按 id 排序就是
    按发生顺序排，排查时能直接从 id 看出先后；uuid4 完全无序，只能全量翻日志。

    用标准库手写而不是引第三方 ULID 包：SDK 的运行时依赖面（``grpcio``/``protobuf``
    两个）是有意收着的，为生成一个 128 位整数多拉一个包不值当。算法本身只有十几行，
    且有测试钉住形状（26 字符、字符表、同毫秒不撞）。
    """
    ms = time.time_ns() // 1_000_000
    value = ((ms & ((1 << 48) - 1)) << 80) | secrets.randbits(80)
    # 128 位 = 26 个 5 位段（最高段只用到 3 位），从高位往低位编
    return "".join(_ULID_ENCODING[(value >> shift) & 0x1F] for shift in range(125, -1, -5))


def is_well_known_fq_name(fq_name: str) -> bool:
    """判断是否属于 protobuf 平台提供的 well-known 类型。

    中台对 ``google.protobuf.*`` 豁免「声明必须出现在自己的 descriptor 里」这条检查；
    插件侧的自测套件用同一个判断，避免两边规则漂移。
    """
    return (fq_name or "").startswith("google.protobuf.")


def payload_json(
    env: envelope_pb2.Envelope | None,
) -> tuple[dict[str, Any] | None, bool]:
    """取出信封里的 JSON 载荷，返回 ``(载荷, 是不是 JSON 载荷)``。

    第二个值回答的是**「载荷是什么类型」而不是「载荷有没有内容」**——这两件事
    混在一起会出大问题：载荷是 ``google.protobuf.Struct`` 但一个字段都没设时返回
    ``({}, True)``，flow 内部传的业务类型（例如 ``wms.v1.OrderCreated``）返回
    ``(None, False)``。只凭 ``载荷 == {}`` 判断的话，**「这次调用真的没有输入」
    会被当成「载荷不是 JSON，去按业务类型解」**，于是插件拿着一个业务消息去当
    JSON 解，得到的是一堆字节被塞进 Struct 字段的怪东西，而不是一个明确的失败。

    所以载荷不是 Struct 时插件应改为按自己的业务类型去 ``ParseFromString``，
    判据是第二个值，不是载荷本身。

    注意数值一律是 float——google.protobuf.Struct 只有一种数值类型。
    大单号这类超出 2^53 的整数请用字符串承载，别指望 JSON 数字。
    """
    if env is None:
        return None, False
    payload = env.payload
    if not payload.type_url or payload.type_url != STRUCT_TYPE_URL:
        return None, False

    message = struct_pb2.Struct()
    try:
        message.ParseFromString(payload.value)
    except Exception:
        return None, False
    return struct_to_plain(message), True


def struct_to_plain(message: struct_pb2.Struct) -> dict[str, Any]:
    """把 Struct 摊成普通的 dict/list/标量。

    不用 ``json_format.MessageToDict``：那条路会把 64 位整数转成字符串、
    对 ``google.protobuf.Value`` 的 null 处理也随版本变过。这里手写一台转换器，
    行为只由我们自己决定——插件拿到的东西不该随 protobuf 版本升级而变样。
    """
    out: dict[str, Any] = {}
    for key, value in message.fields.items():
        out[key] = _value_to_plain(value)
    return out


def _value_to_plain(value: struct_pb2.Value) -> Any:
    kind = value.WhichOneof("kind")
    if kind == "null_value":
        return None
    if kind == "number_value":
        return value.number_value
    if kind == "string_value":
        return value.string_value
    if kind == "bool_value":
        return value.bool_value
    if kind == "struct_value":
        return struct_to_plain(value.struct_value)
    if kind == "list_value":
        return [_value_to_plain(item) for item in value.list_value.values]
    # kind is None：Value 一个字段都没设，等价于 null
    return None


def with_payload_json(
    env: envelope_pb2.Envelope | None, payload: dict[str, Any]
) -> envelope_pb2.Envelope:
    """把 JSON 对象装进信封的载荷，返回**新**信封。

    返回新信封而不是就地改：链路里可能有别的持有者，就地改会让「我在 Handle 里
    换了个载荷」变成对上游可见的副作用。

    载荷不是合法 JSON 对象时抛 ``ValueError``——这是编程错误（插件自己构造的输出），
    不是运行时数据问题，静默吞掉只会让中台收到一个空信封。
    """
    message = struct_pb2.Struct()
    try:
        json_format.ParseDict(payload, message)
    except Exception as exc:
        raise ValueError(f"hubkit: 载荷不是合法的 JSON 对象: {exc}") from exc

    out = clone_envelope(env)
    out.payload.Pack(message)
    return out


def with_payload(env: envelope_pb2.Envelope | None, message: Any) -> envelope_pb2.Envelope:
    """把业务类型装进信封的载荷（flow 内部传递用）。"""
    out = clone_envelope(env)
    out.payload.Pack(message)
    return out


def clone_envelope(env: envelope_pb2.Envelope | None) -> envelope_pb2.Envelope:
    """深拷贝一份信封；``None`` 得到空信封。"""
    out = envelope_pb2.Envelope()
    if env is not None:
        out.CopyFrom(env)
    return out


def deadline(env: envelope_pb2.Envelope | None) -> float | None:
    """返回信封的绝对截止时间（Unix 秒）。未设置时返回 ``None``。

    deadline 逐跳递减：插件应据此提前放弃，而不是把时间耗光后让上层的超时兜底。
    """
    ms = env.deadline_ms if env is not None else 0
    if not ms or ms <= 0:
        return None
    return ms / 1000.0


def budget(env: envelope_pb2.Envelope | None) -> float | None:
    """返回距离截止时间还剩多少秒。未设置时 ``None``；已过期时返回 0.0。"""
    at = deadline(env)
    if at is None:
        return None
    return max(0.0, at - time.time())


def expired(env: envelope_pb2.Envelope | None) -> bool:
    """判断信封是否已过截止时间。"""
    left = budget(env)
    return left is not None and left == 0.0


def valid() -> plugin_pb2.ValidateResponse:
    """构造「校验通过」的响应。"""
    return plugin_pb2.ValidateResponse(valid=True)


def invalid(*issues: plugin_pb2.ValidationIssue) -> plugin_pb2.ValidateResponse:
    """构造「校验不通过」的响应。

    每个 issue 的 path 要能定位到具体字段（例如 ``payload.items[2].sku``），
    中台会原样把它回给调用方，agent 靠它改数据重试。
    """
    return plugin_pb2.ValidateResponse(valid=False, issues=list(issues))


def issue(path: str, message: str) -> plugin_pb2.ValidationIssue:
    """构造一条错误级校验问题。"""
    return plugin_pb2.ValidationIssue(
        path=path, message=message, severity=plugin_pb2.SEVERITY_ERROR
    )


def warning(path: str, message: str) -> plugin_pb2.ValidationIssue:
    """构造一条警告级校验问题。

    警告不会让校验失败——用它标记「能放行但值得记一笔」的情况。
    """
    return plugin_pb2.ValidationIssue(
        path=path, message=message, severity=plugin_pb2.SEVERITY_WARNING
    )
