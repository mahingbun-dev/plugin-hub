# anc-hub 设计决策与实施计划

> 本文记录经完整需求访谈后确认的全部架构决策、契约设计与分期计划。决策表里的「代价」列是刻意保留的——它们是被知情接受的成本，不是待办事项。

## 一、为什么做这个

原 `anc-gateway` 是一套 Node.js/TS 反向代理网关（Redis FIFO 锁排队 + node:vm 注入 + MCP 管理面）。经评估后**整体废弃**：不做迁移、不做纳管、不保留运行。

取而代之的是一个 Rust **控制中台**，思路从「做一条代理链路」转为「做一个插座」：
- 中台只提供契约规范、注册发现、声明式编排、插件间总线、MCP 工具面与治理
- 业务能力全部下沉到**进程外插件**，由各团队用自己熟悉的技术栈独立开发、独立发版
- 插件注册即用，中台不重启；插件与中台不要求同机

原有能力（锁排队、请求改写、反向代理）后续以**插件**形态重做，中台核心不重新背上业务包袱。

## 二、目标架构

```
                         hub.example.com（复用现有证书）
                                    │
                    ┌───────────────┴────────────────┐
        :8081 HTTPS │                     :8094 TLS  │
   前端面 /hub-api/ │                    插件面 gRPC  │
                    ▼                                ▼
            ┌───────────────┐                ┌──────────────────┐
            │  anc nginx    │                │   anc nginx      │
            │  (现有实例)    │                │  grpc_pass       │
            └───────┬───────┘                └────────┬─────────┘
                    │ 127.0.0.1:8095                  │ 127.0.0.1:8093
                    ▼                                 ▼
   ┌──────────────────────────────────────────────────────────────┐
   │                     anc-hub（Rust，host 网络）                 │
   │  ┌────────────┬────────────┬────────────┬─────────────────┐  │
   │  │ 契约中心    │ 注册发现    │ 编排引擎    │ MCP 工具面      │  │
   │  │ descriptor │ 心跳/探测   │ 同步/异步链  │ 插件工具自动聚合 │  │
   │  └────────────┴────────────┴────────────┴─────────────────┘  │
   │  ┌────────────┬────────────┬────────────┬─────────────────┐  │
   │  │ 事件总线    │ 触发器      │ 可观测      │ 主机面运维通道   │  │
   │  │Redis Stream │HTTP/MQ/cron│span+OTel   │ 本地 socket CLI │  │
   │  └────────────┴────────────┴────────────┴─────────────────┘  │
   └───────┬─────────────────────────┬────────────────────────────┘
           │ PG 127.0.0.1:55432/anc_hub │ Redis 127.0.0.1:56380/2
           │ （部署自带的容器，见下）    │
           ▼                         ▼
    ┌──────────────────────────────────────────────┐
    │  插件 A（Go，本机容器）  插件 B（Java，远程服务器）│
    │  插件 C（Node，远程服务器）... 任意语言 + gRPC   │
    └──────────────────────────────────────────────┘
                    ▲
                    │ 浏览器（SSO 登录态，同源）
            anc-frontend 新增「控制中台」模块
```

## 三、已确认的设计决策

| 维度 | 决策 | 主要代价 |
|---|---|---|
| 中台定位 | 只做插座：契约 + 注册发现 + 编排 + 总线 + MCP + 治理，无业务语义 | 中台本体初期无业务能力，价值靠插件生态体现 |
| 插件形态 | **纯进程外插件**：独立容器 + gRPC，任意语言 | 每次调用有序列化 + 网络开销（同机亚毫秒级） |
| 插件 SDK | **五门**（Go / Python / Node / Rust / C#），六件式完整交付；脚手架模板由控制台下发。Java 本期不做。加一门语言 = 接入机制（小，改两处）+ SDK 本身（大，自注册/心跳/自愈/优雅退出一套） | SDK 自身开发与维护量不小，且**每门语言各算一份** |
| 调用协议 | gRPC / Protobuf（单一契约来源） | 远程调试需 grpcurl，需配套调试 CLI |
| 信封契约 | **统一 Envelope + 多载荷类型**，protobuf 定义 | HTTP 语义需作为一等字段提前设计好 |
| 校验器 | **随插件发布**（同一插件导出 `Validate` + `Handle`） | 改一条校验规则也要发插件版本 |
| 编排 | **中台声明式 DAG**，注册与保存时做静态契约校验 | 需实现 flow 模型、DAG 校验与执行引擎 |
| 执行语义 | **同步链 + 异步链双模式** | 两套执行路径与两套测试面 |
| 事件总线 | **自研，底座 Redis Stream**（非 RocketMQ） | 大数据量下 Redis 内存压力，只适合中短途队列 |
| 触发器 | 统一触发模型：HTTP/gRPC、MQ 订阅、cron、agent(MCP) | 触发器抽象要设计得当 |
| 载荷边界 | 内联 ≤4MB；超限走引用传递；逐条结果走 streaming；长任务走异步 flow | 需实现存储通道与流式处理 |
| 注册发现 | 插件主动自注册 + 心跳 + 健康探测 + 超时摘除 | 需处理多副本合并为逻辑插件 |
| 契约校验 | 消息全限定名一致 + **字段级兼容检查**（新增可选字段放行） | 需实现 descriptor 存储与 diff |
| 生命周期 | flow 锁插件版本 + **多版本实例共存** → 天然灰度 | 实例表按 (插件名, 版本) 分组，控制台要能表达版本锁定 |
| 治理 | 中台侧实例级：并发上限 / deadline / 熔断半开 / 背压 | 治理参数需调优，默认值要保守 |
| 鉴权 | **全插件化（含管理面）**；主机面运维通道作为唯一逃生口；**插件面不鉴权** | 正常 HTTP 面的管理面完全没有中台内置鉴权——这是设计意图，不是遗漏 |
| 权限体系 | 复用 anc 平台用户与功能权限，中台只定义权限位 | 中台对 anc 鉴权服务形成依赖 |
| MCP | 插件 manifest 声明 tools → 中台聚合（`plugin__tool` 前缀），热注册触发 list_changed | 需工具名冲突检测与调用链审计 |
| Agent 权限 | 读 + 调用 + 改草稿；**发布需人审批** | 多一套草稿/发布状态机 |
| 可观测 | 自存 span（W3C Trace Context）+ UI 可查 + OTel 导出 | 需自建 span 存储与查询（分区表 + 定期清理） |
| 数据留存 | Stream 24h / span 7d / 审计 90d / 报文摘要 2KB 截断（全量落库默认关） | 需定时清理任务 |
| 容量目标 | 1000–2000 TPS 编排、5000 msg/s、500 并发 flow、单 flow ≤32 节点、编排开销 P99 < 20ms | 超过目标需多实例方案 |
| Rust 栈 | axum + tonic + sqlx + PostgreSQL + redis-rs + tracing + metrics-exporter-prometheus | sqlx 编译期宏在离线环境需 `.sqlx` 缓存 |
| 控制台 | **在 `anc-frontend` 仓库开发**（Vue3 + Element Plus + Vue Flow），产物由 nginx 静态 serve | 前端与中台分属两个仓库，需对齐 API 契约 |
| 部署 | ver server + 复用同一台 nginx 与证书；PG 新建库 `anc_hub`；Redis 新 db `/2` | 中台与 anc 共享服务器资源与故障域 |
| 离线构建 | rust 工具链镜像 + `cargo vendor` 快照（GitLab Generic Package）+ 服务器容器内 `cargo build --offline` | 首次全量编译 5–15 分钟 |
| 交付节奏 | 四期：骨架 → 编排 → 异步 → 治理与可视化 | 完整形态看到得最晚 |

