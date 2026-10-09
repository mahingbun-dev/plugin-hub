# plugin-hub

English | [简体中文](README.zh-CN.md)

> **hub 链接万物** —— Rust 插件中台：契约中心 · 注册发现 · 声明式编排 · 事件总线 · MCP 工具面 · [愿景](docs/vision.md)

![plugin-hub 架构总览](docs/assets/hero.png)

*仓库自带的 Web 控制台：插件目录 + 在线实例 + 聚合后的 MCP 工具面。共 13 个页面——流程编辑器、调用链瀑布、治理快照等，见[控制台](#控制台)。*

![控制台 — 插件目录](docs/assets/console-plugins.png)

[![CI](https://github.com/mahingbun-dev/plugin-hub/actions/workflows/ci.yml/badge.svg)](https://github.com/mahingbun-dev/plugin-hub/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/mahingbun-dev/plugin-hub)](https://github.com/mahingbun-dev/plugin-hub/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

plugin-hub 是一个进程外插件架构的控制中台。核心只做「插座」，**不含任何业务语义**：业务能力全部由**插件**提供——独立容器、gRPC 接口、任意语言（默认 Go SDK），注册即用，无需重启中台。

它解决的是这类问题：多个业务系统需要统一的接入点、契约变更需要可控、调用链需要可观测、AI agent 需要通过 MCP 统一调度这些能力——而业务代码保持独立部署、独立技术栈、独立发布节奏。

## 目录

- [产品特性](#产品特性)
- [为什么是它](#为什么是它)
- [架构](#架构)
- [核心概念](#核心概念)
- [快速开始](#快速开始)
- [Agent 接入（MCP）](#agent-接入mcp)
- [HTTP API 参考](#http-api-参考)
- [插件开发](#插件开发)
- [配置](#配置)
- [部署](#部署)
- [运维](#运维)
- [文档](#文档)
- [仓库结构](#仓库结构)
- [开发](#开发)

## 产品特性

| 特性 | 说明 |
|---|---|
| **字段级契约治理** | 插件注册时提交 proto descriptor，中台做字段级兼容检查——新增可选字段放行，删字段 / 改类型 / 改编号拒绝。契约漂移在注册时就被拦下，而不是在运行时爆炸 |
| **声明式编排** | DAG 流程：可视化编辑器拖拽连线，保存草稿 / 发布两段式，上下游消息全限定名对不上当场拦住 |
| **同步 + 异步执行** | 同步链直返逐节点结果；异步链走 Redis Stream 总线，自带消费组、接管重投、死信与幂等 |
| **MCP 工具面** | 内置 18 个工具，插件工具自动聚合（`插件名__工具名`），agent 拉一次 `tools/list` 即见全部能力 |
| **实例级治理** | 每实例并发上限、熔断（跳闸 / 半开探测）、连续失败计数；实例下线后其工具同步从 MCP 消失 |
| **全链路可观测** | W3C trace 传播、span 落库、可选 OTLP 导出、调用链瀑布图 |
| **多语言 SDK** | Go / Python / Node / Rust / C# 五门，脚手架模板一键下载，包内自带 SDK 源码 |
| **鉴权插件化** | 中台内不含鉴权逻辑；由鉴权插件回答「这个凭证有哪几个权限位」，中台只认权限位 |

## 为什么是它

| 替代方案 | 它是什么 | 在「多系统统一接入 + AI 调度」上的短板 | 什么时候仍该选它 |
|---|---|---|---|
| 进程内插件机制（DI 容器、钩子注册表） | 插件长在服务自己的进程里 | 同进程同命运：加能力 = 重新部署宿主；单一语言栈；契约漂移最多靠 review 拦 | 单团队、单语言、单体部署就够用时 |
| API 网关插件生态（Kong、APISIX 等） | 插件扩展网关的流量路径 | 面向数据平面（鉴权、限流、改写）；承载业务能力并做跨插件编排不是它的模型 | 扩展点确实在 HTTP 流量上 |
| 每个系统各挂一个 MCP server | 各系统各自暴露 MCP 面 | agent 要接 N 个面；系统之间没有契约约束，也没有统一的编排与调用链 | 只有唯一一个系统时 |

plugin-hub 的取舍很明确：多跑一个中台（compose 自带 PG / Redis）与独立插件进程，换来跨系统的注册时契约检查、跨插件编排、单一 MCP 聚合面与统一可观测。

## 架构

```
        hub.example.com（TLS）· <host-ip>（MCP 直连）          插件（任意主机）
                        │                                              │
        ┌───────────────┼───────────────┐                              │
        │ :8081 HTTPS   │ :8096 HTTP    │ :8094 TLS http2              │
        │ 控制台同源面   │ MCP IP 直连    │ 插件面 gRPC（TLS 终结）        │
        ▼               ▼               ▼                              │
     nginx ──127.0.0.1:8095──▶  plugin-hub  ◀──────── grpc_pass ────────┘
                                    │        （转发至 0.0.0.0:8093）
                     ┌──────────────┴──────────────┐
                     ▼                             ▼
         PostgreSQL（plugin_hub 库，:55432）  Redis Stream（db /2，:56380）
```

| 面 | 中台监听 | 对外 | 承载 |
|---|---|---|---|
| HTTP 面 | `127.0.0.1:8095` | nginx `/hub-api/`（与控制台同源）、`/mcp` | Ingress / Admin / MCP / 健康 / 指标 |
| 插件面 | `0.0.0.0:8093`（gRPC） | nginx `8094 ssl http2` | 注册 / 心跳 / 回调 |
| 运维面 | unix socket | 容器内 `hubctl` | 主机面救生通道 |

设计要点：

- **插件与中台不要求同机**。插件自注册时上报自己的可达地址，中台**注册时即做可达性探测**，探不通直接拒绝——避免「注册成功但永远调不通」。
- **HTTP 面只绑回环**，对外一律经 nginx：与控制台同源（零跨域、SSO Cookie 天然携带），TLS 与限流收口在 nginx。
- **依赖自带**：PostgreSQL 与 Redis 由 compose 编排提供（非默认端口，避免与同机其他服务相争），见[部署](#部署)。

### 技术栈

| 层 | 选型 |
|---|---|
| 中台 | Rust：axum + tonic + sqlx + PostgreSQL + redis-rs + tracing + metrics-exporter-prometheus |
| 插件 SDK | Go（默认）；契约由 protobuf 定义，任意语言可实现 |
| 控制台 | Vue 3 + Element Plus + Vue Flow（本仓库 console/），由 nginx 静态 serve |

### 控制台

本仓库 [`console/`](console/) 自带 Web 控制台（Vue 3 + Element Plus + Vue Flow 的独立前端，`pnpm dev` 即起，对接中台 HTTP 面无需登录——管理面默认无内置鉴权；生产部署与 HTTP 面同源，零 CORS）。共十三个页面（十一个菜单项 + 从列表点进去的流程拓扑 / 调用链详情）：

| 页面 | 内容 |
|---|---|
| 插件目录 | 插件 / 版本 / 契约 / MCP 工具 / 实例，含契约影响面（改字段前先看两端） |
| 实例健康 | 已注册实例、心跳新鲜度、自报地址（跨机插件一眼可辨） |
| 编排流程 + 拓扑 | DAG 渲染、草稿/已发布/历史修订切换、校验问题在图上标红 |
| 编排编辑器 | 加节点、拖动连线、节点属性、即时本地校验、保存草稿、发布 |
| 执行记录 + 详情 | 状态 / 耗时 / 失败原因、节点耗时条、逐节点明细 |
| 调用链 + 瀑布图 | 按 `parent_span_id` 建树、缩进与时间轴对齐 |
| 死信 | 查看与重放 |
| 触发器 | cron / MQ 的登记、启停、删除 |
| 治理 | 并发占用、熔断状态、连续失败、累计放行 |
| 版本升级 | 找出锁在旧版本上的节点——灰度升级的操作台 |
| 插件审计 / 插件开发接入 | 插件注册拒绝与互调授权审计；脚手架模板下载 |

本地启动（数据库配置、后端、前端三步完整指引）见 [console/README.md](console/README.md)。

## 核心概念

### 插件模型

每个插件必须提供两部分，缺一不可：

1. **插件体**（`Handle`）——自主实现的数据输入输出；
2. **数据校验器**（`Validate`）——数据进入中台后先经校验器，通过后才进插件体。

插件在 manifest 中声明消费 / 生产的消息类型与 MCP 工具，中台据此完成三件事：注册时做字段级兼容检查；保存编排时校验上下游消息能否对接；把插件能力聚合进 MCP 工具面。

### 编排与执行

- 流程是 DAG，**草稿 / 发布两段式**：保存草稿永远不会被拒（返回 `blocked` 与 `issues` 供修正），发布前重新校验、有阻断问题才拒绝——把「编辑的自由」和「生效的严肃」分开。
- **同步触发**返回逐节点结果；**异步触发**入总线队列立刻返回句柄（202），由常驻消费者执行，失败自动接管重投，超过投递上限进死信，可在控制台查看与重放。

### MCP 工具面

`POST /mcp`，Streamable HTTP，供 agent 使用。

| 类别 | 工具 |
|---|---|
| 插件目录 | `list_plugins` · `get_plugin` · `list_instances` |
| 排障 | `list_register_rejections`（最近的注册拒绝：谁被拒、原因、重试次数） |
| 能力调用 | `invoke_plugin`（载荷先过插件校验器，通过才进插件体） |
| 编排 | `list_flows` · `get_flow` · `save_flow_draft` · `trigger_flow` · `trigger_flow_async` |
| 执行与追踪 | `list_runs` · `get_run` · `list_traces` · `get_trace` |

### 实例级治理

治理快照提供每实例的并发占用、熔断状态、连续失败与累计放行。注意口径：快照按**被调用过**建项——刚注册未调用的实例不在里面，已下线但留有失败计数的实例会保留（否则失败计数凭空归零）。判断「谁在线」用 `/admin/instances`。

### 管理面鉴权

中台内部不含鉴权逻辑，由插件承担。配置 `HUB_AUTH_PLUGIN`（插件名）后，管理面每个请求都会先问该插件：**这个凭证是谁、有哪几个权限位**。中台定义权限位，插件回答「有哪几个」——谁是管理员是平台权限体系的事，不在中台再抄一份。

| 权限位 | 管什么 |
|---|---|
| `hub:read` | 插件目录、编排定义、执行记录、调用链、死信列表、触发器列表 |
| `hub:invoke` | 触发编排、调 MCP 工具（会产生真实副作用） |
| `hub:edit` | 保存草稿、登记触发器（改的是未生效的那一份） |
| `hub:publish` | 发布（把草稿推成生产流量走的那一版） |
| `hub:admin` | 治理快照、死信重放、删除触发器 |

> 发布与改草稿是**两位**：并成一位等于让所有编辑者都能发布。

三条路径不受鉴权影响：`/health` 与 `/metrics`（中台是否存活不应需要凭证才能回答）、`/ingress`（鉴权按设计由插件自己承担）、`/blobs`（ingress 的延伸）。两条安全底线：**插件不可达时返回 503，绝不降级成匿名放行**；认不出来的路径交给 404 而不是凭空发明权限位——把「路径写错」表现成 403 会把真问题藏起来。

鉴权插件的实现要点：实现标准插件协议（`Validate` / `Handle`），在 `Handle` 里读凭证、查权限位、把权限位写回信封 meta 即可，参考 [docs/plugin-onboarding.md](docs/plugin-onboarding.md) 与 [examples/ping-chain](examples/ping-chain/)。

## 快速开始

前提：Rust stable（见 `rust-toolchain.toml`）、Docker（本地 PG / Redis）。

**1. 启动中台**（`DATABASE_URL` / `REDIS_URL` 缺一会拒绝启动；启动时自动跑数据库迁移）：

```bash
DATABASE_URL=postgresql://u:p@127.0.0.1:5432/plugin_hub \
REDIS_URL=redis://127.0.0.1:6379/2 \
HUB_HTTP_PORT=8092 \
cargo run -p hub-server
```

起来后 `curl http://127.0.0.1:8092/health` 应返回 `{"ok":true,...}`。

**2. 创建第一个插件**：

```bash
cd sdk/go
go run ./cmd/hub-plugin new order-reader --dir /tmp/order-reader
```

工程内含源码骨架、接入指南（含「接入四道关」）、契约一致性测试与随包 SDK 源码，拿到手即可构建运行。

> 首次 `go mod tidy` 需访问 Go 代理：`export GOPROXY=https://goproxy.cn,direct`；解析过一次之后不再需要网络。

**3. 实现两个方法并注册**（`hubkit.Run` 处理 gRPC 服务、自注册、心跳、被摘除后自愈与优雅退出）：

```go
func (p *Plugin) Validate(ctx context.Context, env *hubv1.Envelope) (*hubv1.ValidateResponse, error)
func (p *Plugin) Handle(ctx context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error)
```

**4. 调用它**：

```bash
curl -X POST http://127.0.0.1:8092/ingress/order-reader \
  -H 'Content-Type: application/json' \
  -d '{"payload": {"order_id": "SO-123"}}'
```

预期返回（200；字段与中台 `IngressResponse` 一致，`payload` 是插件 `Handle` 的返回载荷）：

```json
{
  "message_id": "01JBF3Z9X2M5Q7W8R4T6Y8U0VW",
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
  "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
  "plugin": "order-reader",
  "version": "0.1.0",
  "instance_id": "myhost-4123",
  "elapsed_ms": 9,
  "payload": { "…": "插件 Handle 返回的 JSON 载荷" }
}
```

完整接入流程见 [docs/plugin-onboarding.md](docs/plugin-onboarding.md)；两个插件互相发现与调用的可运行示例见 [examples/ping-chain](examples/ping-chain/)。

**5. 启动 Web 控制台（可选）**：

```bash
cd console && pnpm install && pnpm dev
# 打开 http://127.0.0.1:5180/plugins
```

插件目录、编排编辑器、调用链瀑布图等十三个页面全部可用。数据库配置与
完整本地调试环境说明见 [console/README.md](console/README.md)。

## Agent 接入（MCP）

MCP 为 HTTP 传输。本地开发连 `http://127.0.0.1:8092/mcp`；生产经 nginx 对外是 `https://hub.example.com/mcp`（同源面）或 `http://<host-ip>:8096/mcp`（IP 直连面）。

客户端配置示例（Claude CLI：`claude mcp add --transport http --scope user plugin-hub <url>`；ZCode：`~/.zcode/cli/config.json`）：

```json
{ "type": "http", "url": "http://127.0.0.1:8092/mcp" }
```

服务端有 DNS rebinding 防护（rmcp Host 白名单），经 nginx 对外时必须配置 `HUB_MCP_ALLOWED_HOSTS`（**一旦配置即替换默认的回环放行**，回环需显式列出）。若返回 `403 Forbidden: Host header is not allowed`，把客户端使用的 Host 加入白名单并重启容器。

## HTTP API 参考

**业务数据入口**（不鉴权——鉴权由插件承担）：

```
POST /ingress/{plugin}         载荷 JSON 对象 → 包成 google.protobuf.Struct 送入插件
  { "payload": {...}, "message_id"?: "...", "version"?: "...", "meta"?: {...}, "timeout_ms"?: 30000 }
→ 200 { plugin, version, instance_id, elapsed_ms, payload | payload_type_url + payload_base64 }
→ 422 校验器拒绝（带 issues）    → 404 插件未注册    → 502 插件不可达/超时
```

> **大载荷引用通道**：载荷超过 4MB 时不内联，响应改给 `payload_ref`（uri + sha256 + 大小），插件收到的信封里只有 uri；`GET /blobs/{id}` 取回内容，带 TTL（默认 1 小时）。请求体硬上限 8MB——必须大于内联上限，否则引用通道够不着；超限在 handler 之前就被挡成 413。

**管理面**（按设计不带中台内置守卫；配 `HUB_AUTH_PLUGIN` 后由插件承担鉴权）：

```
GET  /admin/plugins              插件列表（含版本数、在线实例数）
GET  /admin/plugins/{name}       插件详情：逐版本的契约、MCP 工具、实例
GET  /admin/instances            全部在线实例
GET  /admin/rejections           最近的注册拒绝留痕（谁被拒、拒绝码、原因、重试次数）
                                 ?plugin=<按插件过滤>&limit=<1..200，默认 50>
GET  /admin/messages/{fq_name}   某消息类型被谁生产、被谁消费（改契约前的影响面）
GET  /admin/governance           实例级治理快照（口径说明见「实例级治理」）
DELETE /admin/plugins/{name}/versions/{version}
                                 删除已登记版本（VERSION_CONFLICT / BREAKING_CHANGE
                                 拒死后的恢复操作；级联删契约、工具与实例，不可逆，
                                 随后清掉该插件的拒绝留痕）——与主机面 hubctl
                                 remove-version 同一条 store 路径
GET  /health  GET /metrics       探活与指标（不依赖数据库）
```

**插件脚手架模板**（不鉴权——是开发资料而非运行数据）：

```
GET  /plugin-templates                      有哪几门语言的模板（含构建时间、文件数、包大小）
GET  /plugin-templates/{lang}/download      下载一份可运行的插件工程（zip）
                ?name=<插件名>&package=<包名可选>
```

`name` 用与注册期同一条规则校验（`hub-registry` 同一函数），页面放行的名字注册时一定也放行；模板内容在编译期嵌进中台二进制（`crates/hub-templates`）。

**编排**：

```
GET  /flows                      流程列表：发布版本、修订数、是否有草稿
GET  /flows/{flow}               流程全貌：草稿、已发布版本、历史修订
POST /flows/{flow}/draft         保存草稿（保存不会被拒，返回 blocked 与 issues）
POST /flows/{flow}/publish       发布（发布前重新校验，有阻断问题则拒）
POST /flows/{flow}/trigger       同步触发，返回逐节点结果
GET  /runs  GET /runs/{run_id}   执行记录与逐节点明细
GET  /traces  GET /traces/{id}   调用链（按 trace 聚合，不按 span 铺开）
```

**异步与总线**：

```
POST /flows/{flow}/trigger-async 入队一次异步执行，立刻返回句柄（202）
GET  /dead-letters               死信列表（默认只看没重放过的）
GET  /dead-letters/{id}          死信详情
POST /dead-letters/{id}/replay   重放（载荷必须由调用方提供——死信里只有摘要）
GET  /triggers                   触发器列表（含已停用的，停用的排前面）
POST /flows/{flow}/triggers      登记或更新一条 cron / mq 触发器
POST /triggers/{id}/enabled      启用 / 停用
DELETE /triggers/{id}            删除
```

## 插件开发

### SDK 一览

**五门：Go / Python / Node / Rust / C#**，位于 `sdk/<语言>/`，各带一套脚手架模板（下载接口下发的就是它们）。清单与用法见各门 `README.md`。

> 插件之间要**互相发现、互相调用**，走中台的 `PluginGateway`（A→hub→B，不直连）：SDK 用法、`invokes` 权限声明与限额/防环行为见 [docs/plugin-onboarding.md](docs/plugin-onboarding.md) 的「插件互调与发现」，可运行的双插件示例在 [examples/ping-chain](examples/ping-chain/)。

以下以 Go 为例（其余四门行为对齐它）：

```bash
cd sdk/go
go run ./cmd/hub-plugin new order-reader --dir /tmp/order-reader
```

`sdk/go/` 六件套齐备：服务端骨架、契约定义（生成产物已提交，**不需要装 protoc**）、脚手架、mock 中台、契约一致性自检、调试工具。生成的插件用 `google.protobuf.Struct` 承载 JSON 载荷。

### 插件协议

插件必须实现 `hub.v1.PluginRuntime`：`Describe` / `Validate` / `Handle` / `HandleStream` / `Health`，并向 `hub.v1.PluginRegistry` 自注册（上报可达地址、manifest、FileDescriptorSet），此后按中台指定周期发心跳。完整定义见 `crates/hub-proto/proto/hub/v1/`。

### 插件侧约定

1. **中台上报的地址必须能被中台拨通**——注册时做可达性探测，探不通直接拒。
2. **校验器先行**——中台一定先调 `Validate`，通过才调 `Handle`；不通过则插件体零执行。
3. **强制无状态**——实例内存不保证跨调用保留；换来热切换零代价、可水平扩展。
4. **契约不可漂移**——同版本号的 manifest 与 proto 不可变更，改了请升版本号。
5. **HubState 的键前缀不含版本号**——键为 `hub:state:{插件名}:{namespace}:{key}`，同插件所有版本共用一个状态空间：升版本不清空状态（登录缓存这类正是要的），但两个版本写同一个 `namespace` 就是互相覆盖；要按版本隔离，把版本写进 `namespace`。
6. **`HubState.Publish` 的 subject 由中台覆盖**——信封的 `subject` 无条件换成插件身份（`kind=PLUGIN`、`id=插件名`），下游据此做的审计与二次确认全建立在这一条上。`target` 当前只认**已发布的 flow 名**（topic 订阅尚未实现）。三道闸以 `accepted: false` + `reason` 返回（**不是 gRPC 错误**，插件该改逻辑而不是重试）：**成环**（本次触发链上已有该目标）、**链过长**（8 跳）、**配额**（每插件每分钟 60 次，按插件隔离）；总线或数据库故障才是 gRPC 错误。

### 新增一门 SDK 语言

是**两件事**，量级差得很远：

1. **接入机制**（小）——建模板目录，在 `crates/hub-templates` 的 `build.rs` 加一行；中台侧、下载接口、控制台页面自动跟上。
2. **SDK 本身**（大）——一门语言里的独立小项目，要实现**自注册、心跳、被摘除后自愈、优雅退出**，工作量与可靠度单独算，不是「照着 Go 抄一遍」。

## 配置

完整清单见 [`deploy/.env.example`](deploy/.env.example)（每项都有取值缘由）。

**核心项**：

| 变量 | 默认 | 说明 |
|---|---|---|
| `HUB_HTTP_HOST` / `HUB_HTTP_PORT` | `127.0.0.1` / `8095` | HTTP 面。生产固定绑回环，只经 nginx 对外；本地开发用 `8092` |
| `HUB_GRPC_HOST` / `HUB_GRPC_PORT` | `0.0.0.0` / `8093` | 插件面。必须绑 `0.0.0.0`——插件可能在其他主机上 |
| `HUB_MCP_ALLOWED_HOSTS` | 空（仅回环） | MCP Host 白名单，逗号分隔；经 nginx 对外**必须配**。一旦配置即替换默认回环放行，回环要显式列出 |
| `HUB_AUTH_PLUGIN` | 空（不启用） | 承担管理面鉴权的插件名；启用前提是插件已部署，否则管理面谁都进不去 |
| `PG_PASSWORD` / `REDIS_PASSWORD` | **必填** | 由 compose 拼进连接串。**用纯字母数字**（`openssl rand -hex 24`）：URL 保留字符会让连接串解析失败，`$` 会被 compose 插值吞掉 |
| `OTLP_ENDPOINT` | 空（不导出） | span 已自存 PG，导出只是顺便推给外部 trace 后端 |

**运行时调优项**（默认即合理值，压测后按需调整）：

| 分组 | 变量（默认值） |
|---|---|
| 实例级治理 | `NODE_MAX_CONCURRENCY=32`（到顶背压快速失败）· `NODE_QUEUE_TIMEOUT_MS=100` · `BREAKER_FAILURE_THRESHOLD=5` · `BREAKER_COOLDOWN_SECS=10` |
| 异步总线 | `ASYNC_WORKERS=4` · `BUS_MAX_DEPTH=100000`（到顶 429）· `BUS_MAX_DELIVERY=5`（用完进死信）· `BUS_CLAIM_MIN_IDLE_SECS=60` |
| 数据留存 | `STREAM_RETENTION_HOURS=24` · `SPAN_RETENTION_DAYS=7` · `RUN_RETENTION_DAYS=90` · `AUDIT_RETENTION_DAYS=90` |
| 运维 | `LOG_LEVEL=info` · `HUB_OPS_SOCKET=/run/plugin-hub/ops.sock` · `BOOTSTRAP_TOKEN`（首次配置引导凭据，留空不启用） |

## 部署

依赖**自带**：PostgreSQL 与 Redis 由 `deploy/docker-compose.yml` 编排，与中台一同运行：

| | 镜像 | 端口 | 数据目录 |
|---|---|---|---|
| PostgreSQL | `postgres:16-alpine` | 55432 | `./data/pg` |
| Redis | `redis:7-alpine` | 56380 | `./data/redis` |

端口刻意避开默认值（避免与同机其他服务相争）；连接串由 compose 从分量拼出，密码只在 `.env` 写一处；数据绑宿主机目录而非 named volume。

**最小上线**：

```bash
git clone https://github.com/mahingbun-dev/plugin-hub && cd plugin-hub
cp deploy/.env.example .env          # 至少填 PG_PASSWORD / REDIS_PASSWORD
docker compose -p plugin-hub -f deploy/docker-compose.yml --env-file .env up -d
curl http://127.0.0.1:8095/health    # {"ok":true,...}
```

**对外暴露**（生产形态）：

1. **nginx**：`deploy/nginx/hub-api-location.conf` 并入你的 HTTPS server 块（与控制台同源）；`plugin-hub-grpc.conf`（8094，插件面 TLS 终结）与 `plugin-hub-ip-mcp.conf`（8096，MCP IP 直连）放到 `/etc/nginx/conf.d/`，把 `server_name` 与证书路径改成你的，`nginx -t && nginx -s reload`。
2. **跨机插件**：`.env` 里配 `HUB_PLUGIN_PUBLIC_ADDR=https://your-domain:8094`（插件自报地址的前缀），插件所在主机要能拨通它。

### 备份与恢复

[`deploy/backup-pg.sh`](deploy/backup-pg.sh) 用容器自带 `pg_dump` 导出到备份目录，清理超过 `BACKUP_KEEP_DAYS`（默认 14）天的旧备份。装到宿主机 crontab：

```bash
sudo cp deploy/backup-pg.sh /opt/plugin-hub/ && sudo chmod +x /opt/plugin-hub/backup-pg.sh
sudo crontab -e
```

```
PATH=/usr/local/bin:/usr/bin:/bin
17 3 * * * /opt/plugin-hub/backup-pg.sh >> /opt/plugin-hub/backups/backup.log 2>&1
```

> ⚠️ `PATH=` 首行不能省：docker 通常在 `/usr/local/bin`，而 cron 默认 PATH 只有 `/usr/bin:/bin`——少了它备份静默消失，唯一痕迹是 backup.log 里一行 `docker: command not found`。脚本自身也会检查 `docker` 并明确报错。

**恢复**（备份带 `--clean --if-exists`，是覆盖式恢复）：

```bash
gunzip -c backups/plugin_hub-YYYYMMDD-HHMMSS.sql.gz \
  | docker exec -i plugin-hub-pg psql -U plugin_hub -d plugin_hub -p 55432
```

> ⚠️ `-p 55432` 不能省：PG 在非默认端口上，unix socket 名随之是 `.s.PGSQL.55432`，`psql` 默认找 5432 的那个。

## 运维

### 主机面运维通道

管理面按设计全插件化，鉴权插件坏掉时靠这条通道救回：

```bash
docker exec plugin-hub hubctl status
docker exec plugin-hub hubctl list-plugins
docker exec plugin-hub hubctl delete-plugin <name> --yes    # 不可逆，必须显式确认
```

### 排障速查

| 症状 | 原因与处理 |
|---|---|
| `/mcp` 返回 `403 Forbidden: Host header is not allowed` | 客户端 Host 不在 `HUB_MCP_ALLOWED_HOSTS`；加入后重建容器。注意白名单一旦配置即替换默认回环放行 |
| 插件注册成功但调用 502 | 自报地址不可达；检查 advertise 地址、容器网络与防火墙。注册时的可达性探测未过根本注册不上 |
| 中台启动报 `invalid port number` | `PG_PASSWORD` 含 URL 保留字符（`/` `#` `?`），被拼进连接串所致；改用纯十六进制密码 |
| 两个插件互相顶掉、工具面缺一半 | `*_INSTANCE_ID` 相同（缺省「主机名-PID」在 host 网络下必然撞）；每个实例配不同的显式 instance_id |
| 插件容器 Up 但实例 0 个、工具面缺一门（看容器日志每 5 秒 `VERSION_CONFLICT` 重试） | 改了 manifest / 工具描述 / input schema 但 `PLUGIN_VERSION` 没升——同版本号必须契约一致。正确修法是升版本号重新构建发布；应急可用 `DELETE /admin/plugins/{name}/versions/{version}`（或 `hubctl remove-version`）删旧登记让插件重新注册。拒绝原因在 `GET /admin/rejections` 与 MCP 工具 `list_register_rejections` 里可查 |
| cron 备份静默不再产出 | cron 默认 PATH 找不到 docker；crontab 首行补 `PATH=`，看 backup.log |
| 恢复时 `No such file or directory` | `psql` 默认找 5432 的 socket；补 `-p 55432` |
| 某些网络路径下 Go 插件 TLS 握手被重置（同路径 curl 正常） | 中间设备重置 TLS 1.3 握手；SDK 提供 `HUB_TLS_MAX_VERSION` 逃生口 |
| Go 在 macOS 上自签 CA 不生效 | Go 不读 `SSL_CERT_FILE`（走 Keychain）；跨机自测跑 Linux 容器 |

## 文档

| 文档 | 内容 |
|---|---|
| [docs/vision.md](docs/vision.md) | 愿景——「hub 链接万物」：设计哲学与生态图景（门禁、A2A、hook 触发、上下文共享） |
| [docs/design.md](docs/design.md) | 架构取舍、契约设计、数据模型、分期里程碑、风险与显式假设 |
| [docs/plugin-onboarding.md](docs/plugin-onboarding.md) | 插件接入全流程：四道关、契约一致性、调试与排障 |
| [deploy/.env.example](deploy/.env.example) | 全部环境变量及取值缘由 |
| `sdk/<语言>/README.md` | 各门 SDK 的清单与用法 |

## 仓库结构

```
crates/
├── hub-proto/          契约层：protobuf 定义与生成代码（唯一契约来源）
├── hub-flow/           编排 DSL、静态校验、执行计划
├── hub-contract/       descriptor 索引与字段级兼容检查
├── hub-store/          持久化：PostgreSQL + sqlx 迁移
├── hub-registry/       注册发现：自注册、心跳、摘除、实例选择
├── hub-plugin-client/  中台 → 插件方向的 gRPC 客户端（兼可达性探测）
├── hub-grpc/           插件 → 中台方向的 gRPC 服务端
├── hub-engine/         编排执行：同步链、异步链、实例级治理
├── hub-bus/            Redis Stream 总线：消费组、接管重投、死信、幂等
├── hub-observe/        span 落库、W3C trace 传播、OTLP 导出
├── hub-mcp/            MCP Streamable HTTP + 插件工具聚合
├── hub-ops/            主机面运维通道（unix socket）+ hubctl
├── hub-api/            HTTP 面：Ingress / Admin / 编排 / 健康 / 指标
├── hub-core/           配置与共享基础类型
├── hub-server/         二进制入口：装配后台任务与优雅退出
├── hub-templates/      插件脚手架模板（编译期嵌入二进制）
├── hub-mock/           内存版 mock 中台（SDK 测试用）
└── hub-testkit/        测试夹具：可配置的插件，起在真实 gRPC 上
sdk/                    Go / Python / Node / Rust / C# 五门插件 SDK
console/                Web 控制台 demo（Vue 3 + Element Plus + Vue Flow）
examples/ping-chain/    双插件互调（caller → hub → callee）的可运行示例
deploy/                 Dockerfile · docker-compose · nginx 配置 · 备份脚本
docs/                   架构设计与插件接入指南
```

## 开发

```bash
cargo build --workspace          # 构建
cargo test --workspace           # 测试（需要 DATABASE_URL 与 Redis）
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

集成测试用**真实 PG 与 Redis**（Redis 用 `db /9`，与应用的 `db /2` 分开），没有依赖时相关用例**失败而不是静默跳过**——注册与摘除、总线重投、治理并发竞态只能对着真东西验证。`.cargo/config.toml` 提供了与本仓库 CI 同约定的 `DATABASE_URL` 兜底（本地 PG 容器映射 55433），按注释准备一次即可零配置跑测试。

## License

版权所有 (c) 2026 mahingbun-dev（[github.com/mahingbun-dev](https://github.com/mahingbun-dev)），基于 [MIT](LICENSE) 许可证发布。
