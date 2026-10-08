#!/usr/bin/env bash
# 在容器里跑 Python 侧的测试（SDK + 各个 Python 插件）。
#
# **为什么在容器里跑而不是在 runner 上直接跑**：测的应该是**真实运行环境里的
# Python 版本**（3.12，来自 anc-runtime-base），而不是 runner 上碰巧装了什么。
# 这个仓库已经因为「构建镜像与运行镜像的 Python 版本不同」踩过一次（见
# plugins/sql-executor/Dockerfile 的注释），不想再踩第二次。
#
# 依赖用**同一份 wheel 快照**——与构建镜像时装的完全一样。所以测过的依赖
# 就是上线要用的依赖，不存在"测试时装了一套、构建时换了一套"。
#
# 用法（CI 与本地共用）：
#   scripts/run-py-tests.sh                    # 用 anc-runtime-base:latest
#   BASE_IMAGE=anc-runtime-base:localamd64 scripts/run-py-tests.sh
#
# 前置：`wheels/` 已就位（CI 从快照解压；本地 `pip download` 生成）。
#
# 集成测试（要真 MySQL / PG 的那些）**在这里会自动 skip**：容器里没有那几个
# 数据库。要跑它们得在开发机上设 SQL_EXECUTOR_TEST_MYSQL 等环境变量，见
# plugins/sql-executor/tests/test_execute.py 的说明。**不硬失败是有意的**——
# 这个插件要的是七种数据库，任何 runner 都不可能有全部。
#
# monitor-log 的测试**全是离线的**（它只渲染代码，不发请求），所以在这条链里
# 是真跑，没有 skip。谁要往那个 tests/ 里加需要联网的用例，就得先想清楚这一点。
#
# dc-dict 的测试同理**全是假件**（锁引擎用 fake state/adapter、HTTP 用 mock
# urllib），容器里真跑、没有 skip；它的 requirements.txt 与 sql-executor 逐字节
# 一致（消费同一份 wheel 快照）。
set -euo pipefail

cd "$(dirname "$0")/.."

BASE_IMAGE="${BASE_IMAGE:-anc-runtime-base:latest}"

# 只在**本地跨架构验证**时用（比如在 arm64 开发机上验 amd64 的镜像）。
# CI 与生产都不设它——那时基础镜像与 runner 是同一个架构。
DOCKER_PLATFORM="${DOCKER_PLATFORM:-}"

if [ ! -d wheels ]; then
    echo "缺少 wheels/ —— 先跑 scripts/py-wheels-sync.sh 生成快照，或本地 pip download" >&2
    exit 1
fi

docker run --rm \
    ${DOCKER_PLATFORM:+--platform "$DOCKER_PLATFORM"} \
    -v "$PWD":/src \
    -w /src \
    -e PYTHONDONTWRITEBYTECODE=1 \
    --entrypoint python3 \
    "$BASE_IMAGE" \
    -c 'import subprocess, sys
# 每个插件的清单都单独装、装进**同一个** --target。分别装是必要的：
# 各插件的 requirements.txt 是 requirements-all.txt 的不同子集，合并成一份去装
# 也可以，但那样就多了一份「谁跟谁合并」的规则要维护——而这份文件里的每一处
# 间接都已经被证明会漂移（见 .gitlab-ci.yml 里那几个自指缺口的注释）。
# 装到同一个目录则是安全的：来源同一份 wheel 快照，同版本覆盖同版本。
for req in ("plugins/sql-executor/requirements.txt",
            "plugins/dc-dict/requirements.txt",
            "plugins/monitor-log/requirements.txt"):
    subprocess.run(["pip", "install", "--quiet", "--no-index",
                    "--find-links=/src/wheels", "--target=/tmp/deps", "-r", req],
                   check=True)
env = {
    # 不加 proto 目录：生成的桩在 `hubkit` 包**内部**（`hubkit/proto/hubv1/`），
    # 跟 SDK 一起进 sys.path 就够了，`import hubkit.proto.hubv1` 到哪都成立。
    #
    # 各插件的 src 并列挂上来即可：顶层包名互不相同（sql_executor / dc_dict /
    # monitor_log），不会遮蔽；为一个插件起一次容器反而会把「同一份依赖快照」
    # 这个前提拆散。
    "PYTHONPATH": "/tmp/deps:/src/sdk/python"
                  ":/src/plugins/sql-executor/src:/src/plugins/dc-dict/src:/src/plugins/monitor-log/src",
    "PATH": "/usr/local/bin:/usr/bin:/bin",
    "PYTHONDONTWRITEBYTECODE": "1",
}
# **一个目录一次 pytest 调用**，不要合并成一次。
# pytest 默认的 prepend 模式下，测试模块名取**文件 basename**（这些 tests/ 都没有
# `__init__.py`），于是两个插件各带一份 `test_plugin.py` / `test_render.py` 时，
# 先收集的那份占住了 `sys.modules["test_plugin"]`，后收集的那份就以
# 「import file mismatch」在**收集期**中断整次运行——一个用例都跑不到。
# 分进程则每个目录各有一个干净的 sys.modules。顺带的好处：某个插件的测试挂了，
# 其余插件的结果仍然打得出来，不会互相遮蔽。
#
# 依赖快照不受影响：仍然只起一个容器、只装一次依赖，拆开的只是 pytest 进程。
TARGETS = ("sdk/python/tests",
           "plugins/sql-executor/tests",
           "plugins/dc-dict/tests",
           "plugins/monitor-log/tests")
rc = 0
for target in TARGETS:
    print(f"==> 测试 {target}", flush=True)
    code = subprocess.run([sys.executable, "-m", "pytest", target, "-q"],
                          env=env).returncode
    if code and not rc:
        rc = code
sys.exit(rc)'
