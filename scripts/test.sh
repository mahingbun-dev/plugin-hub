#!/usr/bin/env bash
# 载入本地 .env（不入库）后运行测试。
#
# 需要真实 PostgreSQL：hub-store 的集成测试用 #[sqlx::test] 为每个用例开独立库，
# 依赖 DATABASE_URL 指向一个可 CREATE DATABASE 的实例。
#
#   bash scripts/test.sh            # 全量
#   bash scripts/test.sh -p hub-store
set -euo pipefail

cd "$(dirname "$0")/.."

if [ -f .env ]; then
    set -a
    # shellcheck disable=SC1091
    . ./.env
    set +a
fi

if [ -z "${DATABASE_URL:-}" ]; then
    echo "警告：未设置 DATABASE_URL，需要数据库的集成测试会被跳过或失败。" >&2
    echo "      可在仓库根目录放一份 .env（已 gitignore），参考 deploy/.env.example。" >&2
fi

exec cargo test --workspace "$@"