## 四、契约设计

### 4.1 Envelope（统一信封）

```protobuf
message Envelope {
  string message_id   =  1;   // ULID，幂等键来源
  string trace_id     =  2;   // W3C Trace Context
  string span_id      =  3;
  string flow_id      =  4;
  string run_id       =  5;   // 本次执行
  string node_id      =  6;   // 当前节点
  string tenant       =  7;
  Subject subject     =  8;   // 调用主体（人/agent/插件/系统）
  int64  deadline_ms  =  9;   // 绝对 deadline，逐跳递减
  PayloadType type    = 10;   // REQUEST / EVENT / COMMAND / RESULT
  google.protobuf.Any payload = 11;  // 业务载荷（全限定名即契约标识）
  PayloadRef ref      = 12;   // 超限载荷的引用（URI + 摘要 + 大小）
  map<string,string> meta = 13;
}

message Subject {
  SubjectKind kind = 1;       // HUMAN / AGENT / PLUGIN / SYSTEM
  string id        = 2;       // userCode / agent 会话 id / 插件名
  string tenant    = 3;
  repeated string scopes = 4;
  string origin    = 5;       // 原始凭证来源（不落库，仅透传）
}
```

**边界规则**：`payload` 内联 ≤4MB；超限改填 `ref`（中台存储通道返回的 URI + 摘要）；逐条结果用 server-streaming；耗时任务走异步 flow。

### 4.2 插件面接口

```protobuf
service PluginRuntime {                      // 中台 → 插件
  rpc Describe(DescribeRequest) returns (PluginManifest);   // 拉取 manifest
  rpc Validate(ValidateRequest) returns (ValidateResponse); // 校验器（必备）
  rpc Handle(HandleRequest) returns (HandleResponse);       // 插件体（必备）
  rpc HandleStream(HandleRequest) returns (stream HandleResponse);
  rpc Health(HealthRequest) returns (HealthResponse);
}

service PluginRegistry {                     // 插件 → 中台（经 nginx:8094）
  rpc Register(RegisterRequest) returns (RegisterResponse);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
  rpc Unregister(UnregisterRequest) returns (UnregisterResponse);
}

service HubState {                           // 插件 → 中台：外置状态 API（插件强制无状态）
  rpc KvGet / KvPut / KvDelete / KvScan
  rpc CacheGet / CachePut
  rpc Publish(PublishRequest) returns (PublishResponse);    // 触发下游（防环由中台负责）
}
```

**注册流程**：插件容器启动 → `Register`（上报插件名/版本/advertise 地址/manifest/FileDescriptorSet）→ 中台做 ① 可达性探测（试连 + Health）② descriptor 解析与字段级兼容检查 ③ 工具名冲突检测 → 通过则入注册表并开始心跳计时；失败返回结构化拒绝原因。

**心跳**：定时 unary（非长连接 stream），避开 gRPC 经 nginx 的保活坑。3 次未续期摘除实例，保留插件定义。

### 4.3 兼容性校验规则

| 变更 | 判定 |
|---|---|
| 新增 optional 字段 | ✅ 放行 |
| 新增 enum 值、新增 message | ✅ 放行 |
| required → optional | ✅ 放行 |
| 删除字段 / 改字段类型 / 改字段编号 / 改包名或消息名 | ❌ 拒绝注册 |
| 上游 produces 与下游 consumes 全限定名不一致 | ❌ 拒绝保存 flow |

descriptor 基线存 `plugin_contracts.descriptor_blob`，按 (消息全限定名, 版本) 留历史。

## 五、数据模型（PostgreSQL 库 `anc_hub`）

> 下表的列名以**实现为准**（迁移文件在 `crates/hub-store/migrations/`）。
> 计划阶段写的列名有几处与最终实现不同，这里已订正——一份与代码对不上的数据模型
> 比没有更糟。

| 表 | 关键列 | 说明 |
|---|---|---|
| `plugins` | name(唯一)、description、owner | 插件定义（逻辑插件） |
| `plugin_versions` | plugin_id、version、manifest、descriptor | 语义化版本，多版本可共存；`descriptor` 是兼容检查的基线 |
| `plugin_instances` | version_id、instance_id(唯一)、advertise_addr、status、source_ip、last_heartbeat_at | 按 (插件, 版本) 分组；`source_ip` 是注册来源审计 |
| `plugin_contracts` | version_id、direction、fq_name | 消息类型契约（produces / consumes） |
| `plugin_tools` | version_id、name、description、input_schema_json、requires_approval | manifest 声明的 MCP 工具 |
| `flows` | name(唯一)、description、published_revision | 一条编排；草稿与历史在 `flow_revisions` |
| `flow_revisions` | flow_id、revision、definition、status(draft/published/archived)、validation、created_by、created_at、published_at | 版本化与回滚；`validation` 是保存时的校验结果快照 |
| `triggers` | flow_id、kind(cron/mq)、name、config、enabled、last_fired_at、last_error、fired_count | 统一触发模型；`last_error` 成功时清空 |
| `runs` | run_id、flow_id、flow_revision、trace_id、subject、trigger、status、input_summary、error、started_at、finished_at | 一次执行；`started_at` 取最早那个节点的开始时刻 |
| `run_nodes` | run_id、node_id、plugin、version、instance_id、attempt、status、duration_ms、io_summary、error | 节点执行与失败定位 |
| `spans` | trace_id、span_id、parent_span_id、run_id、node_id、name、duration_ms、status、attributes | 7 天清理 |
| `dead_letters` | stream、stream_id、run_id、flow_name、node_id、attempts、error、payload_summary、replayed_run_id | 查看与重放；「同一条消息」判据是 `(stream, stream_id)` |
| `idempotency_keys` | key、run_id、expires_at | at-least-once 下的去重；节点级键是 `node:{run}:{node}` |
| `payload_blobs` | id、size、sha256、bytes、expires_at | 超限载荷的引用通道；带 TTL 的限时中转，不是归档 |

**`runs.started_at` 的语义值得单独说一句**：它是「这次执行何时开始」，取的是该 run
里最早那个节点的开始时刻。这条曾经写错过——赋值时用的是结算那一刻的 `now`，
于是 `started_at` 与 `finished_at` 恒等，而控制台的执行耗时正是拿这两列相减算的，
算出来永远是 0。**0ms 是个看起来很正常的值**，不会有人怀疑它，只会以为执行很快。
回归测试 `crates/hub-engine/tests/run_timing.rs` 用一个带固定延迟的插件钉住它。

## 六、仓库结构

