# plugin-hub 控制台（demo 前端）

plugin-hub 中台的管理控制台：插件目录、编排流程（可视化编辑器）、执行记录、
调用链瀑布图、实例健康、治理面板、触发器、死信与审计。Vue 3 + Element Plus
+ Vue Flow，独立于后端仓库任何前端工程，克隆即可跑。

```
浏览器 ──/hub-api──► vite 代理（剥前缀）──► 中台 HTTP 面 (127.0.0.1:8092)
        └─/mcp─────► vite 代理（不剥前缀）──► 中台 MCP 面（同端口）
```

## 前置条件

| 依赖 | 版本 | 用途 |
|---|---|---|
| Node.js | ≥ 18（建议 20/22） | 前端构建 |
| pnpm | ≥ 9（10/11 亦可，见下） | 包管理 |
| Rust | stable（见仓库根 `rust-toolchain.toml`） | 跑中台 `hub-server` |
| Docker | 任意近期版本 | 本地 PostgreSQL + Redis |

## 1. 配置数据库（PostgreSQL + Redis）

中台**启动时自动跑数据库迁移**，所以只需要空的库，不需要手工建表。

```bash
# PostgreSQL（端口自选，避免与本机已有实例冲突）
docker run -d --name plugin-hub-pg \
  -e POSTGRES_USER=hub -e POSTGRES_PASSWORD=hub -e POSTGRES_DB=plugin_hub \
  -p 55432:5432 postgres:16-alpine

# Redis
docker run -d --name plugin-hub-redis -p 56379:6379 redis:7-alpine
```

验证：

```bash
docker exec plugin-hub-pg pg_isready -U hub -d plugin_hub   # accepting connections
docker exec plugin-hub-redis redis-cli ping                 # PONG
```

> 已有 PG/Redis 实例？跳过这一步，直接在下一步把连接串换成你的。
> 库可以是全新的空库，中台首启会自动建表。

## 2. 启动中台（后端）

仓库根目录：

```bash
DATABASE_URL=postgresql://hub:hub@127.0.0.1:55432/plugin_hub \
REDIS_URL=redis://127.0.0.1:56379/2 \
HUB_HTTP_PORT=8092 \
HUB_OPS_SOCKET=/tmp/plugin-hub-ops.sock \
cargo run -p hub-server
```

说明：

- `DATABASE_URL` / `REDIS_URL` 缺一会拒绝启动；Redis 的 `/2` 是逻辑 db，任意均可。
- `HUB_OPS_SOCKET` 是运维通道的 unix socket，默认 `/run/plugin-hub/ops.sock`
  在 macOS 上不可写——**macOS 本地调试必须指到可写路径**（如 `/tmp`），
  否则启动日志会报 `Read-only file system`（中台其余功能不受影响，但会吵）。
- 起来之后：HTTP/MCP 面在 `127.0.0.1:8092`，插件面 gRPC 在 `127.0.0.1:8093`。

验证：

```bash
curl -s http://127.0.0.1:8092/health
# {"status":"ok","name":"plugin-hub", ...}
```

## 3. 启动控制台（前端）

```bash
cd console
pnpm install
pnpm dev
```

浏览器打开 <http://127.0.0.1:5180/plugins>（**用 127.0.0.1，不要 localhost**）。

顶栏右侧的「中台在线」指示灯变绿即完成对接；红色说明中台没起或端口不对。

### pnpm 10/11 注意

依赖 `esbuild` / `vue-demi` 需要 postinstall 安装平台二进制。pnpm ≥ 10
默认拦截依赖的构建脚本，本仓库已带 `pnpm-workspace.yaml`（`allowBuilds`
白名单）自动放行；若 install 后 `pnpm dev` 报 esbuild 相关错误，执行：

```bash
pnpm approve-builds   # 勾选 esbuild、vue-demi、@parcel/watcher
```

## 4. 让页面有数据：跑示例插件（可选）

空库时控制台全部页面都能正常渲染（空态）。想看有数据的样子，
起 [examples/ping-chain](../examples/ping-chain/) 的两个 Python 插件：

```bash
# 仓库根目录，安装 SDK
python3 -m pip install -e sdk/python

# 终端 1：下游
cd examples/ping-chain
HUB_ADDR=http://127.0.0.1:8093 HUB_ADVERTISE_ADDR=http://127.0.0.1:9101 \
HUB_LISTEN_ADDR=:9101 HUB_INSTANCE_ID=ping-callee-demo python3 callee/main.py

# 终端 2：上游
HUB_ADDR=http://127.0.0.1:8093 HUB_ADVERTISE_ADDR=http://127.0.0.1:9102 \
HUB_LISTEN_ADDR=:9102 HUB_INSTANCE_ID=ping-caller-demo python3 caller/main.py

# 终端 3：触发一次跨插件调用（会产生一条 trace）
curl -s -X POST http://127.0.0.1:8092/ingress/ping-caller \
  -H 'Content-Type: application/json' -d '{"payload": {"text": "hello"}}'
```

然后回控制台刷新：「插件目录」2 个插件、「实例健康」2 个在线实例、
「调用链」出现瀑布图详情，「编排编辑器」左侧节点面板能列出这两个插件。

## 配置参考

| 变量 | 默认 | 说明 |
|---|---|---|
| `VITE_HUB_TARGET` | `http://127.0.0.1:8092` | 中台 HTTP 面地址。指向远端环境：`VITE_HUB_TARGET=http://<host>:8092 pnpm dev` |

dev server 固定监听 `127.0.0.1:5180`（`pnpm dev:host` 可开放外部访问）。
两个代理规则与部署侧 nginx 一致：`/hub-api` 剥前缀、`/mcp` 不剥——
本地跑通的路径到生产不会变。

## 与鉴权的关系

中台管理面**默认无内置鉴权**（鉴权是可选的 auth 插件，见仓库根 README）。
本 demo 未实现登录页；若中台配置了 `HUB_AUTH_PLUGIN`，页面会按中台返回的
401/403 给出提示。

## 生产构建

```bash
pnpm build   # 产物在 dist/，静态文件，任意 nginx/对象存储可托管
```

部署时 nginx 需要两条 location（与仓库根 `deploy/nginx/` 的示例一致）：

```nginx
location /hub-api/ { proxy_pass http://127.0.0.1:8092/; }   # 剥前缀
location = /mcp    { proxy_pass http://127.0.0.1:8092; }     # 不剥
```

## 目录结构

```
console/
├── vite.config.js          # dev 代理（/hub-api 剥前缀、/mcp 不剥）
└── src/
    ├── api/hub.js          # 中台 HTTP 面封装（错误语义、下载、MCP 探测）
    ├── layout/index.vue    # 侧边菜单 + 顶栏（健康指示灯 + 主题切换）
    ├── router/index.js     # 13 条路由（列表 + 详情/编辑器）
    ├── composables/useTheme.ts
    ├── styles/             # element-plus 覆写 + 暗色主题
    └── views/hub/          # 13 个页面：plugins/flows/runs/traces/instances/
                            #   audit/dead-letters/triggers/governance/upgrades/
                            #   onboarding + shared 工具
```
