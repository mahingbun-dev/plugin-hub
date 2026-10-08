#!/usr/bin/env bash
# 把中台的**契约素材**同步进本 SDK：proto 与跨语言规则文件。
#
# **为什么是拷贝而不是引用 `crates/hub-proto`**：SDK 会随模板包一起下发给插件团队
# （见 crates/hub-templates/build.rs 的 SDK_SOURCES），下载方手里**没有中台仓库**，
# 引用仓库内的相对路径在他们那儿就是断的。Go 侧的做法是把生成的 .pb.go 提交进
# `sdk/go/proto/`，这里等价地把 .proto 与规则文件提交进 `HubKit/`——C# 侧的代码
# 由 Grpc.Tools 在构建时生成，所以随包发的是**源**而不是产物。
#
# 拷贝会漂移，所以漂移由测试兜着：HubKit.Tests/ContractSyncTests.cs 逐字节比对
# 本目录与仓库里的原件。改了契约忘了重跑本脚本，那条测试会红。
set -euo pipefail

cd "$(dirname "$0")"

PROTO_SRC=../../crates/hub-proto/proto/hub/v1
PROTO_DST=HubKit/protos/hub/v1

# 与 Go 侧 generate.sh 同一份清单：bus.proto 是**中台内部**的总线消息，
# 插件既不收也不发（插件只实现 PluginRuntime、只调 PluginRegistry），不发下去。
PROTOS=(
    envelope.proto
    plugin.proto
    registry.proto
    state.proto
    gateway.proto
)

mkdir -p "$PROTO_DST"
for p in "${PROTOS[@]}"; do
    cp "$PROTO_SRC/$p" "$PROTO_DST/$p"
done

# 跨语言规则文件的事实来源是 **Go 侧那份**（crates/hub-grpc/tests/state_rules.rs
# 读的也是它）。取 Go 的那份而不是自己再写一份：两侧读同一份文件才能钉住
# 「判定合一」，各写一份就只是两份手抄件。
RULES_SRC=../go/hubkit/testdata/hub-rules.json
RULES_DST=HubKit/testdata/hub-rules.json

mkdir -p "$(dirname "$RULES_DST")"
cp "$RULES_SRC" "$RULES_DST"

echo "同步完成 → $PROTO_DST（${#PROTOS[@]} 个 proto）、$RULES_DST"
