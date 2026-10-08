"""JSON 日志的形状。

这不是审美问题：中台的接入指南（``templates/_shared/onboarding.md``）里**引用了**
这个形状的日志行，两门语言的插件打出来的东西不一样的话，照着指南排查的人会以为
自己看错了行。所以字段名逐个钉住。
"""

from __future__ import annotations

import io
import json

from hubkit.log import DEBUG, ERROR, INFO, WARN, JsonLogger, level_from_env


def _lines(stream: io.StringIO) -> list[dict]:
    return [json.loads(line) for line in stream.getvalue().splitlines() if line.strip()]


def test_固定字段与业务字段平铺在一起():
    stream = io.StringIO()
    JsonLogger(stream=stream).info("已注册到中台", plugin="order-reader", version="0.1.0")

    record = _lines(stream)[0]
    assert record["level"] == "INFO"
    assert record["msg"] == "已注册到中台"
    assert record["plugin"] == "order-reader"
    assert record["version"] == "0.1.0"
    assert "time" in record

    # 字段顺序稳定：一条日志的三个固定字段总在同一列，grep/awk 才有用
    assert list(record)[:3] == ["time", "level", "msg"]


def test_一行一条():
    """一行一条、不换行不缩进——接入指南里引用的就是这种可直接 grep 的形状。"""
    stream = io.StringIO()
    logger = JsonLogger(stream=stream)
    logger.info("a")
    logger.info("b")

    raw = stream.getvalue()
    assert raw.count("\n") == 2
    assert len(_lines(stream)) == 2


def test_拒绝原因逐条打而不是糊成一段():
    """中台拒绝时给的是结构化原因。整段多行文本被转义成 \\n 塞进单个字段之后，
    一行里糊着 N 条原因，得靠人脑反解析才看得出哪条是哪条。
    """
    stream = io.StringIO()
    JsonLogger(stream=stream).error(
        "中台拒绝了注册",
        code="BREAKING_CHANGE",
        message="相对版本 1.0.0 存在破坏性契约变更",
        detail="wms.v1.Order.sku 已被删除",
    )

    record = _lines(stream)[0]
    assert record["code"] == "BREAKING_CHANGE"
    assert record["detail"] == "wms.v1.Order.sku 已被删除"
    # 结构化字段各占一个 JSON 键、整条消息只占一行——被转义成 \n 塞进单个字段的话，
    # 一行里会糊着 N 条原因，读日志得靠人脑反解析
    assert len(_lines(stream)) == 1
    assert stream.getvalue().count("\n") == 1, "除了行尾的换行，消息里不该再有换行"


def test_中文不被转义():
    """ensure_ascii=False：日志是给人看的，``\\u5df2\\u6ce8\\u518c`` 没人读得出来。"""
    stream = io.StringIO()
    JsonLogger(stream=stream).info("已注册到中台")

    assert "已注册到中台" in stream.getvalue()
    assert "\\u" not in stream.getvalue()


def test_级别过滤():
    stream = io.StringIO()
    logger = JsonLogger(level=WARN, stream=stream)
    logger.debug("d")
    logger.info("i")
    logger.warn("w")
    logger.error("e")

    assert [line["level"] for line in _lines(stream)] == [WARN, ERROR]


def test_级别名与环境变量():
    # warning 也收：Python 用惯了 logging 的人会这么写
    assert level_from_env("debug") == DEBUG
    assert level_from_env("WARNING") == WARN
    assert level_from_env("error") == ERROR
    assert level_from_env("") == INFO
    assert level_from_env("没这个级别") == INFO


def test_child带上恒定字段():
    """把 plugin 这类恒定字段绑一次——漏传一次就会让那条日志没法按插件名检索。"""
    stream = io.StringIO()
    logger = JsonLogger(stream=stream).child(plugin="order-reader")
    logger.info("已注册到中台", instance="i-1")

    record = _lines(stream)[0]
    assert record["plugin"] == "order-reader"
    assert record["instance"] == "i-1"


def test_打不出来的字段不会把插件搞挂():
    """日志本身绝不能把插件搞挂：退化成只打固定字段，而不是把异常抛进调用栈。

    用循环引用来触发——``default=str`` 已经兜住了「类型不认识」这一大类，
    真正能让 ``json.dumps`` 抛出来的就剩循环引用这种结构性错误。
    """
    stream = io.StringIO()
    环: dict = {}
    环["自己"] = 环

    JsonLogger(stream=stream).info("试试", 坏字段=环)

    record = _lines(stream)[0]
    assert record["msg"] == "试试"
    assert record["log_error"] == "字段无法序列化"


def test_stderr关掉之后不抛异常():
    class _Broken(io.StringIO):
        def write(self, _):
            raise ValueError("I/O operation on closed file")

    # 不能抛
    JsonLogger(stream=_Broken()).info("试试")
