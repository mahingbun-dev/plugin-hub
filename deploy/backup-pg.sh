#!/usr/bin/env bash
# plugin-hub PostgreSQL 备份。
#
# 为什么需要它：PG 从「借别人的实例」变成「本编排自带的容器」之后，
# 数据就落在本机目录里了——不再有人替我们做备份。这个脚本补上这一环。
#
# 调度：由**宿主机 crontab** 调用（脚本自己不设定时器）。装法见 README「备份与恢复」。
#
# 用法：
#   /apps/plugin-hub/backup-pg.sh
#
# 环境变量（都有默认值，通常不必设；由 crontab 调用时不读 .env）：
#   PG_CONTAINER      PG 容器名，默认 plugin-hub-pg
#   PG_USER / PG_DB   连接用的用户与库，与 compose 里的一致
#   BACKUP_DIR        输出目录，默认 /apps/plugin-hub/backups
#   BACKUP_KEEP_DAYS  保留天数，默认 14
#
# 退出码：0 成功，非 0 失败（cron 会据此记录，故失败路径一律 exit 1）。

set -euo pipefail

# cron 的 PATH 通常只有 /usr/bin:/bin，而 docker 常在 /usr/local/bin。
# 不显式检查的话，这个脚本会**从此再也不产出备份**，唯一的痕迹是
# backup.log 里一行 `docker: command not found`——而告警目前是暂缓项，没人会看到。
if ! command -v docker >/dev/null 2>&1; then
    echo "找不到 docker 命令（PATH=$PATH）。若是从 crontab 调用，请在 crontab 里补 PATH。" >&2
    exit 1
fi

PG_CONTAINER="${PG_CONTAINER:-plugin-hub-pg}"
PG_USER="${PG_USER:-plugin_hub}"
PG_DB="${PG_DB:-plugin_hub}"
# **端口不能省**：PG 以 `-p 55432` 启动时，unix socket 的名字是
# `.s.PGSQL.55432`，而 pg_dump 默认去找 5432 的那个——不显式给端口会得到
# 「connection to server on socket ... failed: No such file or directory」。
PG_PORT="${PG_PORT:-55432}"
BACKUP_DIR="${BACKUP_DIR:-/apps/plugin-hub/backups}"
BACKUP_KEEP_DAYS="${BACKUP_KEEP_DAYS:-14}"

# 备份是整库明文，别给同机其他用户看
umask 077

stamp="$(date +%Y%m%d-%H%M%S)"
target="$BACKUP_DIR/${PG_DB}-${stamp}.sql.gz"
tmp="$target.tmp"

mkdir -p "$BACKUP_DIR"

# 中途失败（磁盘满、容器没了、pg_dump 报错）时，别把半个文件留在目录里
cleanup() { rm -f "$tmp"; }
trap cleanup EXIT

echo "[$(date '+%F %T')] 备份 ${PG_DB}（容器 ${PG_CONTAINER}）→ ${target}"

# 走容器内的 unix socket（官方镜像的 pg_hba 对 local 是 trust），因此不需要密码，
# 也不受 listen_addresses=127.0.0.1 的影响；但**端口必须显式给**（见上面 PG_PORT）。
#
# --clean --if-exists：恢复时能直接盖回去，不必先手工清库。
# --no-owner：恢复时不必先建出同名角色，换台机器也能恢复。
# `set -o pipefail` 保证 pg_dump 失败不会被 gzip 的成功掩盖。
docker exec "$PG_CONTAINER" pg_dump \
        -U "$PG_USER" -d "$PG_DB" -p "$PG_PORT" \
        --clean --if-exists --no-owner \
    | gzip > "$tmp"

# 检查**解压后**的内容，而不是压缩文件本身。
#
# `[ -s "$tmp" ]` 这种判空是**不成立的**：gzip 对空输入也会产出一个约 20 字节的
# 合法 gzip 流，于是那个兜底永远不会触发，坏备份会顶着正式名字留下来。
# 用 `gzip -l` 读未压缩大小即可，不必真解压一遍。
raw_size="$(gzip -l "$tmp" | awk 'NR==2 {print $2}')"
if [ "${raw_size:-0}" -lt 1024 ]; then
    echo "备份解压后只有 ${raw_size:-0} 字节，不像一份完整的 pg_dump 输出，放弃本次" >&2
    exit 1
fi

# 先写 .tmp 再改名：一个写了一半的文件绝不能顶着正式备份的名字躺着，
# 那会让「有备份」变成一句空话
mv "$tmp" "$target"
trap - EXIT

echo "[$(date '+%F %T')] 完成：$(du -h "$target" | cut -f1)"

echo "[$(date '+%F %T')] 清理超过 ${BACKUP_KEEP_DAYS} 天的备份"
find "$BACKUP_DIR" -maxdepth 1 -type f -name "${PG_DB}-*.sql.gz" \
    -mtime "+${BACKUP_KEEP_DAYS}" -print -delete

echo "[$(date '+%F %T')] 目录现有 $(find "$BACKUP_DIR" -maxdepth 1 -type f -name "${PG_DB}-*.sql.gz" | wc -l) 份备份"