```
anc-hub/
├── Cargo.toml                    # workspace
├── rust-toolchain.toml           # 本地开发用 stable；**不参与**离线构建（见第八节）
├── crates/
│   ├── hub-proto/                # proto + prost/tonic 生成
│   ├── hub-core/                 # 配置与共享基础类型
│   ├── hub-flow/                 # 编排 DSL、静态校验、执行计划
│   ├── hub-contract/             # descriptor 存储与字段级兼容检查
│   ├── hub-registry/             # 注册/心跳/探测/摘除
│   ├── hub-engine/               # 同步链/异步链执行引擎 + 实例级治理
│   ├── hub-bus/                  # Redis Stream 总线（M3）
│   ├── hub-store/                # sqlx 仓储（迁移在 crates/hub-store/migrations/）
│   ├── hub-api/                  # axum：Ingress/Admin/编排/health/metrics
│   ├── hub-mcp/                  # MCP Streamable HTTP + 工具聚合
│   ├── hub-plugin-client/        # tonic 客户端
│   ├── hub-observe/              # span/trace/指标
│   ├── hub-ops/                  # 主机面运维通道 + hubctl
│   ├── hub-templates/            # 插件脚手架的注册/渲染/打包（控制台下发用）
│   ├── hub-mock/                 # 本地 mock 中台：给开发者的 L3 替身，无外部依赖
│   ├── hub-testkit/              # 测试夹具：可配置的插件，起在真实 gRPC 上
│   └── hub-server/               # bin 入口：装配后台任务与优雅退出
├── sdk/go/                       # Go 插件 SDK（六件套）+ 脚手架模板源
├── deploy/                       # Dockerfile / compose / nginx 片段
└── scripts/cargo-vendor-sync.sh
```

控制台 UI 不在本仓库，位于 `anc-frontend`（新增「控制中台」模块，十个页面；
清单与取舍见 README 的「控制台」一节）。

### 端口分配

| 端口 | 用途 | 绑定 |
|---|---|---|
| 8095 | 中台 HTTP 面（Ingress/Admin/MCP/健康/指标） | 127.0.0.1（经 nginx 8081 `/hub-api/`） |
| 8093 | 中台插件面 gRPC | 0.0.0.0（经 nginx 8094 `grpc_pass`） |
| 8094 | nginx 插件面（TLS，`listen 8094 ssl http2`） | 0.0.0.0 |
| 55432 | 自带的 PostgreSQL（`deploy/docker-compose.prod.yml`） | 127.0.0.1 |
| 56380 | 自带的 Redis | 127.0.0.1 |
| 8090/8091 | ~~旧 Node 网关~~（已释放） | — |
| 8092 | ~~中台 HTTP 面~~ → **归 anc 平台后端** | — |

8092 是本项目最初选的端口，但 UAT 上它已经属于 anc 平台后端——`/etc/nginx/conf.d/anc.conf`
的 8081 server 块里 `location /api/` 与 `location = /health` 都指向它。抢这个端口会直接
打挂现有服务，故中台改到 8095。

PG 与 Redis 用非默认端口（55432 / 56380）不是为了好看：这台机器上还跑着 anc 平台、
ai-evo、Dify 等多套服务，6379 那个位置其实是宿主裸装的 `redis-server`。
用独立端口既不与之相争，也让 `ss -lntp` 的输出一眼能认出归属。

## 七、分期实施计划

> 每期结束在 UAT 验收后再开下一期。开发用 `git worktree` 隔离；每期的开发与验证由不同 agent 承担。

### M0 旧资产下线与仓库起步
1. 确认 UAT 旧实例状态（docker ps / nginx access log / 文档引用检索）
2. 下线：`compose down`、归档 `.env` 与 `data/gateway.db`、摘除 nginx vhost、镜像保留 30 天
3. 仓库起步：删除 Node 代码、重写 README、铺 Rust workspace

### M1 骨架 —— 真实插件能被接入并被调用 ✅ 已实现

crate 结构：

```
crates/
├── hub-proto/          契约层：protobuf 定义与生成代码（唯一契约来源）
├── hub-contract/       descriptor 索引与字段级兼容检查
├── hub-store/          持久化：PostgreSQL + sqlx 迁移
├── hub-registry/       注册发现：自注册、心跳、摘除、实例选择
├── hub-plugin-client/  中台 → 插件方向的 gRPC 客户端（兼可达性探测）
├── hub-grpc/           插件 → 中台方向的 gRPC 服务端
├── hub-engine/         调用执行（M1 单插件，M2 长成 DAG 执行器）
├── hub-mcp/            MCP 工具面
├── hub-ops/            主机面运维通道 + hubctl
├── hub-api/            HTTP 面：Ingress / Admin / 健康 / 指标
├── hub-core/           配置与共享基础类型
├── hub-server/         二进制入口
└── hub-testkit/        测试夹具：可配置的插件，起在真实 gRPC 上
sdk/go/                 Go 插件 SDK 六件套
examples/auth-plugin/   鉴权插件（参考实现）
```

**验收状态**：Go 插件（脚手架生成）自注册 → `/admin/plugins` 与 MCP `get_plugin` 可见
（含契约与工具）→ agent 经 MCP `invoke_plugin` 调用成功 → 校验拒绝返回结构化问题。
鉴权插件的四条路径（有效凭证、凭证过期、SSO 不可达、缺 cookie）在真实链路验证通过。
**跨网络验收已在 UAT 完成**（2026-09-18）：插件跑在独立 docker 网络里（独立 IP、
独立网络命名空间），经 `https://hub.example.com:8094` 的 TLS 端点注册、自报非回环地址。
nginx 的 gRPC 代理、可达性探测、自报地址三条链路都因此被走到，另有
`source_ip=172.30.99.10`（同机那条恒为 127.0.0.1）证明 `grpc_set_header X-Real-IP` 生效。
端到端也通：经 ingress 调用时中台拨到该地址、插件的校验器与业务体都真的执行。
**已用另一台物理机复验**：插件跑在开发机（Mac）的容器里，经反向隧道把插件面交给 UAT 的中台。
中台记录里 `source_ip=203.0.113.20`（那台机器主动连过来的 IP），经 ingress 调用时响应点名的
是隧道地址 `http://203.0.113.10:19002`，校验器与业务体都在那台机器上执行。
**这条复验有折扣**：企业网不从 `203.0.113.x` 反向路由到 `203.0.113.x`，UAT 拨不到开发机，
所以插件自报的是 UAT 上一个由开发机主动建连的中继端口——「中台拨插件自己主机的地址」
这一形态**没有被覆盖**，真正的跨网段直拨需要一台与 UAT 双向可达的机器。

#### 实现中修正的设计决策

写代码过程中撞上的问题，改动了原计划：

| # | 原计划 | 实际做法 | 为什么 |
|---|---|---|---|
| 1 | 插件提交的 descriptor 必须有内容 | **空 descriptor 合法** | 只用 `google.protobuf.Struct` 承载 JSON 的插件（直接响应 agent 调用那一类）没有自己的 proto。「声明的类型必须存在于 descriptor」这条检查仍然拦得住乱声明 |
| 2 | 用 `sqlx` 编译期宏 | 改用运行时 `query_as` | 编译期宏要求构建时能连上数据库或维护 `.sqlx` 缓存，会让「本地 PG 没起就编译不过」。偏差由针对真实 PG 的集成测试兜住 |
| 3 | （原计划未涉及） | 字段改名判 **Warning** 而非 Breaking | protobuf 的字段身份是编号，名字不进二进制编码，插件间走 gRPC 流改名不会读错；但 JSON 面受影响，所以报出来不静默 |
| 4 | `hub-mcp` 用 SDK 的客户端传输做测试 | 直接调工具的 `pub` 方法 + 一条真实 `initialize` | SDK 的客户端会拉 reqwest 与 TLS provider（aws-lc-sys 需要 cmake 与 C 编译链），为一条测试把离线镜像压重不划算 |
| 5 | 运维通道「只有 root 能访问」 | 明确为**文件系统权限**（socket 0600） | 与「网络面的管理面完全无守卫」是两回事，文档里必须说清，否则会被当成同一类风险 |
| 6 | descriptor 里的 well-known 类型也要存在 | `google.protobuf.*` 豁免存在性检查 | 它们是平台的一部分而不是插件的契约；豁免范围不含 `google.type.*` 等其他 google 命名空间 |

