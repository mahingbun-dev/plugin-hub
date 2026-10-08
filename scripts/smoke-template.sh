#!/usr/bin/env bash
# 脚手架模板冒烟：五门语言各走一遍「渲染 → 查产物 → 构建 → 跑 L1 →（可选）真的注册进中台」。
#
# **为什么需要它**：模板是会腐烂的。它今天能编译，不代表下个月还能接进来；
# 而腐烂只会在开发者下载解压之后暴露——那时他已经浪费了半天，且不会想到
# 回头怀疑模板。门禁只做到「编译过」不够，因为最难的那一关（L3）编译根本够不着。
#
# 本地与 CI 跑的是**同一个脚本**：两边分头写的话，本地绿而 CI 红是最费时间的那种失败。
#
# **跑不了的会明说**，不会假装通过：本机没有某门工具链、或者处于离线模式时，
# 那门语言的构建与 L1 会被跳过，并在结尾的总结里逐条列出。一份「全绿」的报告
# 若说不清它验了什么、没验什么，就等于什么都没验。
#
# 用法：
#   bash scripts/smoke-template.sh
#
# 环境变量：
#   SMOKE_LANGS      跑哪几门，空格分隔。缺省五门全跑（go python node rust csharp）
#   SMOKE_OFFLINE=1  跳过需要依赖源的那几步（CI runner 没有外网）
#   HUB_MOCK_BIN     hub-mock 可执行文件。给了就跑 L3（真注册）；不给则跳过并说明。
#   HUB_SCAFFOLD_BIN C# 用的本地渲染器（hub-scaffold）。缺省去 target/ 下找，找不到就构建。
#   SMOKE_PYTHON     Python 那门用哪个解释器（缺省 python3）。**它得装着 protobuf / grpcio /
#                    pytest**——生成工程的 SDK 依赖这两样，而开发者的 venv 里通常是有的。
#   MOCK_HTTP_PORT / MOCK_GRPC_PORT  端口覆盖（缺省挑空闲的）
#
# **只用到 bash 3.2 就有的东西**（macOS 自带的 /bin/bash 就是 3.2，没有关联数组）。
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PLUGIN=order-reader
LANGS="${SMOKE_LANGS:-go python node rust csharp}"
OFFLINE="${SMOKE_OFFLINE:-0}"
PYTHON_BIN="${SMOKE_PYTHON:-python3}"
PLUGIN_PIDS=""

# 五门**共用同一个插件名**，这是刻意的。
#
# 同一个 mock 里，第二门及以后的语言注册的是「同名同版本」，中台会拿它的契约与
# 已在册的那份比对——不一致就回 VERSION_CONFLICT。于是这一条顺带把「五门语言产出
# **完全相同**的 manifest」变成了一条会被真跑到的回归：谁在哪一门里多写一个字段，
# L3 就会红。（这条不一致真实发生过：Node 的 `consumes` 一度比其余四门多一句
# description。）
#
# 代价是这条断言藏在拒码后面——报出来的是「同一版本号不可改变契约」。看到它时别去
# 查注册流程，**先逐门比一比模板里的 manifest**。

# L3 的判据：插件启动日志里出现这一条。
#
# `: *` 里的空格不是笔误——五门都打同一形状的 JSON（time/level/msg + 自定义字段），
# 但**分隔符不一致**：Go 的 slog 与 Rust 的手写拼接是 `"msg":"…"`，而 Python 的
# `json.dumps` 用缺省分隔符，出来是 `"msg": "…"`（冒号后有一个空格）。
# 第一版照抄了 Go 的形状，于是 Python 明明注册成功了却被判成「没注册上」。
REGISTERED_RE='"msg": *"已注册到中台"'

