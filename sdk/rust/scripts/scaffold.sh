#!/usr/bin/env bash
# 在本地把 sdk/rust/templates/plugin 渲染成一个能跑的插件工程。
#
# 用途：验证模板本身（渲染得对不对、生成出来的工程能不能编、测试能不能过）。
# **中台侧真正用的渲染器是 crates/hub-templates**，两者规则相同——`@@key@@` 纯文本替换、
# 未定义的占位符直接报错、模板文件名去掉 `.tmpl` 就是输出名。这个脚本只是把那套规则
# 在 SDK 这一侧复制一份，让改模板的人不必先跑起整个中台。
#
# 用法：
#   sdk/rust/scripts/scaffold.sh <插件名> [输出目录]
#
# 生成完之后：
#   cd <输出目录> && cargo test && cargo build
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
sdk_root="$(cd "$here/.." && pwd)"          # sdk/rust
repo_root="$(cd "$sdk_root/../.." && pwd)"  # 仓库根

# 共通层素材寄居在 Go 的模板目录下（Go 的 //go:embed 不能向上取父目录，
# 位置只能迁就它）。中台侧也是从那里取的，见 crates/hub-templates/build.rs 的
# shared_onboarding_expr。
onboarding="$repo_root/sdk/go/cmd/hub-plugin/templates/_shared/onboarding.md"

name="${1:-}"
out="${2:-}"
if [[ -z "$name" ]]; then
    echo "用法: $0 <插件名> [输出目录]" >&2
    exit 2
fi
if [[ -z "$out" ]]; then
    out="./$name"
fi
if [[ -e "$out" ]]; then
    echo "目录 $out 已存在" >&2
    exit 1
fi
if [[ ! -f "$onboarding" ]]; then
    echo "找不到共通层素材 $onboarding" >&2
    exit 1
fi

# 取值与中台侧的语言登记（crates/hub-templates/build.rs 的 Language 字段）一致：
#   sdk_module = Rust 侧的 crate 名，写进代码的 use 语句
#   sdk_path   = SDK 源码在包内的相对路径，由打包器注入为 ./sdk
mkdir -p "$out"

OUT="$out" NAME="$name" \
SDK_ROOT="$sdk_root" ONBOARDING="$onboarding" \
python3 - <<'PY'
import os
import pathlib
import shutil
import sys

out = pathlib.Path(os.environ["OUT"]).resolve()
name = os.environ["NAME"]
sdk_root = pathlib.Path(os.environ["SDK_ROOT"])
onboarding = pathlib.Path(os.environ["ONBOARDING"]).read_text(encoding="utf-8")

# 与 crates/hub-templates/src/render.rs 的 Values 同一组键
values = {
    "name": name,
    "package": name,
    "sdk_module": "hubkit",
    "sdk_path": "./sdk",
    "hub_addr": "http://127.0.0.1:8093",
    "onboarding": onboarding,
}

OPEN = CLOSE = "@@"


def render_once(raw: str, values: dict) -> str:
    parts = []
    rest = raw
    while True:
        start = rest.find(OPEN)
        if start < 0:
            parts.append(rest)
            return "".join(parts)
        parts.append(rest[:start])
        rest = rest[start + len(OPEN):]
        end = rest.find(CLOSE)
        if end < 0:
            raise SystemExit(f"模板里有一个没有收尾的 {OPEN}：{rest[:40]!r}")
        key = rest[:end]
        rest = rest[end + len(CLOSE):]
        if key not in values:
            raise SystemExit(
                f"模板里有未定义的占位符 {OPEN}{key}{CLOSE}"
                f"（认得的只有 {' / '.join(values)}）"
            )
        parts.append(values[key])


def render(raw: str) -> str:
    # 两趟：第一趟把 @@onboarding@@ 展开成共通层正文，第二趟替换**正文里自己的**
    # 占位符。单趟做不到——扫描是自左向右的，刚插入的文本不会被重新扫一遍。
    once = render_once(raw, values)
    return render_once(once, values) if OPEN in once else once


templates = sdk_root / "templates" / "plugin"
files = sorted(p for p in templates.rglob("*") if p.is_file())
if not files:
    raise SystemExit(f"模板目录 {templates} 里一个文件都没有")

for src in files:
    rel = src.relative_to(templates).as_posix()
    # 约定：模板文件名去掉 .tmpl 就是输出名，所以 `.gitignore.tmpl` → `.gitignore`
    dest_rel = rel[:-len(".tmpl")] if rel.endswith(".tmpl") else rel
    dest = out / dest_rel
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(render(src.read_text(encoding="utf-8")), encoding="utf-8")

# 随包把 SDK 源码拷进去。生成的工程用 `path = "./sdk"` 指到它——不带的话，
# 那个 path 在别人机器上就是一个不存在的路径，工程拿到手也编不过。
#
# 排除项要和中台侧 crates/hub-templates/build.rs 的 SDK_SOURCES 一致：
#   target    —— 构建产物，几百 MB
#   templates —— 脚手架自己，不该发给开发者
#   protogen  —— 重新生成 proto 的维护者工具，它要读 crates/hub-proto，
#                而那个目录在开发者的包里不存在
#   scripts   —— 本脚本
EXCLUDE = {"target", "templates", "protogen", "scripts"}

dest_sdk = out / "sdk"
for src in sorted(sdk_root.rglob("*")):
    rel = src.relative_to(sdk_root)
    if rel.parts and rel.parts[0] in EXCLUDE:
        continue
    if src.is_dir():
        continue
    dest = dest_sdk / rel
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, dest)

print(f"已生成插件工程 {out}")
PY

echo
echo "下一步："
echo "  cd $out && cargo test && cargo build"
echo
echo "⚠️ HUB_ADVERTISE_ADDR 必须是**中台能拨通**的地址，不是本机视角的 localhost——"
echo "   中台在注册时会连它做可达性探测，这是接入时最容易踩的坑。"