### M2 编排 —— 多插件链路与可视化 ✅ 已完成

已完成：

- **`hub-flow`**：编排 DSL；静态校验（DAG 无环 / 无汇聚 / 上下游契约能接上 / 版本约束可满足 /
  插件已注册 / 孤立节点告警）；执行计划（把 DAG 排成可并发的层）
- **`hub-store`**：`flows` / `flow_revisions` / `runs` / `run_nodes` / `spans` 数据模型；
  「草稿 → 发布」状态机（一条 flow 同时只有一份草稿，靠部分唯一索引兜住；发布把上一版
  留档为 archived）
- **`hub-engine`**：同步链执行引擎（同层并发、层间串行、失败短路、节点级超时与重试）、
  实例级治理（并发上限 / 熔断半开 / 背压）与编排服务（保存前校验、发布前再校验、执行留痕）
- **`hub-observe`**：span 落库 + W3C trace 传播 + OTLP/HTTP 导出（按 `OTLP_ENDPOINT` 启用）
- **HTTP 面**：草稿 / 发布 / 触发 / 执行记录 / 调用链；**MCP 面**：编排的读 + 改草稿 + 触发。
  **发布刻意不进 MCP**——它直接改变生产流量走向，按设计 agent 只能改草稿
- **控制台**：编排流程列表、拓扑查看（Vue Flow 渲染 DAG、修订切换、校验问题在图上标红）、
  执行记录与详情、调用链瀑布图；以及 M4 范畴的 **Vue Flow 编排编辑器**

实现中明确下来的语义：

- **汇聚节点（入度 >1）在 M2 不支持**。汇聚不是「等它们都完成」那么简单，还要回答
  「合并后的信封以谁的载荷为准」。与其塞一个含糊的规则让人踩坑，不如先在保存时拦住。
- **校验拒绝不重试**。即使节点配了重试，校验拒绝也只跑一次——同样的数据重发结果还是
  拒绝，重试只会把一次失败放大成三次。
- **治理的粒度是实例而不是插件**。一个插件可以有多副本，其中一个副本所在的机器出问题
  不该牵连其他副本；按插件熔断会把整个插件打成不可用，那正好是这层想避免的结果。
- **背压快速失败，不无限排队**；**熔断与背压都不重试**——并发到顶或实例明确在冷却时
  重试只是把同一份压力再推一次，与「超时不重试」是同一条理由。
- **校验器拒绝算实例健康**。判据是「调用本身有没有成功」而不是「业务有没有成功」：
  插件正常干活了，只是数据没过规则。这条最容易写反，所以有专门的端到端用例守着。
- **超时不算实例故障**。预算由调用方决定（HTTP 面来自 `timeout_ms`），把一个调用方自己
  设的极小预算算成实例故障，等于让任何人用 `timeout_ms=1` 把健康实例对所有人打成熔断。
  真正挂死的实例由心跳巡检摘除，偶发变慢体现在耗时指标上——这两条比「按调用方预算熔断」
  干净得多。
- **只有半开探测能关掉熔断**。跳闸状态下回报的成功是**过期消息**（调用在跳闸之前就被
  放行了），放它关闸等于让冷却期形同虚设。同理，退回半开名额的资格也只属于探测的持有者。
- **熔断 → 503 / 背压 → 429**。调用方据此决定等多久、要不要降级，比笼统的 502 有用。
- **熔断指标用两个单调计数器**（跳闸 / 恢复）而不是 gauge。状态是按实例的、指标按插件，
  用一个 gauge 表达会让「instA 还在熔断、instB 已恢复」显示成 0（一切正常）。改标签为
  `instance_id` 又会带来基数只增不减的时间线，两个计数器之差才是正确的表达。

**这一层经过一轮独立的对抗性验证**（并发、取消、边界、竞态四个角度，共 16 个用例），
发现的 5 个真缺陷已全部修复，用例保留为回归防守：`hub-engine/tests/govern_adversarial.rs`、
`hub-api/tests/govern_deadline.rs`、`hub-api/tests/govern_metrics.rs`。

**验收**：3 节点同步 flow 端到端跑通，中间节点失败可定位到节点/耗时/错误；契约不兼容的
两个插件无法连成 flow；编排开销 P99 达标 ✅（见下）。

#### 编排开销基线（`crates/hub-engine/tests/orchestration_overhead.rs`）

**测的是哪一段**：`FlowRun::elapsed_ms` 减去各节点实际调用耗时之和——也就是中台自己花在
解析计划、装配信封、调度、记账上的时间。与插件耗时分开是刻意的：插件慢是插件的问题，
这条基线守的是「中台不该成为瓶颈」。所以压测用的插件是故意做到极快的（同机 gRPC、无
业务逻辑）。

| 样本 | 并发 | P50 | P95 | P99 | MAX | 吞吐 |
|---|---|---|---|---|---|---|
| 320 条 3 节点链 | 32 | 2ms | 2ms | **3ms** | 3ms | 433 条链/秒（约 1300 次节点调用/秒） |

预算 20ms（M4 的容量目标），实测 P99 3ms，**达标且余量很大**。

两点如实说明：

- 计时精度是毫秒，所以 2ms/3ms 应当读成「不超过这个数」。要看微秒级的分布得换计时器，
  而这条线守的是数量级，不值得为它把测量搞复杂。
- **这不是 M4 的容量压测**。M4 的目标是 1000–2000 TPS、500 并发 flow、5000 msg/s，
  那要在真实部署形态下压（跨机插件、真 PG、真 Redis）。这里给的是**编排层自身的基线**：
  它足够低，说明瓶颈不会先出在中台这一段上。

用例同时也是回归防守：P99 超预算就让构建失败。

### M3 异步与总线 ✅ 已完成

**`hub-bus`**：Redis Stream + 消费组 + ACK + 重投 + 死信 + 幂等。异步 flow 执行（节点独立
消费与投递，防环）、MQ/cron 触发器、死信查看与重放、背压与堆积降级、分层留存清理、
载荷四类边界落地（引用传递存储通道 / server-streaming / 长任务异步化）。

M3 落地时明确下来的几条决定：

- **底座是 Redis Stream 而不是 RocketMQ**。中台的队列是「中短途」：消息活不过几分钟，
  真正的持久化在 PG（run/run_node/span 都落库）。Stream 的消费组 + `XAUTOCLAIM` 恰好
  覆盖 ACK、重投与「消费者挂掉后消息被接管」这三件必需的事，不用为了它单独运维一套 MQ。
  **代价是 Redis 内存**：超过 `STREAM_RETENTION_HOURS` 的消息进死信而不是静默丢弃，
  死信本身也需要清理策略。
- **异步 flow 的消息粒度是「节点」而不是「整条链」**。一条消息 = 一次节点执行 + 投递
  下游。好处是：单条消息寿命短（重投便宜）、链路越长并行度越高、节点级重试天然成立。
  代价是需要收敛判断（哪些节点都完成了），用 `runs` 行上的一把锁串行化完成，不靠内存
  计数——中台可以多实例，内存里数不清。
