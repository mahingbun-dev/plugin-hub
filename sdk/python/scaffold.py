#!/usr/bin/env python3
"""生成一个可运行的 plugin-hub 插件工程（Python）。

    python3 scaffold.py new <name> [--dir <path>] [--package <pkg>] [--hub-addr <addr>]

生成的插件**零 proto 工具链依赖**：它用 ``google.protobuf.Struct`` 承载 JSON 载荷，
也就是「直接调用」那条路（agent 经 MCP、外部系统经 HTTP）。需要类型化契约的插件
可以在这个基础上再加自己的 .proto。

与 Go 侧的 ``cmd/hub-plugin`` 是同一件事。两条约定与它逐字相同：

* **模板文件名去掉 ``.tmpl`` 后缀就是输出名**，所以需要前导点的文件在模板侧就写成
  ``.gitignore.tmpl``。文件名即输出名之后，「输出叫什么」只存在于一个地方，
  中台侧（Rust）机械地去后缀渲染同一批模板时不可能再对不上。
* **未定义的占位符直接报错、不留空**。留空会生成一个 manifest 里插件名为空、或
  import 路径残缺的工程，那种工程要等到编译或注册时才炸——而那时已经离「生成」很远了。
"""

from __future__ import annotations

import argparse
import shutil
import sys
from pathlib import Path

# 模板占位符的定界符。
#
# 不用 Jinja 之类：同一份模板中台侧（Rust）也要渲染，`@@key@@` 与任何语言的语法都
# 不冲突，两侧各几十行就能实现同一套规则。改了这里，中台的 crates/hub-templates 要跟着改。
PLACEHOLDER_OPEN = "@@"
PLACEHOLDER_CLOSE = "@@"

# 认得的占位符。集中列出来，模板里写错时这条清单就是唯一的对照表。
PLACEHOLDER_NAME = "name"
PLACEHOLDER_PACKAGE = "package"
PLACEHOLDER_SDK_MODULE = "sdk_module"
PLACEHOLDER_SDK_PATH = "sdk_path"
PLACEHOLDER_ONBOARDING = "onboarding"
PLACEHOLDER_HUB_ADDR = "hub_addr"

KNOWN_PLACEHOLDERS = (
    PLACEHOLDER_NAME,
    PLACEHOLDER_PACKAGE,
    PLACEHOLDER_SDK_MODULE,
    PLACEHOLDER_SDK_PATH,
    PLACEHOLDER_ONBOARDING,
    PLACEHOLDER_HUB_ADDR,
)

# SDK 包名。**必须与 sdk/python/pyproject.toml 里的包名一致**——它同时是模板里
# import 语句的前缀（``@@sdk_module@@`` 的取值）。
SDK_MODULE = "hubkit"

# 随包下发的 SDK 在生成工程里的目录名。
#
# 它与 requirements.txt 里 ``-e @@sdk_path@@`` 的目标、以及中台侧打包时用的目录名
# 必须一致：中台那边在 crates/hub-templates/build.rs 的 SDK_SOURCES 里，这边是这一行。
SDK_DIR_IN_PROJECT = "sdk"

# 不随包下发的条目（相对 SDK 根的**路径前缀**）。
#
# 按路径前缀判断而不是只看第一段：只看第一段的话，带斜杠的排除项会漏掉。
#
#   - tests：SDK 自己的测试，它 import 的 ``echo_plugin`` 不在包里，带过去只会
#     让生成工程里多一个跑不过的测试目录。
#   - templates：脚手架模板自己，带过去是死重量。
#   - scaffold.py：脚手架自己。它读的就是 templates/，而 templates/ 不发出去——
#     带过去会得到一个「点开就报找不到模板」的脚本。
#   - __pycache__ / *.egg-info / .pytest_cache：本地跑过 pip 或 pytest 之后的产物，
#     是不是干净检出取决于维护者有没有先跑过命令，不能指望。
SDK_EXCLUDED = (
    "tests",
    "templates",
    "scaffold.py",
    "__pycache__",
    ".pytest_cache",
    ".venv",
)

# 路径里**任何一段**以这些后缀结尾时跳过。
#
# 单列出来是因为它们的目录名里有包名（``hubkit.egg-info``），写不成固定值；
# 而它恰恰是最容易漏的一个——``pip install -e .`` 就会在 SDK 根目录里建出来，
# 带着它下发的工程会装出一个指向**打包者机器上路径**的旧元数据。
SDK_EXCLUDED_SUFFIXES = (".egg-info",)


