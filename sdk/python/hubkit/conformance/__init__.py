"""契约一致性自测套件（``hubkit.conformance``）。

与 ``sdk/go/conformance`` 对应：-:func:`local` 复现中台注册期的校验，
:func:`runtime` 检查运行中插件的行为。
"""

from .checks import (
    CHECK_TIMEOUT,
    Check,
    Report,
    descriptor_messages,
    local,
    runtime,
)

__all__ = [
    "CHECK_TIMEOUT",
    "Check",
    "Report",
    "descriptor_messages",
    "local",
    "runtime",
]