- **「不丢」靠两道防线**：幂等键 `(run_id, node_id)` 挡住重投时的重复执行；而**完成
  记录**（`run_nodes` 里有没有这个节点的行）区分「做完过」与「上一个持有者中途死了」。
  只看键在不在，一次进程被杀就会让那个节点永远没人跑——那是静默丢数据，比重复执行
  糟糕得多。
- **节点执行失败不是基础设施故障**：它是一条业务结果，记录下来、收敛 run、正常 ACK。
  只有发不出去、库连不上这类问题才让消息不 ACK，交给重投。
- **重投的出口是次数，不是时间**。失败分支里判 `attempts >= BUS_MAX_DELIVERY`，
  到了就写死信并 ACK。
  这条判定一度不存在：`run_once` 的注释写着「重投次数用完时由留存巡检送进死信」，
  而死信表那边的注释也预期「巡检和消费循环同时判定它已耗尽」——但留存巡检只按**时间**
  判定（默认 24 小时），根本读不到次数。结果 `BUS_MAX_DELIVERY` 是个装饰性的配置项：
  一条永远失败的消息（比如引用了一条排不出计划的编排）会在总线上被反复接管、反复失败，
  每一轮都要完整重跑一遍注定失败的逻辑，直到躺满 24 小时。
  **判定必须放在失败之后**：`attempts` 是「这是第几次投递」，处理之前判
  `>= max_delivery` 会让 `max_delivery = 1` 的消息一次都没跑就被判死。
  写库与 ACK 的先后也是刻意的——先写库再 ACK，否则写库失败时这条消息两头都没了。
  回归测试 `crates/hub-engine/tests/async_dead_letter.rs` 用一条有环的编排钉住它。
- **超期消息里，已经定终态的 run 直接丢**，不进死信——那次执行早就跑完了，它的迟到
  消息没有可补救的东西。而停在 `queued` / `running` 的执行记录**不按时间清**：那些
  状态是「有东西卡住了」的信号。
- **留存清理与治理表清理走同一个巡检任务**：M2 已经在做实例级治理表的清理，留存清理
  挂在那里最自然——都是「按时间淘汰掉不再需要的东西」。
- **触发器是「让已发布的编排多一个被触发的入口」，所以它进 MCP 而发布不进**。
  登记一条 cron / mq 触发器不改变编排本身、也不改变生产流量走向，agent 可以做；
  发布则只能由人来做。边界划在接口层面，比靠约定可靠。
- **触发器的必填项在写入时就拦**（cron 要 `config.expr`、mq 要 `config.stream`）。
  表达式本身能不能解析留给调度器判断——cron 解析器在它那儿，装配失败会记在这条触发器
  的 `last_error` 上、控制台看得见。但「字段压根没给」不该等到那一刻：那种触发器从
  存下来第一秒就注定不工作，而它在界面上看起来是配好了的。

**验收**：插件重启/网络抖动/消费失败重投下不丢消息 ✅；死信可查看、可重放并追踪到新 run ✅
（重放要求人填回载荷——死信里只有摘要，做成「一键重发」会是个发出空载荷的假按钮）；
堆积到阈值按配置降级 ✅。

### M4 治理与可视化编排器 —— 进行中

**已完成**：

- **Vue Flow 编排编辑器**：从插件面板加节点、拖动连线、节点属性（插件 / 版本约束 /
  超时 / 重试）、连线时的本地即时校验（自环、重复边、成环，以及契约接不上时的提示）、
  保存草稿、发布。**验收路径已跑通**：在控制台里不改一行 JSON 完成 flow 创建 →
  加节点 → 连线 → 保存 → 发布，中台侧落库并被真实调用。
  契约兼容不做硬拦截——那条规则的真相在中台（字段级兼容检查），前端复刻一份迟早会
  漂移；本地只拦「一定不成立」的，权威判定留给保存。
- **版本灰度操作流**：控制台的「版本升级」页扫全部 flow 的草稿与已发布修订，列出锁在
  旧版本上的节点（落后几个版本、最新是哪个），按「已发布优先、落后越多越靠前」排序。
  同时把「跟随最新版本」的节点也列出来：它们升级时不必动，代价是缺一个稳定的回退点
  ——这件事该被看见而不是藏在数据里。
- **治理面板**：新增 `GET /admin/governance` 暴露实例级的并发占用、熔断阶段（正常 /
  跳闸 / 半开）、连续失败次数、累计放行数；控制台 5 秒自动刷新。
  它的口径与 `/admin/instances` **不同**：治理表按「被调用过」建项，刚注册还没被调用过的
  实例不在里面，而已下线但还留着失败计数的实例会在（那正是要保留它的原因）。
  这个区别写在接口的文档注释里，也写在控制台页面上——两个口径看起来像同一件事。
- **编排的版本 diff 与回滚**：拓扑页对比任意两个修订的 definition，以及「把这一版存为
  草稿」。回滚只做到「存成草稿」为止，不顺手发布——发布改变生产流量走向，把两步合成
  一步会让「回滚」变成一个点下去就生效的按钮，而它改的是线上在跑的编排。
- **鉴权插件正式化**：中台侧加了权限位与鉴权中间件，`examples/auth-plugin` 返回
  这个凭证持有的权限位。**分工是刻意的：中台定义权限位，插件回答「这个凭证有哪几个」**
  ——中台不知道谁是管理员，那是 anc 平台权限体系的事。把「谁能做什么」放进中台，
  等于把平台的权限体系在这里抄了一份，两边迟早不一致。
  详见下面「管理面鉴权落地时明确下来的语义」。

**待完成**：

- **告警接入 —— 暂缓**（2026-09-17 决定先不做，不是遗漏）。
  调研结论：anc 侧没有 Prometheus/Grafana/Alertmanager 的任何配置，中台也只暴露
  `/metrics` 原始指标。要做的话得先定**判断逻辑放哪一侧**——中台自检 + anc 展示、
  anc 拉指标自己判断、或接 UAT 上可能已有的监控——三条路要动的东西差别很大
  （第一条动中台加一张表与一个巡检，第二条要动 anc-backend 这个大项目，
  第三条只是写抓取与告警规则）。**在没定之前先写代码是在赌**。
- **容量压测调优至目标**：**已在 UAT 真实部署形态下压过一轮**（2026-09-18）：
  20 并发 × 50 次校验被拒的请求（不触发熔断）得 **2423 req/s，p50 6.4ms / p95 14.1ms /
  p99 38.3ms**，治理计数精确（`admitted=1006`、`in_flight=0`、`available` 满额归还）。
  每一次都是完整往返：中台 ingress → 跨网络 gRPC 到插件 → 校验器执行 → 返回。

  > 别把它与 M2 那条基线（P99 3ms、预算 20ms）直接比：后者量的是**编排层自身**
  > （进程内），这里多了一整跳真实网络与一个独立进程，不是同一件事。
  > 这一轮只证明了当下的余量，**没有找饱和点**，也没做长时间稳定性——调优到具体
  > 目标（1000–2000 TPS）仍待做。

  顺带在同一轮里验证了熔断器在真实部署上的行为：阈值 5 精确跳闸、快失败返回 503
  （8ms → 1ms）、冷却后半开探测成功自动恢复 closed；杀掉插件容器后陈旧实例
  在心跳超时内被自动摘除。

**验收**：在控制台不改一行 JSON 完成 flow 创建→校验→发布→灰度升级 ✅；
UAT 上已做过一轮容量观测（2423 req/s，p99 38.3ms ✅），但**尚未压到饱和点**、
也未调优至 1000–2000 TPS 的目标值。

#### 管理面鉴权落地时明确下来的语义