# free_port 挑一个当前没人监听的端口。
#
# **不用固定端口**：这台机器上 9000、19001 都撞过别人的服务，而撞上时的现象
# 千奇百怪——有的直接 bind 失败（好看得懂），有的绑上了但 IPv4 侧是别人的
# （插件日志显示「已监听」、看着健康，连过去却是一句 h2 帧错误）。挑一个空闲的，
# 这一整类麻烦就不存在了。
free_port() {
    local port
    for _ in $(seq 1 60); do
        port=$(( (RANDOM % 20000) + 20000 ))
        if ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            echo "$port"
            return 0
        fi
        exec 3>&- 2>/dev/null || true
    done
    echo "挑了 60 次都没找到空闲端口" >&2
    return 1
}

MOCK_HTTP_PORT="${MOCK_HTTP_PORT:-$(free_port)}"
MOCK_GRPC_PORT="${MOCK_GRPC_PORT:-$(free_port)}"

fail() {
    echo "✗ $*" >&2
    exit 1
}

# ---------------------------------------------------------------- 构建结果台账

# 每门语言的构建结果写进一个小文件：`ok`，或 `skipped<TAB>原因`。
# 用文件而不是关联数组——macOS 自带的是 bash 3.2，没有关联数组。
# 结尾的总结按这份台账逐条列出「没验什么、为什么」，这是本脚本的立身之本。
ok_build() { printf 'ok\n' >"$WORK/$1.build"; }

skip_build() {
    printf 'skipped\t%s\n' "$2" >"$WORK/$1.build"
    echo "    ⏭  [$1] 跳过构建与 L1：$2"
}

build_state() { cut -f1 "$WORK/$1.build" 2>/dev/null || echo none; }

report_skips() {
    local lang reason printed=0
    for lang in $LANGS; do
        [ "$(build_state "$lang")" = "skipped" ] || continue
        if [ "$printed" = 0 ]; then
            echo
            echo "  ⚠️  以下几门**没有完整跑**（原因逐条列出，别把这份报告当成「全绿」）："
            printed=1
        fi
        reason="$(cut -f2- "$WORK/$lang.build")"
        echo "      · $lang —— $reason"
    done
}

# ---------------------------------------------------------------- 每门语言的声明

# 渲染命令。**开发者本地就是这么起工程的**，所以这里走各自的原生脚手架，
# 而不是统一调中台的下载接口——那两套渲染器是不同的代码，用哪个就该验哪个。
render() {
    local lang="$1" dir="$2"
    case "$lang" in
    go)
        (cd "$ROOT/sdk/go" && go run ./cmd/hub-plugin new "$PLUGIN" --dir "$dir" --module "example.com/$PLUGIN") >/dev/null
        ;;
    python)
        "$PYTHON_BIN" "$ROOT/sdk/python/scaffold.py" new "$PLUGIN" --dir "$dir" >/dev/null
        ;;
    node)
        (cd "$ROOT" && node sdk/node/src/cli.ts new "$PLUGIN" --dir "$dir") >/dev/null
        ;;
    rust)
        bash "$ROOT/sdk/rust/scripts/scaffold.sh" "$PLUGIN" "$dir" >/dev/null
        ;;
    csharp)
        # **C# 没有本地脚手架**：它的模板只能由中台的下载接口渲染。`hub-scaffold`
        # 走的是和中台**同一份** build_zip，所以这里验的仍然是那份模板，
        # 而不是「另一套实现」。
        (cd "$ROOT" && "$SCAFFOLD_BIN" csharp "$PLUGIN" --dir "$dir" --package OrderReader) >/dev/null
        ;;
    esac
}

# 期望的产物文件集。**写死而不是扫目录**：模板少发一个文件正是要抓的东西，
# 而扫目录再跟自己比是永远为真的。
expect_files() {
    local lang="$1" dir="$2" expected="" f
    case "$lang" in
    go) expected=".gitignore AGENTS.md Dockerfile README.md go.mod main.go plugin.go plugin_test.go" ;;
    python) expected="AGENTS.md Dockerfile README.md main.py plugin.py requirements.txt test_plugin.py" ;;
    node) expected="AGENTS.md Dockerfile README.md package.json tsconfig.json src/main.ts src/plugin.ts test/plugin.test.ts" ;;
    rust) expected="AGENTS.md Dockerfile README.md Cargo.toml src/main.rs src/plugin.rs tests/conformance.rs" ;;
    csharp) expected="AGENTS.md Dockerfile README.md Plugin.cs Plugin.csproj Program.cs global.json tests/ContractTests.cs tests/Tests.csproj" ;;
    esac
    for f in $expected; do
        [ -f "$dir/$f" ] || fail "[$lang] 产物里缺 $f"
    done
}

