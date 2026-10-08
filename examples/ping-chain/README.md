# ping-chain —— 插件间发现与互调（PluginGateway）示例

两个 Python 插件串成一条「调用链」，演示插件之间**经中台**互相发现、互相调用：

```
agent / curl
     │
     ▼
ping-caller（chain_ping）
     │  ① ListPlugins   ── 查在线插件清单，确认 ping-callee 在线
     │  ② Invoke        ── 同步调用 ping-callee 的 echo（A→hub→B，中台代调）
     ▼
中台（hub-server）  …… 鉴权 / 防环 / 限额 / 审计全在这一跳
     │
     ▼
ping-callee（echo）── 把 text 回显，带回 trace_id
```

要点：

- **插件之间不直连**。caller 调 callee 走的是中台的 `PluginGateway.Invoke`；
  直连会绕过中台的治理、审计、熔断与身份体系。
- **caller 的 manifest 用代码构造**，并声明 `invokes: ["ping-callee"]`——插件互调
  的授权名单。中台 `HUB_PLUGIN_CALL_POLICY=declared` 时按这份名单放行（缺省
  `allow` 下不声明也能调，声明是为了让互调关系可审计）。
- **互调带上当前信封**（`current_envelope=env`）：trace 贯通到下游、调用链
  （`hub.call_chain`）原样传递，中台据此防 A→B→A 成环。
- 两个插件都用 [`sdk/python/hubkit`](../../sdk/python/)，仅约百行业务代码；
  注册、心跳、自愈、优雅退出由骨架处理。

## 前提

- Rust stable（见仓库根 `rust-toolchain.toml`）——跑真中台用
- Docker —— 起 PostgreSQL 与 Redis
- Python ≥ 3.9 与 `pip`；SDK 依赖 `grpcio>=1.84.0`、`protobuf>=7.35.1`

> **必须用真中台（`hub-server`），不能用 `hub-mock`**：mock 替身没有装配
> `PluginGateway` 服务（它只挂注册面与状态面），发现与互调的 RPC 在 mock 上
> 是 UNIMPLEMENTED。

## 1. 起真中台

先起本地 PostgreSQL 与 Redis（任选方式，例如）：

```bash
docker run -d --name hub-pg  -e POSTGRES_USER=u -e POSTGRES_PASSWORD=p \
  -e POSTGRES_DB=plugin_hub -p 5432:5432 postgres:16
docker run -d --name hub-redis -p 6379:6379 redis:7
```

再起中台（缺 `DATABASE_URL` / `REDIS_URL` 会拒绝启动；启动时自动跑数据库迁移）：

```bash
# 仓库根目录
DATABASE_URL=postgresql://u:p@127.0.0.1:5432/plugin_hub \
REDIS_URL=redis://127.0.0.1:6379/2 \
HUB_HTTP_PORT=8092 \
cargo run -p hub-server
```

起来之后：HTTP 面与 MCP 在 `127.0.0.1:8092`，插件面 gRPC 在 `127.0.0.1:8093`
（默认端口）。

## 2. 安装 hubkit SDK

```bash
# 仓库根目录；依赖走公共镜像时加 -i https://pypi.tuna.tsinghua.edu.cn/simple
pip install -e sdk/python
```

## 3. 起 ping-callee（下游）

```bash
# examples/ping-chain 目录
HUB_ADDR=http://127.0.0.1:8093 \
HUB_ADVERTISE_ADDR=http://127.0.0.1:9101 \
HUB_LISTEN_ADDR=:9101 \
HUB_INSTANCE_ID=ping-callee-demo \
python3 callee/main.py
```

看到日志里 `已注册到中台 plugin=ping-callee` 即成功。

## 4. 起 ping-caller（上游）

另开一个终端：

```bash
HUB_ADDR=http://127.0.0.1:8093 \
HUB_ADVERTISE_ADDR=http://127.0.0.1:9102 \
HUB_LISTEN_ADDR=:9102 \
HUB_INSTANCE_ID=ping-caller-demo \
python3 caller/main.py
```