def render(raw: str, values: dict[str, str]) -> str:
    """把模板里的 ``@@key@@`` 替换成 ``values[key]``。

    分两趟：第一趟把 ``@@onboarding@@`` 展开成共通层正文，第二趟替换**正文里自己的**
    占位符。单趟做不到——扫描是自左向右的，刚插入的文本不会被重新扫一遍，
    于是共通层里的 ``@@name@@`` 会原样留在产物里。
    """
    once = _render_once(raw, values)
    if PLACEHOLDER_OPEN not in once:
        return once
    return _render_once(once, values)


def _render_once(raw: str, values: dict[str, str]) -> str:
    out: list[str] = []
    rest = raw

    while True:
        start = rest.find(PLACEHOLDER_OPEN)
        if start < 0:
            out.append(rest)
            return "".join(out)

        out.append(rest[:start])
        rest = rest[start + len(PLACEHOLDER_OPEN):]

        end = rest.find(PLACEHOLDER_CLOSE)
        if end < 0:
            raise ValueError(f"模板里有一个没有收尾的 {PLACEHOLDER_OPEN}：{rest[:40]!r}")

        key = rest[:end]
        rest = rest[end + len(PLACEHOLDER_CLOSE):]

        if key not in values:
            raise ValueError(
                f"模板里有未定义的占位符 {PLACEHOLDER_OPEN}{key}{PLACEHOLDER_CLOSE}"
                f"（认得的只有 {' / '.join(KNOWN_PLACEHOLDERS)}）"
            )
        out.append(values[key])


def scaffold_values(name: str, package: str, hub_addr: str) -> dict[str, str]:
    """给出渲染脚手架所需的全部占位符取值。

    中台侧渲染同一份模板时用的是同一组键（见 crates/hub-templates）。两边对不上的话，
    中台那边会以「未定义的占位符」报错，而不是悄悄生成一份残缺的工程。
    """
    return {
        PLACEHOLDER_NAME: name,
        PLACEHOLDER_PACKAGE: package,
        PLACEHOLDER_SDK_MODULE: SDK_MODULE,
        # SDK 源码在生成工程里的**相对路径**。模板里写它、requirements.txt 里用它，
        # 值由这里（打包器）注入——模板侧不该知道 SDK 被放在哪。
        PLACEHOLDER_SDK_PATH: f"./{SDK_DIR_IN_PROJECT}",
        PLACEHOLDER_HUB_ADDR: hub_addr,
    }


def is_valid_name(name: str) -> bool:
    """插件名规则，与 Go 侧 ``hub-plugin`` 的 ``isValidName`` 逐字相同。

    比中台的 ``is_valid_plugin_name`` 更严：这里限定了小写，因为这个值还会被用作
    目录名与镜像名，大小写混排在这些地方会带来跨平台差异（macOS 的大小写不敏感
    文件系统上，``My-Plugin`` 与 ``my-plugin`` 是同一个目录）。
    """
    if not name or len(name) > 64:
        return False
    for index, char in enumerate(name):
        if char.isascii() and (char.islower() or char.isdigit()):
            continue
        if char == "-" and 0 < index < len(name) - 1:
            continue
        return False
    return True


def generate(target: Path, values: dict[str, str], *, with_sdk: bool = True) -> None:
    """把模板渲染进 ``target`` 目录。"""
    if target.exists():
        raise FileExistsError(f"目录 {target} 已存在")

    template_root = _template_root()
    payload_root = template_root / "plugin"

    # 共通层素材由这里注入，**调用方不必知道它**：忘了传的后果是 AGENTS.md 里出现一个
    # 没有内容的章节，而那种缺陷要靠人逐字读生成出来的文件才会发现。
    shared = (template_root / "_shared" / "onboarding.md").read_text(encoding="utf-8")
    values = {**values, PLACEHOLDER_ONBOARDING: shared}

    target.mkdir(parents=True)
    for template in sorted(payload_root.iterdir()):
        if not template.is_file():
            continue

        rendered = render(template.read_text(encoding="utf-8"), values)
        # 文件名即输出名：去掉 .tmpl 后缀。没有后缀的模板原样输出——
        # 静默跳过会让生成出来的工程少文件，而少文件要等运行时才发现。
        output = template.name[: -len(".tmpl")] if template.name.endswith(".tmpl") else template.name
        (target / output).write_text(rendered, encoding="utf-8")

    if with_sdk:
        _copy_sdk(_sdk_root(), target / SDK_DIR_IN_PROJECT)


