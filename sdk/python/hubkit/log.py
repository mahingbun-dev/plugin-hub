"""JSON 结构化日志，打到 stderr。

与 Go 侧（``slog.NewJSONHandler(os.Stderr, ...)``）**逐字段同形**：同样有
``time`` / ``level`` / ``msg`` 三个固定字段，业务字段平铺在后面。

这不是审美选择——中台的接入指南（``templates/_shared/onboarding.md``）里直接引用了
这个形状的日志行，两门语言的插件打出来的东西不一样的话，照着指南排查的人会以为
自己看错了行。字段名也一样：``code`` / ``message`` / ``detail`` / ``retry_in``。
"""

from __future__ import annotations

import datetime as _dt
import json
import sys
from typing import Any, TextIO

# 级别名与 Go 的 slog 一致（DEBUG / INFO / WARN / ERROR），
# 而不是 Python logging 默认的全大写长名（WARNING / CRITICAL）——
# 中台的日志检索按 level 值过滤，多一种拼写就多一类漏网。
DEBUG = "DEBUG"
INFO = "INFO"
WARN = "WARN"
ERROR = "ERROR"

_ORDER = {DEBUG: 10, INFO: 20, WARN: 30, ERROR: 40}

# HUB_LOG_LEVEL 认得的值。``warning`` 也收——Python 用惯了 logging 的人会这么写，
# 为这点差异让人对着一个「日志一条都不出」的进程排查不值得。
_LEVEL_ALIASES = {
    "debug": DEBUG,
    "info": INFO,
    "warn": WARN,
    "warning": WARN,
    "error": ERROR,
}


def level_from_env(raw: str | None) -> str:
    """把 ``HUB_LOG_LEVEL`` 解析成级别名；认不出来的按 INFO（与 Go 侧同款兜底）。"""
    return _LEVEL_ALIASES.get((raw or "").strip().lower(), INFO)


class JsonLogger:
    """按级别过滤、以 JSON 单行写出的日志器。

    自己实现而不是用 ``logging`` + 自定义 Formatter：SDK 会在插件的进程里跑，而
    ``logging`` 的全局配置（root handler、字典配置、别人 ``basicConfig`` 的时机）
    都不是 SDK 能主的。插件作者可以把 ``configure_logging()`` 接进 ``logging``，
    但 SDK 自身的输出不该依赖那个全局状态是对的。
    """

    def __init__(self, level: str = INFO, stream: TextIO | None = None) -> None:
        self.level = level if level in _ORDER else INFO
        # 存的是**当前**的 sys.stderr，而不是构造时抓的那个对象：
        # 插件测试里常用 capsys / redirect_stderr 换掉 stderr，构造时抓死就收不到日志了。
        self._stream = stream

    def _stream_now(self) -> TextIO:
        return self._stream if self._stream is not None else sys.stderr

    def enabled(self, level: str) -> bool:
        return _ORDER[level] >= _ORDER[self.level]

    def log(self, level: str, msg: str, **fields: Any) -> None:
        if not self.enabled(level):
            return

        # 固定字段排在前面、业务字段在后面：Go 的 slog 就是这个顺序，
        # 而「一条日志的三个固定字段总在同一列」在 grep/awk 里是有用的。
        record: dict[str, Any] = {
            "time": _dt.datetime.now().astimezone().isoformat(),
            "level": level,
            "msg": msg,
        }
        for key, value in fields.items():
            record[key] = value

        try:
            line = json.dumps(record, ensure_ascii=False, default=str)
        except (TypeError, ValueError):
            # 日志本身绝不能把插件搞挂：业务字段里塞了不可序列化的东西时，
            # 退化成只打固定字段，而不是把这个异常抛进调用栈。
            line = json.dumps(
                {"time": record["time"], "level": level, "msg": msg, "log_error": "字段无法序列化"},
                ensure_ascii=False,
            )

        stream = self._stream_now()
        try:
            stream.write(line + "\n")
            stream.flush()
        except (ValueError, OSError):
            # 进程退出时 stderr 可能已经关了。丢一条日志比在关闭路径上抛异常好。
            pass

    def debug(self, msg: str, **fields: Any) -> None:
        self.log(DEBUG, msg, **fields)

    def info(self, msg: str, **fields: Any) -> None:
        self.log(INFO, msg, **fields)

    def warn(self, msg: str, **fields: Any) -> None:
        self.log(WARN, msg, **fields)

    def error(self, msg: str, **fields: Any) -> None:
        self.log(ERROR, msg, **fields)

    def child(self, **fields: Any) -> "JsonLogger":
        """派生一个会把 ``fields`` 附在每条日志上的日志器。

        用它把 ``plugin`` 这类恒定字段绑一次，避免在每个调用点重复传——
        漏传一次就会让那一条日志没法按插件名检索。
        """
        return _ChildLogger(self, fields)


class _ChildLogger(JsonLogger):
    def __init__(self, parent: JsonLogger, fields: dict[str, Any]) -> None:
        super().__init__(parent.level, parent._stream)
        self._parent = parent
        self._fields = fields

    def log(self, level: str, msg: str, **fields: Any) -> None:
        merged = dict(self._fields)
        merged.update(fields)
        self._parent.log(level, msg, **merged)