- **默认关闭**：没配 `HUB_AUTH_PLUGIN` 时整层不生效，管理面维持原样。
  给开关而不是直接启用，是因为 auth 插件在 UAT 上还没部署——直接启用会让管理面
  在插件就位之前谁也进不去。这条有个直接的后果，所以专门写了用例：
  **「启用鉴权」必须是一个显式的动作**。
- **五个权限位**：`read` / `invoke` / `edit` / `publish` / `admin`。
  **发布与改草稿必须是两位**——并成一位等于让所有编辑者都能发布，
  而发布改变的是线上在跑的那一份。
- **插件不可达时返回 503，绝不降级成匿名放行**。降级意味着一次 auth 插件抖动
  会让整个管理面变成无守卫，那正好是这一层想避免的事。这一条在端到端验证里
  专门走过：把插件停掉，管理面回 503 而不是 200。
- **认不出来的路径放行**、交给 axum 的 404。在这里凭空发明一个权限位，会让
  「路径写错了」表现为 403，把真正的问题藏起来。漏配的风险由单元测试兜住。
- **中间件挂在 body limit 外层**：未认证的请求不该被读一遍 body。
- **`scopes` 里的数组要转成 `[]any` 才有得回去**：`hubkit.WithPayloadJSON` 内部走
  `structpb.NewStruct`，它只认 `[]any`——直接给 `[]string` 会报
  `proto: invalid type: []string`，而那是个运行时才炸、报错指不到代码行的问题。
  这个是既有测试抓住的。
- **控制台把三种失败分开处理**：401 跳登录、403 直接展示中台给的那句话
  （它带着「缺哪个位、当前有哪些」）、503(auth_unavailable) 提示稍后重试。
  最后这条**不是登录问题**——中台刻意没降级，重新登录也解决不了它。

**端到端验证过的五个场景**（真插件 + 真中台）：无凭证 401、管理员 200、
普通用户 403 且消息点出缺哪个位、探活不要凭证、`/ingress` 不受鉴权影响；
外加插件停掉后 503。

#### 控制台（在 `anc-frontend` 仓库）

十一个页面（十个菜单项 + 从列表点进去的流程拓扑）：插件目录、实例健康、
编排流程与拓扑、编排编辑器、执行记录与详情、调用链与瀑布图、死信、触发器、
治理、版本升级。

两处与中台契约相关的取舍：

- **控制台用独立的 axios 实例，不复用平台既有的 `request` 封装**。中台返回裸 JSON +
  标准 HTTP 状态码，而那个封装假定 `{code: 200, data, message}` 的包封——套上去会把
  **每个成功响应都判成失败**，也会把 409 / 422 / 429 / 503 的语义压平成一个字符串，
  而治理面板与编排页正要靠这些区分决定怎么展示。
- **`GET /hub-api/` 的前缀剥离要在两处保持一致**：nginx 的
  `location /hub-api/ { proxy_pass http://127.0.0.1:8095/; }` 与本地 vite 的代理。
  两边行为不一致的话，本地跑通的路径上生产就 404 了。

**前端经过一轮独立的对抗性验证**（另起 agent，覆盖功能正确性、边界异常、视觉与交互），
共 7 个缺陷（2 严重），已全部修复并逐条验证。两个严重的值得记下来：

- **侧栏卡片被 flex 压扁**：`.side` 是 flex column，子项默认 `flex-shrink: 1` 被压扁，
  而 Element Plus 的 `.el-card` 带 `overflow: hidden`，于是超出卡片高度的内容被静静吃掉
  ——「有阻断性问题，发布会被拒」这句提示整个消失。更坏的是**压扁之后容器不再溢出**，
  `.side` 上那条 `overflow-y: auto` 永远不触发，被裁的内容连滚都滚不到。
- **长节点 id 让节点重叠**：节点 id 没有长度限制（中台只约束 flow 名），一个 50 字符的
  id 把节点撑到 550px，而布局按写死的 190px 排坐标，于是后一个节点整个落进前一个的
  矩形里、连线也跟着指错方向。
  修法不只是给节点加宽度上限——**布局用的尺寸必须与节点的实际约束一致**，
  所以尺寸估算与 dagre 参数被抽到 `views/hub/shared/graph.js`，两个画布共用一份。

### M5 插件脚手架下发 —— 已实现（五门）

让「写一个插件」有个起点：控制台新增「插件开发接入」页（`/hub/onboarding`），
填一个插件名就能下载到一份可运行的工程——不必先检出本仓库。

新增两个 crate：

| crate | 作用 |
|---|---|
| `hub-templates` | 模板的注册、渲染与打包。模板由 `build.rs` 在**编译期**嵌进二进制——页面显示的版本与下载到的内容因此永远同源 |
| `hub-mock` | 本地 mock 中台，给开发者的 L3 替身。判据**调 hub-registry 里真中台也在调的同一批函数**，不照抄（本仓库已吃过一次手抄副本漂移的亏） |

**四条值得记的取舍**：

1. **随包下发 SDK 源码**（~550KB）。不给的话生成的工程 `replace` 指向一个不存在的
   绝对路径——包能下、能解压，**就是编不过**，而「不必先检出中台仓库」正是这页的
   全部理由。排除 `vendor/`（15MB，且新工程用别人的 vendor 会被 Go 直接拒绝）
   与 `cmd/hub-plugin/`（它 `//go:embed all:templates`，而 templates 不随源码发出，
   带进去编译直接失败）。
2. **`@@key@@` 占位符**而不是 Go 的 `{{.Key}}`：中台（Rust）与 CLI（Go）渲染同一批
   模板，而 `{{`/`}}` 在 Node 模板与 C# 插值字符串里会撞。**未定义的占位符报错而不留空**
   ——留空会生成一个要等到注册时才炸的工程。
3. **接入指南分两层**：共通层（`_shared/onboarding.md`，由中台侧统一维护、内联进
   各语言的 `AGENTS.md`）与语言层。同一条「接入四道关」不该在六份模板里各抄一遍。
4. **下载接口不鉴权**：它是「怎么开发一个插件」的开发资料，不是运行中的数据。
   要收紧时改 `authz` 里那一行即可。

**验收状态**：

- **五门**（Go / Python / Node / Rust / C#）的模板都能渲染出可编译、可注册的工程。
  `scripts/smoke-template.sh` 对每门走一遍「渲染 → 产物文件集 → 无残留占位符 /
  HTML 注释 → AGENTS.md 含四道关 → 随包 SDK 在位且不含构建产物 → 构建 → L1 →
  L3（真注册进 mock 中台）」。本地带 L3 全跑过。
- 经 HTTP 下载的 zip 与本地渲染的工程**逐字节一致**——C# 那份比对过，唯一差异是
  `@@hub_addr@@`：本地渲染填本地 mock 地址，中台填它自己的地址，设计如此。
- 前端走完整流程（点「开发插件」→ 填插件名 → 下载 → 落盘 → 解压结构正确），并经
  视觉工具确认渲染无错乱、无未渲染占位符。
- ⚠️ **CI 里只跑得了「渲染 + 产物断言」那一半**：这个 runner 没有外网，而生成工程要
  解析依赖；镜像里也只有 Rust / Go / Python 三套工具链，没有 node 与 dotnet。
  所以构建 / L1 / L3 在 CI 上是**跳过并把原因逐门列出来**的，不会给出一份看着全绿、
  实则没验的报告。要补齐得先给 CI 备好各语言的依赖源，并把工具链装进构建镜像。