def _template_root() -> Path:
    """模板目录。它就在本文件旁边，不依赖任何编译期路径。"""
    root = Path(__file__).resolve().parent / "templates"
    if not (root / "plugin").is_dir():
        raise FileNotFoundError(f"模板目录不存在: {root / 'plugin'}")
    return root


def _sdk_root() -> Path:
    """SDK 源码的根目录（本文件所在目录）。

    认一个必须有、且不随包下发的标志物，确认真的是 SDK 根而不是别的目录。
    """
    root = Path(__file__).resolve().parent
    if not (root / SDK_MODULE / "run.py").exists():
        raise FileNotFoundError(
            f"按本文件推出来的 SDK 根目录 {root} 里没有 {SDK_MODULE}/run.py。"
            "多半是脚手架被拷到了别处，而它旁边的 SDK 源码没跟着走。"
        )
    return root


def _excluded(rel: str) -> bool:
    """判断一个相对 SDK 根的路径（POSIX 风格）是否该跳过。"""
    parts = rel.split("/")
    for part in parts:
        if any(part.endswith(suffix) for suffix in SDK_EXCLUDED_SUFFIXES):
            return True

    for skip in SDK_EXCLUDED:
        if rel == skip or rel.startswith(skip + "/"):
            return True
    return False


def _copy_sdk(root: Path, target: Path) -> None:
    """把 SDK 源码拷进生成工程。

    **为什么要拷**：生成的工程用 ``-e ./sdk`` 指到 SDK，而那个路径在别人机器上不存在
    （内网没有私有 PyPI 镜像托管这个包）。带上源码，依赖指向包内的相对路径，
    工程拿到手就能装。
    """
    target.mkdir(parents=True, exist_ok=True)

    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root).as_posix()
        if _excluded(rel):
            continue
        if "__pycache__" in path.parts or path.name.endswith(".pyc"):
            continue

        destination = target / rel
        if path.is_dir():
            destination.mkdir(parents=True, exist_ok=True)
        else:
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, destination)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="scaffold.py", description="plugin-hub 插件脚手架（Python）"
    )
    sub = parser.add_subparsers(dest="command", required=True)

    new = sub.add_parser("new", help="生成一个插件工程")
    new.add_argument("name", help="插件名（= manifest.name）")
    new.add_argument("--dir", default=None, help="生成到哪个目录（缺省 ./<name>）")
    new.add_argument("--package", default=None, help="工程的包名/分发名（缺省 <name>）")
    new.add_argument(
        "--hub-addr",
        default="http://127.0.0.1:8093",
        help="中台插件面地址，写进 README 的示例里（缺省 http://127.0.0.1:8093）",
    )
    new.add_argument("--no-sdk", action="store_true", help="不把 SDK 源码拷进工程（仅供测试模板）")

    args = parser.parse_args(argv)

    if not is_valid_name(args.name):
        print(
            f"插件名 {args.name!r} 非法：只允许小写字母、数字与连字符，且以字母开头",
            file=sys.stderr,
        )
        return 2

    target = Path(args.dir or f"./{args.name}")
    values = scaffold_values(args.name, args.package or args.name, args.hub_addr)

    try:
        generate(target, values, with_sdk=not args.no_sdk)
    except (FileExistsError, FileNotFoundError, ValueError) as err:
        print(f"生成失败: {err}", file=sys.stderr)
        return 1

    print(
        f"""已生成插件工程 {target}

下一步:

  cd {target}
  pip install -r requirements.txt   # 内含 -e {values[PLACEHOLDER_SDK_PATH]}
  pip install pytest
  python -m pytest -q               # L1 契约自洽
  HUB_ADDR={args.hub_addr} \\
  HUB_ADVERTISE_ADDR=http://127.0.0.1:9000 \\
  python main.py

注意 HUB_ADVERTISE_ADDR 必须是**中台能拨通**的地址，不是本机视角的 localhost——
中台在注册时会连它做可达性探测，这是接入时最容易踩的坑。

Dockerfile 已一并生成，按「独立容器」的方式部署插件（见 docs/design.md）。
"""
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
