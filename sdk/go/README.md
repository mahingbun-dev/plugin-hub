# anc-hub 插件 SDK（Go）

用 Go 写 anc-hub 插件的工具箱。**六件套**：

| 件 | 位置 | 作用 |
|---|---|---|
| ① 服务端骨架 | `hubkit` | 实现两个方法就有个能跑的插件：gRPC 服务、自注册、心跳、被摘除后自愈、优雅退出全由它处理 |
| ② 契约定义 | `proto/hubv1` | 由 `../crates/hub-proto` 的 proto 生成并提交，**不需要装 protoc** |
| ③ 脚手架 | `cmd/hub-plugin` | `hub-plugin new <name>` 生成可运行的插件工程 |
| ④ mock 中台 | `mockhub` | 本地 mock，离线开发与自动化测试用 |
| ⑤ 一致性套件 | `conformance` | 插件接入前必须跑通的检查，跑的就是中台注册期的规则 |
| ⑥ 调试工具 | `cmd/hubprobe` | 直连插件调试（相当于我们这个契约的 grpcurl），外加一个 `e2e` 经中台打真实链路 |

## 快速开始

```bash
# 生成一个可运行的插件工程
go run ./cmd/hub-plugin new order-reader --dir /tmp/order-reader

cd /tmp/order-reader
go mod tidy     # 先把 go.mod 里的 replace 指向本 SDK 的实际位置
go test ./...   # 含契约一致性检查
```

生成的插件**零 proto 工具链依赖**：它用 `google.protobuf.Struct` 承载 JSON 载荷，
也就是「直接调用」那条路（agent 经 MCP、外部系统经 HTTP）。需要与其他插件在 flow 里
传递类型化消息时，再加自己的 `.proto`。

## 最小插件

```go
type Plugin struct{}

func (p *Plugin) Manifest() *hubv1.PluginManifest {
	return &hubv1.PluginManifest{
		Name:    "order-reader",
		Version: "1.0.0",
		Consumes: []*hubv1.MessageContract{{FqName: hubkit.StructFQName}},
	}
}

// Descriptor：只用 Struct 载荷的插件没有自己的 proto，返回空即可
func (p *Plugin) Descriptor() []byte { return hubkit.DescriptorOf() }

// Validate：中台一定先调它，通过才进 Handle
func (p *Plugin) Validate(_ context.Context, env *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	payload, ok := hubkit.PayloadJSON(env)
	if !ok {
		return hubkit.Invalid(hubkit.Issue("payload", "需要 JSON 对象载荷")), nil
	}
	if _, ok := payload["orderId"].(string); !ok {
		return hubkit.Invalid(hubkit.Issue("payload.orderId", "缺少必填字段 orderId")), nil
	}
	return hubkit.Valid(), nil
}

// Handle：插件体
func (p *Plugin) Handle(_ context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	payload, _ := hubkit.PayloadJSON(env)
	return hubkit.WithPayloadJSON(env, map[string]any{"orderId": payload["orderId"]})
}

func main() {
	if err := hubkit.Run(&Plugin{}, hubkit.ConfigFromEnv()); err != nil {
		log.Fatal(err)
	}
}
```

## ⚠️ 接入时最容易踩的坑

**`HUB_ADVERTISE_ADDR` 必须是中台能拨通的地址。**

中台在注册时会连你上报的地址做**可达性探测**，探不通直接拒绝注册。所以它不能写成
本机视角的 `localhost`：

- 中台与插件同机 → `http://127.0.0.1:9000` 可以
- 中台在别的机器 → 必须写本机的内网 IP 或域名，例如 `http://203.0.113.10:9000`

注册被拒时，中台会给结构化的拒绝原因，`hubkit` 会把它们**逐条**打进日志（JSON，一条原因一行）。
实测抓的一行长这样：

```json
{"time":"2026-09-18T10:41:58.549264+08:00","level":"ERROR","msg":"中台拒绝了注册","code":"BREAKING_CHANGE","message":"相对版本 1.0.0 存在破坏性契约变更","detail":"wms.v1.Order.sku（编号 1，类型 string）已被删除；基线版本里已有的字段不能删、改类型或改编号，只能新增——请把这些字段按原编号原类型改回再注册；若确实要重新设计契约，需由中台侧先清掉旧版本基线（hubctl remove-version）"}
{"time":"2026-09-18T10:41:58.549272+08:00","level":"ERROR","msg":"注册未通过，稍后重试","reasons":1,"retry_in":"5s"}
```

有几条原因就有几行 `中台拒绝了注册`，**最后一行是汇总**：`reasons` 是条数、`retry_in` 是下次
重试的间隔。`detail` 会直接写出该改什么——**别只看 `code`**，它只能定位到一类问题。

**某些网络路径上，Go 的 TLS 1.3 握手会被中间设备重置。**

如果 `HUB_ADDR` 是 `https://...`，而注册一直卡在这个错上：

```
authentication handshake failed: read tcp 203.0.113.20:54976->203.0.113.10:8094:
    read: connection reset by peer
```

先别急着怀疑中台或证书——**同一条路径上用 `curl` 或 `openssl s_client` 连同一个地址，
TLS 1.3 是通的**。这一点正是它难查的原因：手上所有的排查工具都告诉你「网络没问题」。

**怎么确认是它**：把 TLS 上限压到 1.2。如果立刻就通了，就是这个问题。

```bash
HUB_TLS_MAX_VERSION=1.2
```

**为什么会这样**：说不清是哪台设备、按什么规则拦的。逐项二分排除过两个常见猜测——
**不是报文大小**（Go 的 ClientHello 1490 字节、openssl 的 1544 字节，更大的那个反而通），
**也不是后量子密钥共享**（两边都发了 `X25519MLKEM768`，关掉它也照样被重置）。
能确定的只有两点：**路径相关**（同一个插件从中台那侧发起就是通的，服务端可以排除），
**且只挑 Go**（同路径 openssl 的 TLS 1.3 正常）。

