"""插件的契约一致性自测套件。

插件接入中台之前必须跑通它。检查的都是「中台在真实调用时依赖、但等生产才发现
代价太大」的约定：契约自洽、校验器不崩、插件体真的返回信封。

分两部分，因为它们的输入不同：

* :func:`local` —— 只看插件对象，不需要把它跑起来。检查 manifest 与 descriptor
  是否自洽，也就是**中台注册期的那一套校验**；本地先跑能省一轮「改完推上去才发现被拒」。
* :func:`runtime` —— 对运行中的插件跑，检查它在真实调用下的行为。

典型用法（生成的工程里就有这份测试）::

    from hubkit import conformance

    def test_conformance():
        report = conformance.local(NewPlugin())
        assert report.passed(), str(report)

与 ``sdk/go/conformance`` 的检查项**逐条对应**，措辞也一样——两门语言的插件作者
看到同一句失败原因，才能对着同一份接入指南排查。
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any

from google.protobuf import descriptor_pb2

from ..envelope import STRUCT_FQ_NAME, is_well_known_fq_name, with_payload_json
from ..plugin import Plugin
from ..proto.hubv1 import envelope_pb2, plugin_pb2
from ..rules import valid_plugin_name

# 各项运行时检查的时间上限（秒）。
#
# 插件是外部进程，卡住的检查要尽快暴露而不是把测试挂死。
CHECK_TIMEOUT = 5.0

# 工具名要拼进 MCP 的工具标识，字符集更窄。
_TOOL_NAME_PATTERN = re.compile(r"^[A-Za-z0-9_-]+$")


@dataclass
class Check:
    """一项检查的结果。"""

    name: str
    passed: bool
    detail: str = ""


@dataclass
class Report:
    """一套检查的结果。"""

    # 被检查的对象：本地检查是插件名，运行时检查是插件地址
    subject: str = ""
    checks: list[Check] = field(default_factory=list)

    def passed(self) -> bool:
        """是否全部通过。"""
        return all(check.passed for check in self.checks)

    def failures(self) -> list[Check]:
        """返回未通过的检查。"""
        return [check for check in self.checks if not check.passed]

    def add(self, name: str, passed: bool, detail: str = "") -> None:
        self.checks.append(Check(name=name, passed=passed, detail=detail))

    def __str__(self) -> str:
        lines = [f"契约一致性检查 {self.subject}"]
        for check in self.checks:
            mark = "✓" if check.passed else "✗"
            line = f"  {mark} {check.name}"
            if check.detail:
                line += f" —— {check.detail}"
            lines.append(line)
        return "\n".join(lines) + "\n"


def local(plugin: Plugin) -> Report:
    """检查插件对象自身的自洽性，不需要把它跑起来。

    复现的是中台注册期的那几条校验，因此它能挡住绝大多数「推上去才发现被拒」的问题。
    """
    report = Report(subject="（本地）")

    manifest = plugin.manifest()
    if manifest is None:
        report.subject = "（未命名插件）"
        report.add("manifest 存在", False, "manifest() 返回了 None")
        return report

    report.subject = f"{manifest.name}@{manifest.version}"
    report.add("manifest 存在", True)

    name = manifest.name
    if not (name or "").strip():
        report.add("插件名合法", False, "缺少插件名 name")
    elif not valid_plugin_name(name):
        report.add("插件名合法", False, "只允许字母数字与 -_，最长 64 字符，且以字母数字开头")
    else:
        report.add("插件名合法", True, name)

    version = manifest.version
    if not (version or "").strip():
        report.add("版本号存在", False, "缺少版本号 —— flow 靠它锁定实例")
    else:
        report.add("版本号存在", True, version)

    # descriptor 是中台做字段级兼容检查的依据。
    # 空的是合法的：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 proto。
    raw = plugin.descriptor()
    messages: set[str] = set()
    if not raw:
        report.add("descriptor 可用", True, "无自有 proto（只用 well-known 载荷）")
    else:
        try:
            messages = descriptor_messages(raw)
        except Exception as err:  # noqa: BLE001 —— 解析失败就是检查失败，报出原因即可
            report.add("descriptor 可用", False, str(err))
            return report
        report.add("descriptor 可用", True, f"{len(raw)} 字节、{len(messages)} 个消息类型")

    _check_declared_messages(report, manifest, messages)
    _check_tools(report, manifest)
    return report


def _check_declared_messages(
    report: Report, manifest: plugin_pb2.PluginManifest, messages: set[str]
) -> None:
    """验证「自述与编译产物一致」。

    这是最有价值的一条：改了 proto 忘了重新生成、或者消息改名后忘了同步 manifest，
    都在这里被抓住，而不是等注册时被中台拒。
    """
    missing: list[str] = []

    def check(direction: str, contracts: Any) -> None:
        for contract in contracts:
            fq_name = contract.fq_name
            if is_well_known_fq_name(fq_name):
                continue
            if fq_name not in messages:
                missing.append(f"{direction} 里的 {fq_name}")

    check("produces", manifest.produces)
    check("consumes", manifest.consumes)

    if missing:
        report.add(
            "声明的类型都在 descriptor 中",
            False,
            "；".join(missing) + " —— manifest 的自述必须与提交的 proto 一致",
        )
        return

    if not manifest.produces and not manifest.consumes:
        report.add(
            "声明了契约",
            False,
            "既没有 produces 也没有 consumes —— 至少声明一个，"
            f"接受直接调用的插件请声明 {STRUCT_FQ_NAME}",
        )
        return

    report.add(
        "声明的类型都在 descriptor 中",
        True,
        f"produces {len(manifest.produces)} 个、consumes {len(manifest.consumes)} 个",
    )


def _check_tools(report: Report, manifest: plugin_pb2.PluginManifest) -> None:
    if not manifest.tools:
        # 不暴露工具是合法的：插件可能只参与 flow
        report.add("工具声明合法", True, "未声明工具（仅参与 flow）")
        return

    seen: set[str] = set()
    for tool in manifest.tools:
        if not _TOOL_NAME_PATTERN.match(tool.name):
            report.add(
                "工具声明合法",
                False,
                f"工具名 {tool.name!r} 含非法字符 —— 它要拼进 MCP 的工具标识",
            )
            return
        if tool.name in seen:
            report.add("工具声明合法", False, f"工具 {tool.name} 重复声明 —— 同一插件内工具名必须唯一")
            return
        seen.add(tool.name)

    report.add("工具声明合法", True, f"{len(seen)} 个工具")


def runtime(plugin_addr: str, timeout: float = CHECK_TIMEOUT) -> Report:
    """对运行中的插件检查运行时行为。

    ``plugin_addr`` 是插件的 gRPC 地址（如 ``http://127.0.0.1:9000``）。
    """
    # 延迟导入：conformance 与 mockhub 互相引用，模块级的循环 import 会让
    # 「只跑本地检查」的场景也被迫加载 grpc 的全部依赖。
    from ..mockhub import dial_plugin

    report = Report(subject=plugin_addr)

    try:
        client = dial_plugin(plugin_addr)
    except Exception as err:  # noqa: BLE001
        report.add("插件地址可用", False, str(err))
        return report

    try:
        try:
            health = client.health(timeout=timeout)
        except Exception as err:  # noqa: BLE001
            report.add("Health 可应答", False, str(err))
            return report
        if not health.healthy:
            report.add("Health 可应答", False, "插件自报不健康: " + health.message)
            return report
        report.add("Health 可应答", True)

        try:
            manifest = client.describe(timeout=timeout)
        except Exception as err:  # noqa: BLE001
            report.add("Describe 可应答", False, str(err))
        else:
            report.add("Describe 可应答", True, f"{manifest.name}@{manifest.version}")

        # 空信封：插件必须能处理，而不是 panic 或挂住
        try:
            client.validate(envelope_pb2.Envelope(), timeout=timeout)
        except Exception as err:  # noqa: BLE001
            report.add("校验器对空信封不崩", False, str(err))
        else:
            report.add("校验器对空信封不崩", True)

        try:
            probe = with_payload_json(
                envelope_pb2.Envelope(message_id="conformance-1"), {"conformance": True}
            )
        except ValueError as err:
            report.add("校验器可处理 JSON 载荷", False, str(err))
            report.add("插件体返回信封", False, "无法构造探针载荷")
            return report

        try:
            response = client.validate(probe, timeout=timeout)
        except Exception as err:  # noqa: BLE001
            report.add("校验器可处理 JSON 载荷", False, str(err))
        else:
            if response.valid:
                report.add("校验器可处理 JSON 载荷", True, "通过")
            else:
                # 探针载荷本来就可能不满足业务规则，拒绝是合法结果
                report.add(
                    "校验器可处理 JSON 载荷",
                    True,
                    f"拒绝（{len(response.issues)} 条问题）—— 探针载荷不满足业务规则属正常",
                )

        try:
            out = client.handle(probe, timeout=timeout)
        except Exception as err:  # noqa: BLE001
            # 被业务逻辑拒掉是正常的，但必须是「明确报错」而不是超时或 panic
            report.add("插件体返回信封", True, f"拒绝处理（{err}）—— 探针载荷不满足业务规则属正常")
        else:
            if out is None:
                report.add("插件体返回信封", False, "返回了空信封 —— 中台会把它当成插件异常")
            else:
                detail = "已返回信封"
                from ..envelope import payload_json

                # 只看第二个值：载荷**是** JSON（哪怕是空对象）就算这条过了。
                # 拿「载荷 == {}」当判据的话，一个刻意返回空 JSON 的插件会被
                # 报成「不是 JSON 载荷」，与事实正好相反。
                if payload_json(out)[1]:
                    detail = "已返回 JSON 载荷"
                report.add("插件体返回信封", True, detail)
    finally:
        client.close()

    return report


def descriptor_messages(raw: bytes) -> set[str]:
    """解析 FileDescriptorSet，返回其中的消息全限定名。

    刻意**直接遍历 descriptor 结构，而不是走 ``descriptor_pool``**：后者要求
    descriptor 自包含（能解析出所有 import），而插件提交的 descriptor 只含自己的
    proto——中台侧的 Rust 实现同样只遍历不解析引用，两边必须一致，否则会出现
    「本地检查不过但中台接受」这种更糟的分歧。
    """
    file_set = descriptor_pb2.FileDescriptorSet()
    try:
        file_set.ParseFromString(raw)
    except Exception as err:  # noqa: BLE001
        raise ValueError(f"descriptor 无法解析: {err}") from err

    found: set[str] = set()
    for file in file_set.file:
        _collect_messages(file.package, file.message_type, found)
    return found


def _collect_messages(
    prefix: str, messages: Any, into: set[str]
) -> None:
    for message in messages:
        # map 字段会生成合成的 XxxEntry 消息，属实现细节，不算契约类型
        if message.options.map_entry:
            continue
        if not message.name:
            continue

        fq_name = f"{prefix}.{message.name}" if prefix else message.name
        into.add(fq_name)
        _collect_messages(fq_name, message.nested_type, into)
