#!/usr/bin/env bash
# 检查 Go 插件的 vendor 里那些 **SDK 源码副本** 是否与 sdk/go 同步。
#
# 为什么需要它：Go 在 module 带 vendor/ 时走 `-mod=vendor`，**不看** go.mod 里
# `replace ../../sdk/go` 指向的源码。于是「改了 sdk/go 而没重跑 go mod vendor」时，
# 插件仍拿 vendor 里的旧副本编译——测试照绿、镜像照出，全程无声。这条漂移真实发生过。
#
# 覆盖范围：vendor 里**每一个** SDK 的 .go 文件，而不只是 hubkit。
# （第一版只比对了 hubkit/，而实际 vendored 的是 conformance / hubkit / mockhub /
#   proto/hubv1 四个包 14 个文件——另外 8 个仍会漂移，这正是要堵的。）
#
# 检查对象是**每一个带 vendor 的 Go 插件**（examples/auth-plugin、plugins/file-store）。
# 最初只有 auth-plugin 一个时这里硬编码过它的路径——file-store 接进来后如果忘了
# 扩这里，新插件的漂移就没人管：这正是「加插件要改门禁」这件事本身。
#
# 用法：scripts/check-go-vendor.sh [仓库根目录]
# 只依赖 find/cmp，不需要 Go，也不需要网络——所以能在 CI 里直接跑。
set -uo pipefail

ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
GOMOD="$ROOT/sdk/go/go.mod"

# 所有 vendored 了 SDK 的 Go 插件，新增插件时往这里加一行
GO_PLUGINS=(
    examples/auth-plugin
    plugins/file-store
)

if [ ! -f "$GOMOD" ]; then
    echo "找不到 $GOMOD" >&2
    exit 1
fi

MOD="$(awk '/^module /{print $2}' "$GOMOD")"

bad=0

for plugin in "${GO_PLUGINS[@]}"; do
    VROOT="$ROOT/$plugin/vendor/$MOD"
    if [ ! -d "$VROOT" ]; then
        echo "找不到 $plugin vendor 里的 SDK 副本目录：$VROOT（$plugin 还没跑 go mod vendor？）" >&2
        bad=1
        continue
    fi

    echo "==> $plugin"
    echo "    模块    : $MOD"
    echo "    源码    : $ROOT/sdk/go"
    echo "    副本    : $VROOT"

    checked=0
    pkgs=""

    # 以 **vendor 侧** 为准枚举：vendor 里有哪些 .go，就逐个与 sdk/go 下的同路径比对。
    # 这样嵌套包（proto/hubv1）也能覆盖，不用手写目录层级。
    while IFS= read -r rel; do
        [ -n "$rel" ] || continue
        src="$ROOT/sdk/go/$rel"
        ven="$VROOT/$rel"
        pkg="$(dirname "$rel")"
        case " $pkgs " in *" $pkg "*) ;; *) pkgs="$pkgs $pkg" ;; esac
        checked=$((checked + 1))

        if [ ! -f "$src" ]; then
            echo "  !! vendor 里有 $rel，而 sdk/go 下没有——vendor 是旧版本留下的？"
            bad=1
            continue
        fi
        cmp -s "$src" "$ven" || { echo "  !! 内容不一致：$rel"; bad=1; }
    done < <(cd "$VROOT" && find . -name '*.go' | sed 's|^\./||' | sort)

    # 反方向：vendor 里那些包里，源码有的非测试文件却不在 vendor 中
    # （比如新增了一个 .go 而忘了重跑 go mod vendor）
    while IFS= read -r sdir; do
        pkg="${sdir#"$ROOT/sdk/go/"}"
        case " $pkgs " in *" $pkg "*) ;; *) continue ;; esac
        for f in "$sdir"/*.go; do
            [ -e "$f" ] || continue
            name="$(basename "$f")"
            case "$name" in *_test.go) continue ;; esac
            if [ ! -f "$VROOT/$pkg/$name" ]; then
                echo "  !! sdk/go/$pkg/$name 没有被 vendor 进去（重跑 go mod vendor？）"
                bad=1
            fi
        done
    done < <(find "$ROOT/sdk/go" -type d -not -path '*/vendor/*' | sort)

    echo "    覆盖的包：$(printf '%s' "$pkgs" | tr ' ' '\n' | grep -v '^$' | sort | tr '\n' ' ')"
    echo "    比对的文件数：$checked"
    echo
done

if [ "$bad" = "0" ]; then
    echo "OK：所有 Go 插件 vendor 里的 SDK 副本与 sdk/go 源码完全一致"
    exit 0
fi
echo "→ 已漂移。在开发机上执行：cd <对应插件> && go mod vendor  然后提交。"
exit 1
