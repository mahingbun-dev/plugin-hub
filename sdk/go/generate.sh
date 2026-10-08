#!/usr/bin/env bash
# 从 crates/hub-proto 的 proto 生成 Go 代码。
#
# 产物提交进仓库：插件团队不需要装 protoc 就能用。
# 改了 proto 之后重跑本脚本，并把 proto/hubv1/ 的变更一起提交。
#
# 依赖：
#   - protoc（PROTOC 指定路径，或已在 PATH 中）
#   - protoc-gen-go / protoc-gen-go-grpc（go install ...）
#   - PROTOC_INCLUDE 指向 protoc 自带的 include 目录（well-known types）
set -euo pipefail

cd "$(dirname "$0")"

PROTO_ROOT=../../crates/hub-proto/proto
PROTOC=${PROTOC:-protoc}
PROTOC_INCLUDE=${PROTOC_INCLUDE:?请设置 PROTOC_INCLUDE 指向 protoc 自带的 include 目录}
MODULE=$(awk '/^module /{print $2}' go.mod)
OUT_PKG=$MODULE/proto/hubv1

PROTOS=(
    hub/v1/envelope.proto
    hub/v1/gateway.proto
    hub/v1/plugin.proto
    hub/v1/registry.proto
    hub/v1/state.proto
)

mkdir -p proto/hubv1

# require_unimplemented_servers=false：插件只实现部分方法时不必再嵌一个
# UnimplementedXxxServer，接入成本低一点
"$PROTOC" \
    --proto_path="$PROTO_ROOT" \
    --proto_path="$PROTOC_INCLUDE" \
    --go_out=. --go_opt=module="$MODULE" \
    --go-grpc_out=. --go-grpc_opt=module="$MODULE" --go-grpc_opt=require_unimplemented_servers=false \
    "${PROTOS[@]}"

echo "生成完成 → proto/hubv1/（包 $OUT_PKG）"
