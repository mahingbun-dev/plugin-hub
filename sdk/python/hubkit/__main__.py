"""``python -m hubkit`` —— 直连插件调试（Python 侧的 ``hubprobe``）。

    python -m hubkit health   http://127.0.0.1:9000
    python -m hubkit describe http://127.0.0.1:9000
    python -m hubkit validate http://127.0.0.1:9000 --payload '{"text":"你好"}'
    python -m hubkit invoke   http://127.0.0.1:9000 --payload '{"text":"你好"}'
    python -m hubkit conform  http://127.0.0.1:9000

**这些命令连的都是插件自己的地址，不需要中台**——它们验的是「插件自己做得对不对」，
证不了「中台能不能按登记的地址拨到你」。那是网络视角的事，只有把插件真注册进中台
（L3）才答得了。

``conform`` 与 ``invoke`` 的退出码：通过 0，有任何 ✗ 或被拒是 1，参数/连接问题 2。
"""

from __future__ import annotations

import argparse
import json
import sys

from . import conformance
from .envelope import payload_json, with_payload_json
from .mockhub import dial_plugin
from .proto.hubv1 import envelope_pb2

EXIT_OK = 0
EXIT_FAIL = 1
EXIT_USAGE = 2


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m hubkit", description="直连插件调试")
    sub = parser.add_subparsers(dest="command", required=True)

    for name in ("health", "describe", "conform"):
        sub.add_parser(name, help=f"{name} <插件地址>").add_argument("addr")

    for name in ("validate", "invoke"):
        cmd = sub.add_parser(name, help=f"{name} <插件地址> --payload <JSON>")
        cmd.add_argument("addr")
        cmd.add_argument(
            "--payload",
            default="{}",
            help="JSON 对象，作为信封的载荷（缺省空对象）",
        )

    args = parser.parse_args(argv)

    try:
        if args.command == "health":
            return _health(args.addr)
        if args.command == "describe":
            return _describe(args.addr)
        if args.command == "conform":
            return _conform(args.addr)
        return _call(args.addr, args.payload, handle=args.command == "invoke")
    except ValueError as err:
        print(f"参数错误: {err}", file=sys.stderr)
        return EXIT_USAGE


def _health(addr: str) -> int:
    with dial_plugin(addr) as client:
        response = client.health()
    print(json.dumps({"healthy": response.healthy, "message": response.message}, ensure_ascii=False))
    return EXIT_OK if response.healthy else EXIT_FAIL


def _describe(addr: str) -> int:
    with dial_plugin(addr) as client:
        manifest = client.describe()
    print(json.dumps(_manifest_to_dict(manifest), ensure_ascii=False, indent=2))
    return EXIT_OK


def _conform(addr: str) -> int:
    report = conformance.runtime(addr)
    print(report, end="")
    return EXIT_OK if report.passed() else EXIT_FAIL


def _call(addr: str, raw_payload: str, *, handle: bool) -> int:
    try:
        payload = json.loads(raw_payload)
    except json.JSONDecodeError as err:
        raise ValueError(f"--payload 不是合法 JSON: {err}") from err
    if not isinstance(payload, dict):
        raise ValueError("--payload 必须是 JSON **对象**")

    probe = with_payload_json(envelope_pb2.Envelope(message_id="hubkit-cli"), payload)

    with dial_plugin(addr) as client:
        # 与中台一致的顺序：先校验、通过才进插件体。校验不通过会短路——
        # 这样 CLI 的输出形状与真实调用完全一样，不会出现「CLI 能过、中台过不了」。
        validation = client.validate(probe)
        if not validation.valid:
            issues = [
                {"path": issue.path, "message": issue.message, "severity": issue.severity}
                for issue in validation.issues
            ]
            print(json.dumps({"valid": False, "issues": issues}, ensure_ascii=False, indent=2))
            return EXIT_FAIL

        if not handle:
            print(json.dumps({"valid": True}, ensure_ascii=False))
            return EXIT_OK

        out = client.handle(probe)

    # 只回载荷，不回那个 bool：这个命令的用途是「看看插件吐了什么」，
    # 载荷不是 JSON 时打印 None 已经足够说明问题，多一层包装反而碍眼。
    out_payload, _ = payload_json(out)
    print(json.dumps(out_payload, ensure_ascii=False, indent=2))
    return EXIT_OK


def _manifest_to_dict(manifest) -> dict:
    return {
        "name": manifest.name,
        "version": manifest.version,
        "description": manifest.description,
        "owner": manifest.owner,
        "consumes": [c.fq_name for c in manifest.consumes],
        "produces": [c.fq_name for c in manifest.produces],
        "tools": [{"name": t.name, "description": t.description} for t in manifest.tools],
    }


if __name__ == "__main__":
    sys.exit(main())