# 随包 SDK 的检查。工程必须**自带 SDK 源码**——「开箱能跑」的全部前提就是它：
# 不带的话，依赖声明指向的就是别人机器上不存在的路径，包下了也编不过。
check_sdk() {
    local lang="$1" dir="$2" bad
    [ -d "$dir/sdk" ] || fail "[$lang] 产物里没有 sdk/——工程拿不到 SDK，编不过"

    # 构建产物**必须不在**：体积是一回事，更要紧的是带机器绝对路径的元数据
    # 文件（如 Python 的 hubkit.egg-info/PKG-INFO）会把构建环境泄露给每一个下载者。
    bad=$(find "$dir/sdk" -maxdepth 4 \
        \( -name node_modules -o -name __pycache__ -o -name '*egg-info' -o -name target \
           -o -name '.pytest_cache' -o -name '.venv' \) -print -quit 2>/dev/null || true)
    [ -z "$bad" ] || fail "[$lang] 随包 SDK 里混进了构建产物：${bad#"$dir"/}"

    if [ "$lang" = "go" ]; then
        grep -q "=> \./sdk" "$dir/go.mod" || fail "[go] go.mod 里的 replace 没指向包内的 ./sdk"
        # vendor/ 15MB 且会让新工程被判 inconsistent vendoring；
        # cmd/hub-plugin/ 自己 embed 了 templates，带进去开发者**编译直接失败**
        [ ! -d "$dir/sdk/vendor" ] || fail "[go] 包内 SDK 带了 vendor/"
        [ ! -d "$dir/sdk/cmd/hub-plugin" ] || fail "[go] 包内 SDK 带了 cmd/hub-plugin/"
        [ -d "$dir/sdk/cmd/hubprobe" ] || fail "[go] 包内 SDK 缺 cmd/hubprobe/——开发者没法跑 L2"
    fi
}