代价是失去 1.3 更短的握手与更强的前向保密。它是**逃生口，不是默认配置**：确认服务端
没问题、且换了版本就能通之后再用。

**Go 在 macOS 上不读 `SSL_CERT_FILE`。**

中台证书若由内网 CA 签发（本项目的 UAT 就是），在 Linux 上挂载 CA 并设置
`SSL_CERT_FILE` 就能被信任；**macOS 上不行**——Go 走的是 Keychain，这个变量被忽略，
于是报出这样一条：

```
tls: failed to verify certificate: x509: "hub.example.com"
    certificate is not standards compliant
```

注意它点名的其实是**服务端叶子证书**，读起来像在说中台证书不合规，也很误导。

**在开发机上自测跨机接入时**，把插件交叉编译成 `linux/amd64`、跑在容器里，就绕开了：

```bash
GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o /tmp/plugin .
docker run --rm -v /tmp/plugin:/plugin:ro \
  -v /path/to/内网CA.crt:/ca/ca.crt:ro \
  -e SSL_CERT_FILE=/ca/ca.crt \
  -e HUB_ADDR=https://... -e HUB_ADVERTISE_ADDR=http://... \
  alpine:3.20 /plugin
```

生产环境是 Linux，不受这条影响。

## 插件模型的四条约定

1. **校验器先行**：中台一定先调 `Validate`，通过才调 `Handle`。校验不通过时插件体
   不会被调用，链路当场短路。
2. **强制无状态**：实例内存不保证跨调用保留（换来的是热切换零代价、可水平扩展、
   重放与测试都简单）。需要跨调用保留的东西走中台的状态接口。
3. **契约不可漂移**：同一版本号的 manifest 与 proto 不可变更，中台会拒绝「同号不同
   契约」的注册；改了请升版本号。中台还会做**字段级兼容检查**——删字段、改字段类型、
   改字段编号都会被拒；新增可选字段、新增枚举值放行；字段改名放行但会告警。
4. **预算来自信封**：`hubkit.Budget(env)` 给出剩余时间。紧张时应提前放弃，而不是把
   时间耗光后靠上层超时兜底——那样调用方连"为什么慢"都看不出来。

## 调试

`hubprobe` 有六个子命令，按「打谁」分成两组——**上线前两组都要过**：

```bash
# ① 直连插件（不需要中台）：验「插件自己做得对不对」
go run ./cmd/hubprobe health   http://127.0.0.1:9000
go run ./cmd/hubprobe describe http://127.0.0.1:9000
go run ./cmd/hubprobe validate http://127.0.0.1:9000 --payload '{"orderId":"SO-1"}'
go run ./cmd/hubprobe invoke   http://127.0.0.1:9000 --payload '{"orderId":"SO-1"}'
go run ./cmd/hubprobe conform  http://127.0.0.1:9000

# ② 打中台的 HTTP 面：验「注册进来之后，中台按登记的地址真的拨得到它」
go run ./cmd/hubprobe e2e http://127.0.0.1:8092 order-reader --payload '{"text":"你好"}'
```

- `invoke` 走的是与中台一致的顺序（先校验、通过才进插件体），校验不通过会短路并给出
  退出码 1，便于塞进脚本。
- `conform` 把 `conformance.Runtime` 的检查跑一遍并打印报告（健康时是五项），有任何 `✗` 退出码 1。
- **`e2e` 是第一组的对照组，不能拿第一组代替它**：前五个连的都是插件自己的地址，
  证明不了中台能不能按你登记的 `HUB_ADVERTISE_ADDR` 拨到你——那是网络视角的事，
  只有 `e2e` 答得了。它要两个参数：`<中台地址> <插件名>`。

`e2e` 的 `<中台地址>` 是**中台的 HTTP 面**，不是插件面：

| 在哪敲 | 写什么 |
|---|---|
| 本地中台 | `http://127.0.0.1:8092` |
| 开发机 → UAT | `https://hub.example.com:8081/hub-api` |

⚠️ 别写 `127.0.0.1:8095`——那是 **UAT 主机内部**的中台监听端口（8092 在 UAT 上被 anc
平台后端占了），在你机器上敲连的是你自己。拿不准就先 `curl <中台地址>/health`，
探活不鉴权，看到 `{"status":"ok",...}` 就说明地址对了。

判通过看退出码：只有「状态码 200 **且**响应是 JSON **对象**、且对象里的 `plugin` 是个**非空字符串**」
才算过。两种假绿都判 1：`200` 但响应不是 JSON（nginx 的错误页、中间层的一张 200 页面），以及 `200`
也是 JSON 对象、但 `plugin` 缺失或是空的（中间层把一份错误响应配成 200 转发过来）——防的就是验收
脚本被假绿骗过去。

> 完整的四道关（L1 契约自洽 / L2 运行时自检 / L3 中台接受注册 / L4 端到端调用）、
> 每关的判据与失败排查，见 [`docs/plugin-onboarding.md`](../docs/plugin-onboarding.md)。

## 开发这个 SDK 本身

```bash
# 1. 改了 proto 之后重新生成 Go 代码（需要 protoc 与两个 protoc-gen 插件）
PROTOC=/path/to/protoc PROTOC_INCLUDE=/path/to/protoc/include bash generate.sh

# 2. 测试
go test ./...
```

`proto/hubv1/` 的生成产物**提交进仓库**，插件团队因此不需要装 protoc。

内网拉依赖需要 Go module 代理：

```bash
export GOPROXY=https://goproxy.cn,direct
```
