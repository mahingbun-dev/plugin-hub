"""跨语言的**规则判定**。

事实来源是 ``sdk/go/hubkit/testdata/hub-rules.json``——Rust 侧
（``crates/hub-grpc/tests/state_rules.rs``）与 Go 侧（``sdk/go/hubkit/rules_test.go``）
读的是同一份文件。Python 侧这里跟着同一套判据写了一份实现，**但没有参与那份契约测试**：
Python 的插件开发者手上不会同时有中台仓库，为了一个字符集判定把整个仓库拉下来不划算。

代价说清楚：这一份与契约文件之间**没有自动化的漂移检测**。中台改了插件名规则而这里
没跟上的话，表现是「本地自检过了、注册被拒」；改了状态键名规则而这里没跟上，表现是
「本地预检放行、HubState 调用被回 INVALID_ARGUMENT」。判据都很短（见下），维护时对着
proto、Rust 的 ``crates/hub-registry/src/validate.rs`` 与 ``crates/hub-grpc/src/state.rs``
一起读一遍即可。
"""

from __future__ import annotations


def valid_state_segment(segment: str) -> bool:
    """判断字符串能否作为 HubState 的 ``namespace`` / ``key`` / scan ``prefix``。

    规则：非空、最长 200 字节、只允许 ``[A-Za-z0-9_.-]``。与
    ``sdk/go/hubkit.ValidStateSegment``、中台的 ``crates/hub-grpc/src/state.rs``
    的 ``valid_segment`` 等价，事实来源同为 ``hub-rules.json`` 的 ``stateSegment``
    （同一节里的 ``_byte_vs_char`` 解释了为什么不必专门区分字节与字符）。

    **放行 ``*`` 是漏洞不是功能**：中台靠字符串拼接加插件前缀，通配符会让 KvScan
    变成跨命名空间的模式匹配；冒号同理——它破坏前缀的结构。

    客户端拿它做**本地预检**（fail-fast，见 :mod:`hubkit.state`）：中台对同样的输入
    回的是一句 ``INVALID_ARGUMENT``，而插件侧看不到是哪个字段、哪个字符的问题。
    """
    if not segment:
        return False
    # 同 valid_plugin_name：先按字节判长，再逐字符判字符集（与 Rust/Go 两侧同序）
    if len(segment.encode("utf-8")) > 200:
        return False

    # `char.isascii()` 这个前置条件不能省：Python 的 str.isalnum() 是 Unicode 语义，
    # 「中」也算 alnum，只写 isalnum() 会把中文放行。
    return all(
        char.isascii() and (char.isalnum() or char in ("_", ".", "-"))
        for char in segment
    )


def valid_plugin_name(name: str) -> bool:
    """判断 manifest 里的插件名是否合法。

    规则：非空、最长 64 字节、首字符是字母或数字、其余位置允许 ``[A-Za-z0-9_-]``。
    与中台的 ``crates/hub-registry/src/validate.rs`` 的 ``is_valid_plugin_name`` 等价，
    也与 ``sdk/go/hubkit.ValidPluginName`` 等价。

    按**字节**判长度而不是字符数：中台用 ``String::len()``（字节）判，
    一个中文插件名在这里按字符算是 4、按字节算是 12，两边会给出不同的结论。
    """
    if not name:
        return False
    # 先按字节判长，再逐字符判字符集：Rust 侧先取首个 char 再看 name.len()
    if len(name.encode("utf-8")) > 64:
        return False

    for index, char in enumerate(name):
        if char.isascii() and (char.isalnum()):
            continue
        if index > 0 and char in ("-", "_"):
            continue
        return False
    return True