### M6 插件间发现与调用 —— 已实现

插件之间要有内部机制互相发现、互相调用。此前插件间唯一的通道是 `HubState.Publish`
（异步触发 flow），没有同步互调；发现能力只在 MCP/HTTP 管理面，proto 里没有
插件→hub 的 discovery RPC。M6 补上同步的半边：**hub 代调的 gRPC（A→hub→B）**，
下游复用与 flow 执行同一条 `Invoker`/`Governor` 链路；发现三件套只是把 hub-store
已有的注册表查询翻成插件够得着的 RPC，不另立事实源。

| 新增 / 改动 | 内容 |
|---|---|
| `gateway.proto`（新） | `PluginGateway` 四个 RPC：`ListPlugins`（在线清单）/ `DescribeMessage`（消息→生产者消费者）/ `GetContract`（契约+字段级 schema+`invokes`）/ `Invoke`（同步互调）。`PluginManifest` 增 `invokes = 9`——manifest 以字节存库，纯追加字段，无迁移，旧 hub / 旧插件双向兼容 |
| `hub-grpc/src/gateway.rs`（新） | `GatewayService`。鉴权与 StateService 共用 `authenticate_plugin`（`x-hub-state-token` 每次直查 PG）；Invoke 依次过：配额（Redis 固定窗口，每插件每分钟 60）→ 策略校验（`declared` 时对 caller manifest 的 `invokes`）→ 防环/链深（`hub.call_chain`，上限 8）→ 信封整备（subject 无条件覆盖为 caller、deadline 夹紧到 `min(传入, now+timeout)`）→ 复用 `Invoker` 全链路 → 审计 span 落库 |
| `hub-core` / `hub-store` | `HUB_PLUGIN_CALL_POLICY`（`allow`｜`declared`，默认 `allow`——存量插件零改动）；新查询 `caller_of_state_token` / `manifest_of_version` 供策略解析 caller 的授权名单 |
| 五门 SDK | `GatewayClient`（发现三件套 + `invoke_plugin` 便捷互调：造信封、复制 trace 与链、把业务结果翻成类型化异常）+ 既有的 `StateClient.publish()` 补齐封装。互调的治理（链、配额、防环）全在中台，插件侧只复制不计算 |
| 跨语言事实源 | `sdk/go/hubkit/testdata/hub-rules.json` 增 `gateway` 小节（链键名 / 深度 8 / 配额 60），`state_rules.rs` 与各语言 SDK 的常量都对它断言 |
| mock 与示例 | `hub-mock` **不装配**网关（保持替身语义，mock 上网关 RPC 是 UNIMPLEMENTED）；可运行示例 `examples/ping-chain`（Python 双插件：caller 声明 `invokes`，先 `ListPlugins` 发现再互调，演示 trace 贯通） |

**五条值得记的取舍**：

1. **结果语义分两层**。被策略、配额、防环拦下，以及下游拒绝/出错，都是**业务结果**
   ——走响应字段（`outcome` + `reason`/`issues`），SDK 翻成类型化异常；只有基础设施
   故障（未鉴权、本实例没装配调用能力）才走 gRPC Status。混成一种的话，调用方分不清
   「该改逻辑」还是「该重试」。
2. **链的语义是「已处理过该消息的插件序列」，追加 caller 是中台的职责**。SDK 只把
   收到的链**原样**复制——客户端自己追加会写错链，制造「本地放行、中台拒绝」的裂缝
   （Python 侧测试故意把调用方自己的名字放进链里钉住这一点）。
3. **治理参数用常量不给旋钮**（深度 8、每插件每分钟 60 次）：与 Publish 的配额同一
   哲学——它需要按真实流量校准，没有证据之前，配错的旋钮等于没限额。数值与键名钉在
   `hub-rules.json`，防跨语言漂移。
4. **互调的身份与 Publish 同强度**：每次直查 PG 反查凭证（不缓存），信封 subject
   服务端无条件覆盖为反查结果——请求自报的身份无效。这决定了 M6 没有**扩大**插件面
   的冒名敞口；全面鉴权仍留二期（风险 13）。
5. **审计 span 旁路写**：每个出口调用落一条 `gateway.invoke.{target}`（caller /
   target / outcome / 调用链），写失败只 `warn!` 不挡调用——审计是治理的眼睛，
   不该成为链路上的单点。

**验收状态**：

- `cargo test -p hub-grpc --test gateway`：9 条集成测试对着真 PG/Redis + testkit
  双插件跑——发现三件套（含 `schema_json` 与 `invokes`）、互调身份/载荷/链、
  `declared` 策略的拒绝与放行、成环拒绝、链深边界、配额打满、deadline 夹紧、
  审计 span 落库、未装配 Invoker 回 `unavailable`。随主 workspace 的 `cargo test`
  进 CI 门禁。
- Go / Python 门在 CI 各自的 job 里跑（`build-go`；`run-py-tests.sh` 在容器里跑
  `sdk/python/tests`，含网关往返与异常映射的用例）。**Node / C# 的网关测试不在
  CI**——构建镜像没有 node 与 dotnet（与 M5 同一限制），本地 best-effort 跑过。
- ⚠️ **互调经 nginx TLS 插件面（8094）的链路未跨机验证**：网关 RPC 与注册/状态共用
  同一条连接、同一张凭证，理论上没有新增链路，但没验过就是没验过，UAT 部署后补。
- 本地起真中台 + `examples/ping-chain` 双插件的完整走查步骤见该示例的 README
  （「从 MCP 面调 `chain_ping`」到观察 trace 贯通与调用链）。

## 八、测试与验收（四层）


| 层 | 内容 | 工具 |
|---|---|---|
| 契约层 | `buf lint` / `buf breaking` CI 门禁；SDK `conformance` 套件（schema 匹配、超时、错误码、幂等），插件接入前必须跑通 | buf + Go 测试 |
| 单元/集成 | 执行引擎各条失败路径、注册/摘除、兼容检查、总线重投、治理的并发与竞态；用官方测试插件可控注入延迟/失败/错 schema | `cargo test` + testcontainers |
| E2E | 全栈 compose 跑注册→编排→同步/异步执行→失败→死信→重放；MCP 工具全量调用；控制台关键路径 | 脚本化场景 + Playwright |
| UAT 冒烟 | 部署后**跨机**远程插件注册（经 nginx:8094）→ 真实业务插件跑通 → 压测报告 | 人工 + 压测脚本 |

**跨机验证是 M1 的硬性验收项**——同机验证不能证明 nginx gRPC 代理、可达性探测与自报地址三条链路正确。
**这四条已在 UAT 验证**（2026-09-18，插件跑在独立 docker 网络 / 独立网络命名空间，
经 `https://hub.example.com:8094` 注册，自报 `http://172.30.99.10:9000`）：
代理生效、探测拨通非回环地址、自报地址被采纳、`source_ip` 是真实来源而非 127.0.0.1。
**跨物理机也复验过**（插件跑在开发机、经反向隧道接入，中台记录 `source_ip=203.0.113.20`），
但那条有折扣：UAT 拨不到开发机，自报的是中继端口而非插件主机的地址。
真正的跨网段直拨仍缺一台与 UAT 双向可达的机器。

### 实际做到的与没做到的

