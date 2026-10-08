# plugin-hub 的通用运行镜像（多阶段在线构建）。
#
# 用法（在仓库根目录）：
#   docker build -f deploy/Dockerfile -t plugin-hub:latest .
#
# 构建输入刻意与 .dockerignore 对齐：vendor/（如需离线构建先 `cargo vendor`
# 生成）、crates/、sdk/（hub-templates 的 build.rs 会扫描 sdk/*/templates/
# 并把脚手架模板嵌进二进制，不拷它构建会停在「模板目录不存在」上）。

# ---------- 构建阶段 ----------
FROM rust:1-slim AS build
WORKDIR /src

# 依赖先行，利用层缓存——源码改动不会让依赖重编一遍。
# 有 vendor/ 快照（`cargo vendor` 产物）则离线编译；没有就走 crates.io。
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY sdk ./sdk
COPY vendor ./vendor

RUN if [ -d vendor ]; then \
        cargo build --release --offline \
            --config 'source.crates-io.replace-with="vendored-sources"' \
            --config 'source.vendored-sources.directory="vendor"' ; \
    else \
        cargo build --release ; \
    fi \
    && strip target/release/hub-server target/release/hubctl

# ---------- 运行阶段 ----------
FROM debian:bookworm-slim

# curl 给 HEALTHCHECK 用，tini 做信号转发（优雅退出依赖它）
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl tini \
    && rm -rf /var/lib/apt/lists/*

# 非 root 运行
RUN useradd --system --uid 1001 --create-home app

COPY --from=build /src/target/release/hub-server /usr/local/bin/hub-server
COPY --from=build /src/target/release/hubctl /usr/local/bin/hubctl

# 运维通道的 socket 目录。容器用 host 网络时，这个路径只有宿主机 root 能进
# ——信任边界就是它。构建期以 root 建好并把属主交给 app，运行期非 root 跑。
RUN mkdir -p /run/plugin-hub && chown app:app /run/plugin-hub

# HTTP 面只绑回环（经 nginx 同源暴露给控制台/agent），插件面绑全网卡（远程插件要能注册）。
# 这两个值刻意不同，见 docs/design.md。compose 会用 .env 里的同名变量覆盖它们。
ENV HUB_HTTP_HOST=127.0.0.1 \
    HUB_HTTP_PORT=8095 \
    HUB_GRPC_HOST=0.0.0.0 \
    HUB_GRPC_PORT=8093 \
    HUB_OPS_SOCKET=/run/plugin-hub/ops.sock

EXPOSE 8095 8093

HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${HUB_HTTP_PORT}/health" || exit 1

USER app
ENTRYPOINT ["/usr/bin/tini", "--", "hub-server"]