# 构建 + L1。跑不了时**记一笔并说明原因**，不当作通过。
build() {
    local lang="$1" dir="$2" log="$WORK/$lang-build.log"

    if [ "$OFFLINE" = "1" ]; then
        skip_build "$lang" "SMOKE_OFFLINE=1：这一步要解析依赖，而离线 runner 没有依赖源"
        return 0
    fi

    case "$lang" in
    go)
        # GOPROXY：内网没有私有代理，按 sdk/go/README.md 的做法走公共代理。
        # 取不到依赖时报出来的话必须**指向依赖源**而不是指向模板——两者都会让
        # tidy 失败，但去处完全不同。第一版把两者混成一句「模板的依赖声明坏了」，
        # 会把人送去查一个不存在的问题。
        if ! (cd "$dir" && GOPROXY="${GOPROXY:-https://goproxy.cn,direct}" go mod tidy) >"$log" 2>&1; then
            echo "--- go mod tidy 输出 ---" >&2
            tail -20 "$log" >&2
            fail "[go] go mod tidy 失败。**先分清是哪一种**：
    · 输出里有 dial tcp / proxy / timeout / 403 → 取不到依赖源（不是模板的问题）
    · 输出里说某个 import 找不到包 / 版本对不上 → 那才是模板或 SDK 的问题"
        fi
        (cd "$dir" && go build ./...) >>"$log" 2>&1 \
            || { cat "$log" >&2; fail "[go] go build 失败——模板生成的代码编不过"; }
        (cd "$dir" && go test ./...) >>"$log" 2>&1 \
            || { tail -30 "$log" >&2; fail "[go] L1 没过"; }
        ok_build go
        ;;
    python)
        (cd "$dir" && "$PYTHON_BIN" -m compileall -q .) >>"$log" 2>&1 \
            || { cat "$log" >&2; fail "[python] 语法编译失败"; }
        # 生成工程是**新**工程，SDK 靠包内的 sdk/ 提供。SDK 自己要 protobuf 与 grpcio，
        # 所以这里先确认解释器装没装——**没装就明说并给出办法**，而不是判成模板的错：
        # 那是运行环境的问题，模板本身没有问题。开发者那边是 venv，通常是装好的。
        if ! "$PYTHON_BIN" -c 'import google.protobuf, grpc' >/dev/null 2>&1; then
            skip_build python "$PYTHON_BIN 里没有 protobuf/grpcio（SDK 的依赖）。装上后可跑：\
'$PYTHON_BIN' -m pip install protobuf grpcio pytest；或用 SMOKE_PYTHON 指一个装好的解释器"
            return 0
        fi
        if ! "$PYTHON_BIN" -m pytest --version >/dev/null 2>&1; then
            skip_build python "$PYTHON_BIN 里没有 pytest：语法编译过了，但 L1（契约一致性）没跑"
            return 0
        fi
        # 必须显式给 PYTHONPATH——生成工程靠包内的 sdk/ 提供 hubkit，
        # 开发者那份 README 里也是这么写的。
        (cd "$dir" && PYTHONPATH="$dir/sdk" "$PYTHON_BIN" -m pytest -q) >>"$log" 2>&1 \
            || { tail -30 "$log" >&2; fail "[python] L1 没过"; }
        ok_build python
        ;;
    node)
        (cd "$dir" && npm install --no-audit --no-fund) >>"$log" 2>&1 \
            || { tail -20 "$log" >&2; fail "[node] npm install 失败（多半是取不到依赖源，不是模板的问题）"; }
        (cd "$dir" && npx tsc --noEmit) >>"$log" 2>&1 \
            || { tail -30 "$log" >&2; fail "[node] 类型检查没过"; }
        (cd "$dir" && npm test) >>"$log" 2>&1 \
            || { tail -30 "$log" >&2; fail "[node] L1 没过"; }
        ok_build node
        ;;
    rust)
        (cd "$dir" && cargo test) >>"$log" 2>&1 \
            || { tail -30 "$log" >&2; fail "[rust] 构建或 L1 没过"; }
        # 生成的工程**不该有编译警告**：模板里留一段注释掉的示例，就是一段死代码，
        # 而开发者会以为是模板本身有问题。这条挡的是那种回归。
        if grep -q "^warning" "$log"; then
            grep -A3 "^warning" "$log" >&2
            fail "[rust] 产物编译时有警告——模板里多半留了没人用的代码"
        fi
        ok_build rust
        ;;
    csharp)
        (cd "$dir" && dotnet build -v q --nologo) >>"$log" 2>&1 \
            || { grep -iv "NU1900" "$log" | tail -30 >&2; fail "[csharp] 构建失败"; }
        # 用 README 里教开发者的那条命令，**一个字都不加**。
        #
        # `dotnet test` 需要 `--project`（.NET 10 起不再接受直接给路径的形式），
        # 而本工程根目录下只有 Plugin.csproj 一个工程。
        #
        # ⚠️ **不要给它加 `--nologo`**：那个选项在 .NET 10 的 `dotnet test` 上不存在，
        # 于是被原样透传给测试宿主（xunit v3 的 MTP runner），runner 认不出就报错退出，
        # 报出来的是「运行了零个测试，错误: 1」（退出码 5）——看着像测试工程坏了，
        # 其实是多了一个参数。实测：同一条命令去掉 `--nologo` 就是 5/5 通过。
        # `dotnet build` 上带 `--nologo` 没问题，那是它的合法选项。
        (cd "$dir" && dotnet test --project tests/) >>"$log" 2>&1 \
            || { grep -iv "NU1900" "$log" | tail -30 >&2; fail "[csharp] L1 没过"; }
        ok_build csharp
        ;;
    esac
}