**集成测试用真实依赖，不用打桩**：真实 PostgreSQL 与真实 Redis（`db /9`，与应用用的
`/2` 分开）。注册与摘除、总线重投、治理的并发竞态这些性质只能对着真东西验证——
打桩会把被测的那一层换成桩的行为，验的就不再是它了。没有这些依赖时相关用例会
**失败而不是跳过**，这是刻意的。

**回归测试要证明自己有效**。这次修的两个缺陷都做了这一步：把修复临时改回旧写法，
确认测试真的失败（run 起止时间那条抓到 28.9ms 的差值；重投耗尽那条表现为
「消息转 20 轮仍不进死信」）。一条不会失败的回归测试是负资产——它给人已经守住了的错觉。

**控制台经过一轮独立的对抗性验证**：另起 agent，按功能正确性 / 边界异常 / 视觉与交互
三个角度验，产出的 7 个缺陷全部修正并逐条复验，另有 13 项怀疑经查证后不成立
（同样记下来了，免得后来的人重复怀疑）。

**没做到的**：E2E 那一层还没有脚本化的全栈 compose 场景，控制台的验证目前靠
playwright 脚本临时驱动而不是纳入 CI。（UAT 的跨网络注册验收与容量观测已于
2026-09-18 做过一轮，见 M1/M4 两节；但都还没脚本化进 CI，仍是人工驱动。）

**MCP 工具聚合已经落地**——这里此前记的是「尚未落地」，自 2026-09-18 起不再成立。
manifest 里声明的工具以 `插件名__工具名` 出现在 `tools/list` 里，中台不用重启。
实测（本地中台 + 一个插件在线）19 个工具，插件全部离线时 18 个：列表**按「有没有
在线实例」现查**，所以插件不在线时它的工具也不在，不会留下一个调得到却永远调不通的
死工具。

仍然没做到的是**工具面变化的通知**：全仓没有 `notifications/tools/list_changed`，
所以已连接的 agent 必须**自己重新拉一次** `tools/list`，才看得见新插件、或发现某个
插件已经下线。这条留在这里，别当成不存在。

**HubState 这一版另有两处没有任何门禁覆盖，如实记下来**（不是「以后再说」，是此刻的事实）：

- **Go 侧测试在 CI 里根本不跑**。`.gitlab-ci.yml` 只有 `cargo test` / `cargo clippy` /
  `docker build`，而本特性新增的 700+ 行 Go——包含 SDK 那条收到 `UNAUTHENTICATED` 就重注册
  的路径与它的冷却窗口——一条门禁都不过。
  > **已解决**：现在有了 `build-go` job（测试 + 按需构建插件镜像）与 `deploy-auth-plugin`
  > （独立 stage，按需部署）。依赖不再需要额外的 module 快照——Go 的 vendor 提交进了仓库，
  > 容器里 `GOPROXY=off` 也编得过。工具链则由**自制的构建镜像**提供：服务器上那个
  > `anc-builder` 走的是 rustup 的 minimal profile，**没有 Go**。故 `deploy/builder.Dockerfile`
  > 自制了一个 Rust / Go / Python 齐备的 `anc-hub-builder:1.0`，中台与插件共用。
  > 它顺带也带了 clippy 与 rustfmt，但 **CI 不跑**——门禁只有 `cargo test`，与
  > anc / anc-codebase 一致；那两个留给本地排查用。
- **`hub-server` 的装配没有自动化验证**。注册面、状态面（含它为 HubState 单开的 Redis
  连接）、后台巡检任务与优雅退出全写在 `main()`（`crates/hub-server/src/main.rs`）里。
  各 crate 的集成测试验的是各自的行为，Task 4 的端到端用例也是自己搭的一套装配——
  它们都**替代不了**「照 `main()` 那样把东西都接起来、真的能起、真的能跑」这一步。

## 九、风险与显式假设

| # | 风险 / 假设 | 应对 |
|---|---|---|
| 1 | **管理面全插件化 + 无内置守卫**：正常 HTTP 面的管理面完全没有中台内置鉴权 | 已记录为设计意图；主机面运维通道是唯一逃生口；过渡期用 nginx 网段限制兜底 |
| 2 | **插件面不鉴权**：任何能连上 8094 的进程都能注册插件并被编排调用 | 已记录为设计意图；注册时做可达性探测过滤无效注册；审计记录注册来源 IP |
| 3 | gRPC 经 nginx 的超时与保活坑 | 心跳用定时 unary（非长 stream）；`grpc_read_timeout` 显式调大；M1 跨机验收专门覆盖 |
| 4 | **PG / Redis 与中台同机**：单机故障域扩大，且备份成了己方责任（原设计假设的是别人托管的实例） | 数据绑在 `/apps` 大卷上（避开 devicemapper 的 10G 切片）；`deploy/backup-pg.sh` 走 `pg_dump` + 保留策略，**恢复路径已演练**；两者都只绑回环，不对网段暴露 |
| 5 | 插件强制无状态 + 外置状态 API，插件作者可能不习惯 | SDK 提供 kv/cache 易用封装；文档给正反例 |
| 6 | 前端分属两个仓库，API 契约易漂移 | **原计划用 OpenAPI 导出 + 类型生成器，实际没做**——控制台是一份手写的 `api/hub.js`。当前的兜底是：中台的错误体是结构化的（`{error, message, issues, plugin}` 四个字段有机器可读的语义），控制台按 `error` 而不是按文案分支；接口一改，那些分支会立刻失效而不是静默走偏。真正的生成器仍待做 |
| 7 | Rust 首次离线编译 5–15 分钟拖慢 CI | `target/` 与 `.sqlx/` 缓存 |
| 8 | 容量目标为估算值，未经真实压测 | M2 先压编排层基线（P99 3ms ✅）；UAT 部署后压了一轮真实形态（2423 req/s、p99 38.3ms ✅），**饱和点仍未探明**；不达标则按预案走向多实例 |
| 9 | 单人 + AI 推进，四期总周期较长 | 每期都是可独立验收的交付物；M1 结束即可让真实插件团队介入 |
| 10 | `HubState` **只做插件级隔离**，state.proto 原承诺的租户隔离未实现 | 协议推不出当前租户（请求只有 KvKey，无 Envelope 上下文）。已同步订正 proto 注释，不留做不到的承诺 |
| 11 | **HubState 的隔离强度 = 「能过注册探测 + 写出兼容 descriptor」** | 插件面按设计不鉴权（见第 2 条），任何能连 8094 的进程注册一个与目标插件契约兼容的新版本，即可拿到反查为该插件的凭证，读写其状态空间；又因键前缀不含版本号，新旧版本共享同一状态空间。本期交付的是**「身份不可自报」**，不是插件间租户级隔离 |
| 12 | **跨机制环不严防**：`hub.call_chain`（网关互调）与 `hub.publish_chain`（Publish/flow 触发链）是两套互不知晓的链——「互调里发 Publish、Publish 的 flow 里再互调」这种跨机制环没有统一检测 | 双向各 8 层深度上限 + 各自的每分钟配额先兜住；出现真实案例之前不为它合并两套链的过检 |
| 13 | **插件面冒名敞口不扩大**：互调把「点名调用任意在线插件」的能力开在了无鉴权的插件面上（见第 2 条） | 与 Publish 同强度：`x-hub-state-token` 每次直查 PG 反查，信封 subject 服务端无条件覆盖为反查结果——调用方自报的身份无效，冒名者至多以自己注册的实例身份发起调用；`HUB_PLUGIN_CALL_POLICY=declared` 可整体收紧为白名单。全面鉴权（mTLS/凭证下发）留二期 |
