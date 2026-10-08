#!/usr/bin/env bash
# 从 crates/hub-proto 的 proto 生成 Python 代码。
#
# 产物**提交进仓库**：插件团队不需要装 protoc、也不需要通过内网代理拉 grpcio-tools
# 就能直接 import。改了 proto 之后重跑本脚本，并把 hubkit/proto/hubv1/ 的变更一起提交。
#
# 依赖（只在**维护 SDK 时**需要，普通插件开发者不需要）：
#   pip install grpcio-tools protobuf
#
# 版本要一起来：生成的代码里钉了两条运行时下限——
#   * `_runtime_version.ValidateProtobufRuntimeVersion` 认 protobuf 运行时
#   * `GRPC_GENERATED_VERSION` 认 grpcio
# 换 grpcio-tools 版本会让这两条一起变，所以重新生成之后必须同步 sdk/python/pyproject.toml
# 里的依赖下限，否则插件装上旧版本会在 import 期直接抛 RuntimeError。
set -euo pipefail

cd "$(dirname "$0")"

PROTO_ROOT=../../crates/hub-proto/proto
PYTHON=${PYTHON:-python3}
OUT=hubkit/proto/hubv1

PROTOS=(
    hub/v1/envelope.proto
    hub/v1/gateway.proto
    hub/v1/plugin.proto
    hub/v1/registry.proto
    hub/v1/state.proto
)

# grpc_tools 自带 well-known types（any.proto / struct.proto 这些），
# 不必要求调用方另装一份 protoc 的 include 目录——Go 侧要 PROTOC_INCLUDE 是因为
# protoc 是外部二进制，Python 这边工具链整个在 pip 包里。
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

"$PYTHON" -m grpc_tools.protoc \
    --proto_path="$PROTO_ROOT" \
    --python_out="$TMP" --grpc_python_out="$TMP" \
    "${PROTOS[@]}"

mkdir -p "$OUT"

# proto 的 package 是 `hub.v1`，protoc 于是生成 `from hub.v1 import ...`——
# 而 SDK 里的落地位置是 `hubkit/proto/hubv1/`。**刻意不改 proto 的 package**：
# package 是契约的一部分（descriptor 里的消息全限定名就是 hub.v1.Envelope），
# 改了它中台的兼容检查会把每个字段都判成「包名变了」。
#
# 所以只把 import 语句重写成本包内的路径。descriptor 池里的文件名仍是
# `hub/v1/plugin.proto`、消息名仍是 `hub.v1.Envelope`，与 Go/Rust 侧完全一致。
#
# 不做 sys.modules 别名（把 hubkit.proto.hubv1 挂成 `hub.v1`）：那会独占顶层
# 包名 `hub`，与 PyPI 上任何叫 hub 的包撞车，而撞车的表现是「import 到别人的东西」。
"$PYTHON" - "$TMP" "$OUT" <<'PY'
import pathlib, re, sys

src, dst = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])

# `from hub.v1 import x_pb2 as ...`（当前 protoc 的形状）
FROM_RE = re.compile(r"^from hub\.v1 import ", re.M)
# `import hub.v1.x_pb2 as ...`（老版本 protoc 的形状，留着以防降级生成）
IMPORT_RE = re.compile(r"^import hub\.v1\.", re.M)

for path in sorted(src.rglob("*.py")):
    text = path.read_text(encoding="utf-8")
    text = FROM_RE.sub("from hubkit.proto.hubv1 import ", text)
    text = IMPORT_RE.sub("import hubkit.proto.hubv1.", text)

    out = dst / path.name
    out.write_text(text, encoding="utf-8")
    print(f"  {out}")
PY

# 生成物所在的两个目录都要有 __init__.py，否则 `from hubkit.proto.hubv1 import plugin_pb2`
# 在非 editable 安装下会被当成命名空间包、在 editable 安装下直接 ImportError。
: > hubkit/proto/__init__.py
: > "$OUT/__init__.py"

echo "生成完成 → $OUT"