# L3：把插件真的跑起来，让它向 mock 中台注册。
#
# 中台只接受**它自己拨得通**的地址，所以这里要起一个真进程、监听一个真端口——
# 用一个假的成功日志糊过去的话，这一关就没有任何意义了。
#
# 五门的配置变量名是同一套（HUB_ADDR / HUB_ADVERTISE_ADDR / HUB_LISTEN_ADDR /
# HUB_LOG_LEVEL），所以这里不必按语言分叉。
run_plugin() {
    local lang="$1" dir="$2" port="$3" log="$4"
    (
        cd "$dir" || exit 1
        export HUB_ADDR="http://127.0.0.1:$MOCK_GRPC_PORT"
        export HUB_LISTEN_ADDR=":$port"
        export HUB_ADVERTISE_ADDR="http://127.0.0.1:$port"
        export HUB_LOG_LEVEL=info
        case "$lang" in
        go) exec go run . ;;
        # PYTHONPATH 指到包内的 sdk/：开发者那边是 `pip install -r requirements.txt`
        # 把 SDK 装进 venv，而这里不往系统解释器里装东西，效果一样。
        python) export PYTHONPATH="$dir/sdk"; exec "$PYTHON_BIN" main.py ;;
        node) exec node src/main.ts ;;
        rust) exec cargo run --quiet ;;
        csharp) exec dotnet run -v q --nologo ;;
        esac
    ) >"$log" 2>&1 &
    echo $!
}

toolchain_of() {
    case "$1" in
    go) echo go ;;
    python) echo "$PYTHON_BIN" ;;
    node) echo node ;;
    rust) echo cargo ;;
    csharp) echo dotnet ;;
    esac
}

# 收掉一个插件进程。
#
# **子进程也要收**：`go run` / `cargo run` / `dotnet run` 都是「启动器 + 真进程」
# 两层，只杀启动器的话真进程会留下来，继续占着它的端口、继续往日志里写——
# 下一门语言看起来就像「同时注册了两个实例」，很难查。
kill_plugin() {
    local pid="${1:-}"
    [ -n "$pid" ] || return 0
    pkill -P "$pid" 2>/dev/null || true
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

# ---------------------------------------------------------------- 0. 前置检查

echo "==> 前置检查"

# C# 的本地渲染器（其余四门有自己的脚手架）。不给就现构建一个，
# 而不是静默少跑一门。
SCAFFOLD_BIN="${HUB_SCAFFOLD_BIN:-}"
if [ -z "$SCAFFOLD_BIN" ]; then
    for cand in "$ROOT/target/debug/hub-scaffold" "$ROOT/target/release/hub-scaffold"; do
        [ -x "$cand" ] && SCAFFOLD_BIN="$cand" && break
    done
fi
case " $LANGS " in
*" csharp "*)
    if [ -z "$SCAFFOLD_BIN" ]; then
        echo "==> 构建 hub-scaffold（C# 没有本地脚手架，用它渲染；与中台下载接口同一份代码）"
        # 构不出来**不判失败**：离线 runner 里没有 cargo 依赖快照，那是环境问题而不是
        # 模板的问题。但也不能假装 C# 验过了——下面那门会被整门跳过并说明。
        if (cd "$ROOT" && cargo build -q -p hub-templates --bin hub-scaffold) >/dev/null 2>&1; then
            SCAFFOLD_BIN="$ROOT/target/debug/hub-scaffold"
        else
            echo "    ⚠️  构建失败（多半是离线、且没有 cargo 依赖快照）——C# 这门会被跳过"
        fi
    fi
    [ -n "$SCAFFOLD_BIN" ] && echo "    hub-scaffold: $SCAFFOLD_BIN"
    ;;
esac

# ---------------------------------------------------------------- 1~3. 逐门语言