> `HUB_ADVERTISE_ADDR` 是**中台回拨你**的地址，必须从中台的视角拨得通。
> 中台与插件同机时 `127.0.0.1` 可以；跨机时写中台能路由到的内网 IP。
> 两个插件各用一对端口，别共用——监听端口冲突会直接起不来。

## 5. 从 MCP 面调 `chain_ping`

caller 的工具注册后以 `ping-caller__chain_ping` 出现在 MCP 工具面（无需重启
中台；已连接的 agent 需重新拉一次 `tools/list` 才看得见）。

用任意 MCP 客户端连 `http://127.0.0.1:8092/mcp`，例如 Claude CLI：

```bash
claude mcp add --transport http --scope user ping-hub http://127.0.0.1:8092/mcp
```

然后在对话里让它调 `ping-caller__chain_ping`，参数 `{"text": "hello"}`。
也可以用 MCP Inspector（`npx @modelcontextprotocol/inspector`）手动 `tools/list`
→ `tools/call`。

**等价的确定性调用**——经 HTTP ingress 直接调 caller（与 MCP 走的是同一个
插件体，差别只在入口）：

```bash
curl -s -X POST http://127.0.0.1:8092/ingress/ping-caller \
  -H 'Content-Type: application/json' \
  -d '{"payload": {"text": "hello"}}' | python3 -m json.tool
```

预期响应（字段在 `payload` 里）：

```json
{
  "ok": true,
  "reply": {"echo": "hello", "trace_id": "01J……"},
  "trace_id": "01J……",
  "call_chain": "ping-caller"
}
```

## 观察这三件事

1. **trace 贯通**：`trace_id` 与 `reply.trace_id` 相同——caller 与 callee 处理的
   是同一条 trace。`GET /traces/{trace_id}` 能看到整条链上的 span。
2. **调用链**：`call_chain` 是 `ping-caller`——中台在代调时把 caller 追加进
   `hub.call_chain`（链的语义是「已处理过该消息的插件序列」；SDK 只原样复制、
   不自己追加）。若 callee 再去调 caller，中台会因 caller 已在链上而拒绝。
3. **审计**：中台为每次互调落一条 span（`name=gateway.invoke.ping-callee`），
   attributes 里带 caller / target / outcome / 调用链；`GET /traces` 按其
   `trace_id` 聚合可见。

## 再试一件事：把授权收回来

中台以 `HUB_PLUGIN_CALL_POLICY=declared` 重启，caller 的 manifest 里去掉
`invokes=[TARGET]` 那一行并把版本号升到 `0.1.1`（同版本号不可改契约），重启
caller 后再调一次：

```json
{"ok": false, "outcome": "ERROR", "reason": "未声明对 ping-callee 的调用授权"}
```

这是**业务结果**而不是调用失败：caller 把 `InvokeFailed` 的 `reason` 翻译进了
返回载荷。恢复声明即可放行。

## 常见问题

| 现象 | 多半是 |
|---|---|
| `chain_ping` 返回 `ok=false`，reason 说 `ping-callee 不在线` | callee 没起来或注册没成功——看 callee 终端的 JSON 日志 |
| reason 是 `未声明对 ping-callee 的调用授权` | 中台开了 `declared` 策略而 caller 没声明（见上节） |
| 插件日志反复刷 `注册未通过` / `UNREACHABLE` | `HUB_ADVERTISE_ADDR` 从中台视角拨不通（跨机写了 `127.0.0.1`） |
| 发现 / 互调报 `UNAUTHENTICATED` | 中台重启后凭证轮换——骨架会自动重注册换新凭证，稍等即可 |
| 互调 reason 是 `本分钟调用已达上限` | 触发了每插件每分钟 60 次的互调配额（固定值，无环境变量旋钮） |

互调的限额、防环与权限声明的完整说明见
[`docs/plugin-onboarding.md`](../../docs/plugin-onboarding.md) 的「插件互调与发现」。