for lang in $LANGS; do
    echo
    echo "########## $lang ##########"
    dir="$WORK/$lang"

    # C# 没有本地脚手架，渲染器（hub-scaffold）构不出来时整门跳过。
    # 这不是「C# 模板没验」的托词：它渲染的就是中台的同一份模板，
    # 那条路由 `cargo test -p hub-templates` 覆盖（见 lib.rs 里那几条）。
    if [ "$lang" = "csharp" ] && [ -z "$SCAFFOLD_BIN" ]; then
        skip_build csharp "拿不到 hub-scaffold（离线 runner 缺 cargo 依赖快照）：整门没验"
        continue
    fi

    echo "==> 渲染模板"
    rm -rf "$dir"
    if ! render "$lang" "$dir" >"$WORK/$lang-render.log" 2>&1; then
        cat "$WORK/$lang-render.log" >&2
        fail "[$lang] 渲染失败"
    fi

    echo "==> 查产物"
    expect_files "$lang" "$dir"
    check_sdk "$lang" "$dir"

    # 占位符没渲染干净会留下 @@key@@，那是「不逐字读产物就发现不了」的缺陷。
    #
    # 排除 sdk/（原样照抄的源码，中台自己的 README 里就有 `@@`）、node_modules/
    # （第三方依赖）、__pycache__ 与 .egg-info（构建产物，本来就该由上面那条拦住）。
    leftover=$(grep -rn '@@' "$dir" \
        --exclude-dir=sdk --exclude-dir=node_modules --exclude-dir=__pycache__ \
        --exclude-dir='*egg-info' 2>/dev/null || true)
    if [ -n "$leftover" ]; then
        echo "$leftover" >&2
        fail "[$lang] 产物里有未替换的占位符"
    fi

    # HTML 注释是维护者视角的话，不该发给开发者。**但 .csproj 例外**：那是 XML，
    # 注释本来就长这样，而且里面写的是给开发者看的说明。
    html=$(grep -rn '<!--' "$dir" \
        --exclude-dir=sdk --exclude-dir=node_modules --exclude-dir=__pycache__ \
        --exclude-dir='*egg-info' --exclude='*.csproj' 2>/dev/null || true)
    if [ -n "$html" ]; then
        echo "$html" >&2
        fail "[$lang] 产物里有发给开发者的 HTML 注释（维护者视角的话不该进包）"
    fi

    # AGENTS.md 的四道关由共通层内联而来，缺了它说明共通层没被注入——
    # 那不会报错，只会发出去一份没有接入指南的工程。
    for want in "L1 契约自洽" "L2 运行时自检" "L3 中台接受注册" "L4 端到端调用"; do
        grep -q "$want" "$dir/AGENTS.md" || fail "[$lang] AGENTS.md 里缺「$want」——共通层没内联进来"
    done

    echo "==> 构建 + L1"
    tc="$(toolchain_of "$lang")"
    if ! command -v "$tc" >/dev/null 2>&1; then
        skip_build "$lang" "本机没有 $tc"
    else
        build "$lang" "$dir"
    fi
done

# ---------------------------------------------------------------- 4. L3（可选）

if [ "$OFFLINE" = "1" ]; then
    echo
    echo "✓ 模板冒烟通过（**离线模式**）"
    echo "  已验：渲染 → 产物文件集 → 无残留占位符/HTML 注释 → AGENTS.md 含四道关"
    echo "        → 随包 SDK 在位且不含构建产物"
    echo "  未验：构建、L1 契约一致性、L3 真注册——离线 runner 取不到依赖源。"
    report_skips
    exit 0
fi

if [ -z "${HUB_MOCK_BIN:-}" ] || [ ! -x "${HUB_MOCK_BIN:-}" ]; then
    echo
    echo "⚠️  没给 HUB_MOCK_BIN，**跳过 L3**（真注册）。"
    echo "   也就是说这次冒烟只证明了「渲染干净、编得过、L1 过」，没证明「中台会接受它」。"
    echo "   要跑 L3：先 cargo build -p hub-mock，再 HUB_MOCK_BIN=target/debug/hub-mock 重跑本脚本。"
    report_skips
    exit 0
fi

echo
echo "==> L3：起一个 mock 中台，让插件真的注册进去"
HUB_MOCK_HTTP_ADDR="127.0.0.1:$MOCK_HTTP_PORT" \
HUB_MOCK_GRPC_ADDR="127.0.0.1:$MOCK_GRPC_PORT" \
HUB_MOCK_LOG=warn \
    "$HUB_MOCK_BIN" &
MOCK_PID=$!
cleanup() {
    local pid
    for pid in $PLUGIN_PIDS; do kill_plugin "$pid"; done
    kill "$MOCK_PID" 2>/dev/null || true
    # 收尸。少了这一句，bash 会在退出时把「Terminated」打到 stdout 上——
    # 明明成功了却看到两行「Terminated」，人会以为哪里出错了。
    wait 2>/dev/null || true
}
trap 'cleanup; rm -rf "$WORK"' EXIT

for _ in $(seq 1 30); do
    curl -sf "http://127.0.0.1:$MOCK_HTTP_PORT/health" >/dev/null 2>&1 && break
    sleep 1
done
curl -sf "http://127.0.0.1:$MOCK_HTTP_PORT/health" >/dev/null \
    || fail "mock 中台没起来（$MOCK_HTTP_PORT）"

# 确认对面是 mock 而不是真中台：两者绑同一组端口，「注册成功了」这句日志
# 两者都会打，但能证明的东西不同。这条判据是血换来的——真实发生过一次，
# 有人拿着 mock 的结果当对真中台验过了。
mock_name="$(curl -s "http://127.0.0.1:$MOCK_HTTP_PORT/health" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')"
[ "$mock_name" = "hub-mock" ] || fail "对面不是 hub-mock（报的是 $mock_name）——先确认你在对谁验"

L3_DONE=""
for lang in $LANGS; do
    if [ "$(build_state "$lang")" != "ok" ]; then
        echo "    ⏭  [$lang] 没构建成功（或没构建），跳过 L3"
        continue
    fi

    echo
    echo "==> L3 [$lang]"
    dir="$WORK/$lang"
    log="$WORK/$lang-plugin.log"
    port="$(free_port)"
    pid=$(run_plugin "$lang" "$dir" "$port" "$log")
    PLUGIN_PIDS="$PLUGIN_PIDS $pid"

    for _ in $(seq 1 60); do
        grep -q "$REGISTERED_RE" "$log" 2>/dev/null && break
        sleep 1
    done

    if ! grep -q "$REGISTERED_RE" "$log"; then
        echo "--- 插件日志 ---" >&2
        tail -30 "$log" >&2
        echo "--- mock 侧被拒记录 ---" >&2
        curl -s "http://127.0.0.1:$MOCK_HTTP_PORT/rejections" >&2 || true
        fail "[$lang] 插件没注册上（L3 没过）"
    fi

    # 中台侧也要能看到它——插件日志说成功、中台那边却没有，说明两边说的不是一件事
    registered="$(curl -s "http://127.0.0.1:$MOCK_HTTP_PORT/plugins" | grep -c "$PLUGIN" || true)"
    [ "$registered" -ge 1 ] || fail "[$lang] 插件自称注册成功，但 mock 的 /plugins 里没有它"

    # 收掉这一门的插件再跑下一门：同名插件第二次注册会被当成「重启」而放行，
    # 但留着一堆进程会让端口与日志互相干扰。
    kill_plugin "$pid"
    echo "    ✓ 已注册到中台，mock 侧也查得到"
    L3_DONE="$L3_DONE $lang"
done

echo
echo "✓ 模板冒烟通过"
echo "  已验：${L3_DONE# } 各走了一遍「渲染 → 产物干净 → 构建 → L1 → L3（真注册进 mock 中台）」"
report_skips
