# 插件接入指南

给**要写 anc-hub 插件的人**：从零到「接入成功」的完整路径。

## 三份文档的分工

| 文档 | 回答什么 |
|---|---|
| **本文** | **流程**——接入要过哪几关、每关怎么验、不过怎么办 |
| [`sdk/go/README.md`](../sdk/go/README.md) | **工具**——SDK 六件套各自的用法 |
| [主 README](../README.md) | **架构**——中台是什么、为什么这么设计 |

本文只讲「怎么走通」，不重复工具用法。遇到某个工具的具体参数，去 SDK 那份看。

## 什么叫「接入成功」

两件事，仅此而已：

1. **中台接受了你的注册**
2. **经中台真实调用一次，正常返回**

但它被拆成**四道关**——不是因为流程繁琐，而是因为**每一关失败的原因和验证手段完全不同**，混在一起看会让人无从下手。

## 四道关

| 关 | 验的是什么 | 怎么验 | 需要中台吗 |
|---|---|---|---|
| **L1 契约自洽** | manifest 与 proto 一致、至少声明一条契约、工具名合法 | `go test ./...` | 不需要 |
| **L2 运行时自检** | gRPC 起得来、Health/Describe 应答、空信封不崩 | `hubprobe conform <插件地址>` | 不需要 |
| **L3 中台接受注册** | 上报地址拨得通、契约兼容、版本/实例不冲突 | **启动插件，看它的日志** | 需要 |
| **L4 端到端调用** | 经中台 ingress 真实调用一次 | `hubprobe e2e <中台地址> <插件名>` | 需要 |

### ⚠️ 先记住这一句：L1、L2 全绿 ≠ 能接入

前两关验的是「**插件自己是否自洽**」。它们**够不着最难的两件事**：

- **中台能不能拨通你上报的地址**——这是**网络视角**的问题。你本机测自己通不通，证明不了中台拨得到（中台可能在另一台机器、另一个网段，中间还隔着 nginx）。
- **你的契约与已登记版本冲不冲突**——这要跟中台库里的**历史版本**比对，插件侧根本没有那份数据。

这两件事只有 **L3** 能答。所以 L1/L2 全绿之后**不要直接宣布接入完成**——真正的分水岭在第 3 关。

## 从零到接入成功

```bash
# ── 准备：把 SDK 路径记下来，后面全程要用
export HUB_SDK=/path/to/anc-gateway/sdk/go

# ── 生成工程
cd $HUB_SDK
go run ./cmd/hub-plugin new order-reader --dir /tmp/order-reader

cd /tmp/order-reader
# 改 go.mod 里的 replace 指向 $HUB_SDK 的实际路径（生成时是占位符），然后：
go mod tidy

# ── L1 契约自洽
go test ./...

# ── L2 运行时自检：先把插件跑起来（连不上中台不影响本地自检）
HUB_ADDR=http://127.0.0.1:8093 \
HUB_ADVERTISE_ADDR=http://127.0.0.1:9000 \
go run . &
go -C $HUB_SDK run ./cmd/hubprobe conform http://127.0.0.1:9000

# ── L3 中台接受注册：看上面那个 `go run .` 的输出（JSON 日志，打到 stderr）
#      成功 → {"level":"INFO","msg":"已注册到中台","plugin":"order-reader",...}
#      失败 → {"level":"ERROR","msg":"中台拒绝了注册","code":"UNREACHABLE","message":"...","detail":"..."}

# ── L4 端到端调用：8092 是**本地**中台的 HTTP 面
go -C $HUB_SDK run ./cmd/hubprobe e2e http://127.0.0.1:8092 order-reader \
  --payload '{"text":"你好"}'
```

> 打 UAT 时把最后一条的地址换成 `https://hub.example.com:8081/hub-api`（经 nginx），
> **不要**写 `127.0.0.1:8095`——那是 UAT 主机内部的监听地址，在你自己机器上敲连的是你自己。
> 详见下面的「端口速查」。

> `go -C $HUB_SDK run ./cmd/hubprobe` 里的 `-C` 不能省：`hubprobe` 住在 SDK 这个 module 里，
> 而你的插件工程是另一个 module，直接从插件目录 `go run` 它会报
> `stat /tmp/order-reader/cmd/hubprobe: directory not found`（Go 说的是「这个目录不存在」，
> 不是「包里没有 main」——别被自己的预期带偏）。
> 嫌麻烦就先构建一份：`go -C $HUB_SDK build -o ~/bin/hubprobe ./cmd/hubprobe`，之后直接用 `hubprobe`。

### 端口速查

接入过程中要打交道的端口有好几个，**很容易搞混**。先分清两件事：

- **谁在听**：8092 / 8093 是**中台**在听，9000 是**你的插件**在听。
- **站在哪看**：下文命令里的地址一律是**调用者视角**——你在**自己的开发机上**敲什么。
  UAT 那一列给的是同一件事的**开发机可敲地址**，不是 UAT 主机内部的监听端口。

| 端口 | 是什么 | 本地（默认） | UAT（在开发机上） |
|---|---|---|---|
| **8092** | 中台 **HTTP 面**（ingress / admin / MCP）——`hubprobe e2e`、`curl <地址>/health` 指这里 | `http://127.0.0.1:8092` | `https://hub.example.com:8081/hub-api`（nginx 转给 UAT 主机上的 `127.0.0.1:8095`） |
| **8093** | 中台 **插件面**（gRPC）——插件的 `HUB_ADDR` 指这里 | `http://127.0.0.1:8093` | `https://hub.example.com:8094`（nginx TLS 转给 8093） |
| **9000** | **插件自己**的监听端口（`HUB_LISTEN_ADDR`）——也是 `HUB_ADVERTISE_ADDR` 该指向的地方 | `:9000` | 同 |

> **两个环境不能换着用**。本地照 UAT 写 `127.0.0.1:8095` 会 `connection refused`——那个端口
> 只在 UAT 主机内部存在；反过来在开发机上把 UAT 写成 `127.0.0.1:8092` 连的是你自己机器。
> 这一条踩起来最费时间，因为报错只有一句 connection refused，看不出是端口问题。

> UAT 主机上中台为什么监听 8095 而不是 8092：**8092 在 UAT 上已经归 anc 平台后端**，
> 抢它会直接打挂现有服务。所以那是个**部署视角**的数字——只有登到 UAT 主机上看它才有意义，
> 你在开发机上要连的是上面的 nginx 域名地址。

**别猜，自己验一下**（探活不鉴权，这是最省事的自证动作）：

```bash
curl http://127.0.0.1:8092/health          # 本地
# → {"status":"ok","name":"anc-hub","version":"0.1.0","uptime_seconds":1515}

curl https://hub.example.com:8081/hub-api/health   # UAT
```

看到 `{"status":"ok",...}` 就说明地址是对的；`connection refused` 是**端口**不对，
`404` 或一段 HTML 是**前缀**不对（本地多加了 `/hub-api`，或 UAT 漏了它——nginx 那边
只认 `/hub-api/` 这个前缀）。

> 8093 是**中台在听**，9000 是**你在听**——`go run .` 启动后日志里报的监听端口，
> 和 `HUB_ADDR` 要填的中台端口，是两回事。

---

## L1 契约自洽

### 做什么

实现四个方法：`Manifest()` / `Descriptor()` / `Validate()` / `Handle()`。

脚手架生成的工程已经给了一份能跑的实现，照着改即可。契约细节见 [`sdk/go/README.md`](../sdk/go/README.md) 的「最小插件」。

### 怎么验

```bash
go test ./...
```

脚手架生成的 `plugin_test.go` 里已经有一个 `TestConformance`，跑的就是它。

### 全绿意味着什么

全绿时报告是下面这六行（**一行一项，顺序固定**；只有 manifest 为 nil 或 descriptor
解析不了时会提前收尾，那时行数更少）：

| 检查项 | 通过了意味着 |
|---|---|
| manifest 存在 | `Manifest()` 没返回 nil |
| 插件名合法 | 只含字母数字与 `-_`、≤64 字符、以字母数字开头 |
| 版本号存在 | 不空——flow 靠它锁定实例 |
| descriptor 可用 | 提交的 proto 编译产物能解析 |
| **声明的类型都在 descriptor 中** | **manifest 的自述与 proto 一致**——改了 proto 忘了同步 manifest 会在这里被抓住 |
| 工具声明合法 | 工具名能拼进 MCP 标识、且同一插件内不重名 |

其中 **`声明的类型都在 descriptor 中` 与 `声明了契约` 是同一行的两种形态**，一次报告里只会出现一条：

- 声明了类型 → 出现 `声明的类型都在 descriptor 中`，检查这些类型在不在 descriptor 里；
- 一条都没声明（既没有 `produces` 也没有 `consumes`）→ 出现 `声明了契约`，直接判 ✗。

`descriptor 可用` 在「只用 `google.protobuf.Struct` 承载 JSON 载荷」时显示为「无自有 proto」，**这是合法的**，不是缺省。

### 不过怎么办

报告里 `✗` 那一行直接写了原因：

```
契约一致性检查 order-reader@1.0.0
  ✓ manifest 存在
  ✓ 插件名合法 —— order-reader
  ✓ 版本号存在 —— 1.0.0
  ✓ descriptor 可用 —— 无自有 proto（只用 well-known 载荷）
  ✗ 声明的类型都在 descriptor 中 —— produces 里的 wms.v1.Order —— manifest 的自述必须与提交的 proto 一致
  ✓ 工具声明合法 —— 未声明工具（仅参与 flow）
```

按这行改就行。**注意 `✗` 那句只说「manifest 自述与 proto 对不上」，不说「在 descriptor 中不存在」**——
「在 descriptor 中不存在」是**中台**那边的措辞（L3 被拒时才会见到），别拿它去本地报告里对号入座。

另一条常见的 ✗ 是**根本没声明契约**：

```
  ✗ 声明了契约 —— 既没有 produces 也没有 consumes —— 至少声明一个，接受直接调用的插件请声明 google.protobuf.Struct
```

**这一条是本地比中台更严**：中台接受一份 `produces` / `consumes` 都为空的 manifest（实测注册成功），
本地这关却会红。所以别指望「中台能收就说明没问题」——本地这关的判据是「你这个插件对外的契约说清楚了没有」，
对直接调用的插件就是声明 `google.protobuf.Struct`。

> 除这一条外，L1 检的确实就是中台注册期的那几条，所以 L1 过了，L3 不会因为 manifest /
> descriptor 自洽问题被拒。

---

## L2 运行时自检

### 做什么

把插件**真的跑起来**（`go run .`）。

这一步**不需要中台**：插件连不上中台会自己按 5 秒间隔重试，不影响你本地自检。

### 怎么验

```bash
go -C $HUB_SDK run ./cmd/hubprobe conform http://127.0.0.1:9000
```

实测输出长这样：

```
契约一致性检查 http://127.0.0.1:9000
  ✓ Health 可应答
  ✓ Describe 可应答 —— order-reader@1.0.0
  ✓ 校验器对空信封不崩
  ✓ 校验器可处理 JSON 载荷 —— 拒绝（1 条问题）—— 探针载荷不满足业务规则属正常
  ✓ 插件体返回信封 —— 已返回 JSON 载荷
```

> 「校验器可处理 JSON 载荷 —— **拒绝**」是**正常结果**，不是失败：探针载荷本来就不满足你的
> 业务规则，只要不是超时或 panic 就算过。别看到「拒绝」就以为这关红了。

### 全绿意味着什么

健康时报的是这五项：

| 检查项 | 通过了意味着 |
|---|---|
| Health 可应答 | 插件起来了、地址拨得通、且自报健康 |
| Describe 可应答 | manifest 能正常拉取 |
| 校验器对空信封不崩 | 收到畸形输入时 `Validate` 不 panic、不挂住 |
| 校验器可处理 JSON 载荷 | 正常载荷能走通（**被业务规则拒绝也算通过**——探针载荷本来就可能不满足你的规则） |
| 插件体返回信封 | `Handle` 返回了非 nil 信封 |

### 不过怎么办

| 现象 | 多半是 |
|---|---|
| **Health 可应答 ✗** | **最常见**。三种成因按发生频率排：地址/端口写错（比如把中台的 8092/8093 当成了插件地址、插件实际没监听 9000）→ 插件没起来 → 插件起来了但自报不健康。报错文本里已经给了 `dial tcp <地址>: connect: connection refused` 或 `produced zero addresses` 这类原话，照着查 |
| Describe 可应答 ✗ | `Manifest()` 有问题，回 L1 |
| 校验器对空信封不崩 ✗ | `Validate` 里对 `nil` / 缺字段没做防御 |
| 校验器可处理 JSON 载荷 ✗ | `Validate` 在这个载荷上**超时或 panic** 了（被业务规则拒绝**不算** ✗，见上） |
| 插件体返回信封 ✗ | `Handle` 返回了 nil——中台会把它当成插件异常 |

> **「插件地址可用 ✗」为什么不在这张表里**：它几乎不可达。
> `conformance.Runtime` 只在 **`grpc.NewClient` 构造失败**时才会打出那一行——那是 target
> 字符串**连解析器都建不起来**（实测只有非法百分号转义这类，如 `a%zzb`）；而本地检查用的是
> **惰性连接**（`grpc.NewClient` 只登记 target，不发包），所以「地址写错、端口没人听、
> 名字纯属乱写」这类最常见的问题**当时不报错**，要等第一次 RPC 才浮出来——于是全部落在
> 「Health 可应答 ✗」。
>
> 实测 18 个坏地址（`not-a-real-address`、`''`、`:`、`::::::::`、`http://[::1` 等），
> **16 个落在「Health 可应答 ✗」**，一个也没落在「插件地址可用」上；只有 2 个真的触发了那一行
> （`a%zzb`、`http://127.0.0.1:%zz`——都是非法百分号转义，target 连解析器都建不起来）。
> 也就是说，「地址写错」这件事根本不从那行进。下面挑三个坏地址看：
>
> ```
> $ hubprobe conform http://127.0.0.1:9555      # 端口没人听
>   ✗ Health 可应答 —— rpc error: code = Unavailable desc = connection error: desc = "transport: Error while dialing: dial tcp 127.0.0.1:9555: connect: connection refused"
>
> $ hubprobe conform not-a-real-address          # 乱字符串
>   ✗ Health 可应答 —— rpc error: code = Unavailable desc = name resolver error: produced zero addresses
>
> $ hubprobe conform ''                          # 空串
>   ✗ Health 可应答 —— rpc error: code = Unavailable desc = delegating_resolver: invalid target address "": missing address
> ```
>
> 所以排查从 **「Health 可应答 ✗」那一行**入手，**不要**去翻健康检查逻辑——那是最后一个可能，
> 不是第一个。

---

## L3 中台接受注册

**这一关是分水岭。**

### 做什么

让插件的 `HUB_ADDR` 指向**中台的插件面**（不是 HTTP 面），然后启动它：

```bash
HUB_ADDR=http://127.0.0.1:8093  \    # 中台插件面，gRPC
HUB_ADVERTISE_ADDR=http://127.0.0.1:9000 \  # 你希望中台回连你的地址
go run .
```

跨机部署时 `HUB_ADDR` 走 nginx 的 TLS 端点，例如 `https://hub.example.com:8094`。

### 怎么验：看插件的启动日志

**这是最反直觉的一点**——中台不会主动通知你「注册成功了」，结果只出现在**插件自己的日志**里。

插件日志是 **JSON**（`slog.NewJSONHandler`，打到 stderr）。启动时会连打几行，
**认准最后那句 `"msg":"已注册到中台"`**：

```json
{"time":"...","level":"INFO","msg":"插件 gRPC 已监听","listen":"[::]:9000","advertise":"http://127.0.0.1:9000"}
{"time":"...","level":"INFO","msg":"插件已启动","hub":"http://127.0.0.1:8093","instance":"order-reader-1"}
{"time":"...","level":"INFO","msg":"已注册到中台","plugin":"order-reader","version":"1.0.0","instance":"order-reader-1"}
```

失败（按 `RetryInterval` 默认 5 秒重试，所以会反复出现）——**每条拒绝原因独占一行，
直接看就行，不需要任何工具**。下面是实测抓的（构造「新版本号里删掉了基线字段」这一种）：

```json
{"time":"2026-09-18T10:41:58.541654+08:00","level":"INFO","msg":"插件已启动","hub":"http://127.0.0.1:8093","instance":"l3probe-1"}
{"time":"2026-09-18T10:41:58.549264+08:00","level":"ERROR","msg":"中台拒绝了注册","code":"BREAKING_CHANGE","message":"相对版本 1.0.0 存在破坏性契约变更","detail":"wms.v1.Order.sku（编号 1，类型 string）已被删除；基线版本里已有的字段不能删、改类型或改编号，只能新增——请把这些字段按原编号原类型改回再注册；若确实要重新设计契约，需由中台侧先清掉旧版本基线（hubctl remove-version）"}
{"time":"2026-09-18T10:41:58.549272+08:00","level":"ERROR","msg":"注册未通过，稍后重试","reasons":1,"retry_in":"5s"}
```

**`detail` 会把该改什么直接写出来**（这里就是「按原编号原类型改回」），所以别只看 `code` 那一列——
`code` 只够定位到一类问题，`detail` 才是这一条的具体出路。

有几条原因就打几行，**最后一行是汇总**：`reasons` 是原因条数，`retry_in` 是下次重试的间隔。
两条原因时是这样（实测：manifest 声明了两个 descriptor 里都没有的类型）：

```json
{"time":"2026-09-18T10:46:06.221636+08:00","level":"ERROR","msg":"中台拒绝了注册","code":"MANIFEST_INVALID","message":"manifest 声明的 produces 类型 wms.v1.Nope 在 descriptor 中不存在","detail":"manifest 的自述必须与提交的 proto 一致；要么在 proto 里定义这个类型并重新生成提交的 descriptor，要么把这条声明从 manifest 里删掉"}
{"time":"2026-09-18T10:46:06.221643+08:00","level":"ERROR","msg":"中台拒绝了注册","code":"MANIFEST_INVALID","message":"manifest 声明的 consumes 类型 wms.v1.AlsoNope 在 descriptor 中不存在","detail":"manifest 的自述必须与提交的 proto 一致；要么在 proto 里定义这个类型并重新生成提交的 descriptor，要么把这条声明从 manifest 里删掉"}
{"time":"2026-09-18T10:46:06.221646+08:00","level":"ERROR","msg":"注册未通过，稍后重试","reasons":2,"retry_in":"5s"}
```

> **中台连不上**（不是被拒，是网络不通）走的是另一条路，`err` 字段里是 grpc 的错误文本（实测）：
>
> ```json
> {"time":"2026-09-18T10:44:29.490482+08:00","level":"ERROR","msg":"注册未通过，稍后重试","err":"调用中台注册接口失败: rpc error: code = Unavailable desc = connection error: desc = \"transport: Error while dialing: dial tcp 127.0.0.1:59999: connect: connection refused\"","retry_in":"5s"}
> ```
>
> 这一档**只有一行**（不拆条），要查的是网络与地址——别拿 `err` 里的文本去对下面的拒绝码表。

拒绝原因是结构化的，逐条排开——**照着改就行，不必去翻中台的日志**。

### ⚠️ `HUB_ADVERTISE_ADDR` 是接入时最大的坑

中台在注册时会**连你上报的地址做可达性探测**，**探不通直接拒绝注册**。

判断标准是「**中台的视角**」能不能拨通，不是「我自己能不能连上」：

| 情形 | 该填什么 |
|---|---|
| 中台与插件**同机** | `http://127.0.0.1:9000` 可以 |
| 中台在**别的机器** | 必须是中台能拨通的**内网 IP 或域名**，如 `http://203.0.113.10:9000` |

**写成 `localhost` 是最常见的错**：在你本机看它是通的，但中台在另一台机器上，`localhost` 指的是**中台自己**。

> **实证**：UAT 上验过插件跑在独立 docker 网络里（独立 IP、独立网络命名空间），
> 自报非回环地址 `http://172.30.99.10:9000`，经 `https://hub.example.com:8094`
> 的 TLS 端点注册，中台成功拨通该地址——**自报的地址被采纳**，而不是被改写成来源 IP。

注意 `HUB_ADVERTISE_ADDR` 是**端口**地址，而 `go run .` 监听哪个端口由 `HUB_LISTEN_ADDR` 决定（默认 `:9000`）。两者指的不是同一个东西，别混淆。

### 不过怎么办

按拒绝码：

| 拒绝码 | 含义 | 怎么办 |
|---|---|---|
| `UNREACHABLE` | 中台拨不通你上报的地址，**或者拨通了但你的 Health 自报不健康** | 前者查 `HUB_ADVERTISE_ADDR`（见上）——**最常见的一类**；后者是另一回事：查插件自身的健康检查逻辑与它的下游依赖 |
| `MANIFEST_INVALID` | manifest 自述有问题 | 回 L1，拒绝原因会指出是哪个字段 |
| `DESCRIPTOR_INVALID` | 提交的 descriptor 解析不了 | 回 L1。注意它要的是 proto 的**编译产物**（`FileDescriptorSet` 的编码字节），不是 `.proto` 源码 |
| `TOOL_CONFLICT` | 工具名冲突 | 改工具名（同一插件内必须唯一） |
| `VERSION_CONFLICT` | 同一版本号但契约变了 | **升版本号**——版本号是制品身份，同号不可改契约 |
| `BREAKING_CHANGE` | 相对基线版本删了字段、改了类型或字段编号 | **把那些字段按原编号、原类型改回去**。⚠️ **升版本号在这里没用**——这条恰恰只在你**已经用了新版本号**时才会触发，"再升一个版本"不解决任何问题。确实要重新设计契约，得由中台侧先清掉旧版本基线 |
| `INSTANCE_CONFLICT` | `instance_id` 为空 / 超长 / 被**别的插件**占用 | 撞车多半是几个插件的 `HUB_INSTANCE_ID` 写成了同一个值。⚠️ **空值语义容易记反**：`HUB_INSTANCE_ID=`（**空字符串**）等同没设——SDK 照样自动生成「主机名-PID」，**注册得成**（实测）；只有**纯空白**（如 `HUB_INSTANCE_ID=" "`）才会带着一个空白 id 去注册，被中台按 `注册请求缺少 instance_id` 拒掉（实测）。真正会踩的是从别处复制粘贴时带上的那个空格 |
| `INTERNAL` | 中台内部错误 | 不是你的问题。重试后仍失败，把拒绝原因里那段错误信息交给中台维护方 |

> ⚠️ **`INSTANCE_CONFLICT` 的症状是不对称的，只有一半会显式报错。**
>
> 中台**只在注册时**挡「拿别人的 id 注册」。A 先注册占住这个 id、B 拿同一个 id 来注册
> 被拒之后，是这样一条链：
>
> | 什么时候 | 发生了什么 | 当事人看到什么 |
> |---|---|---|
> | B 注册 | 被拒——A 是属主 | B **显式**收到 `INSTANCE_CONFLICT`，一直重试 |
> | B 重启（发布就会） | 修复前：B 退出时按 `instance_id` 注销，**删掉的是 A 那一行** | — |
> | B 的新进程重试成功 | B 占住这个 id | B 起来了，一切正常 |
> | A 的进程**始终没重启** | 它的心跳 `WHERE instance_id = $1` 命中 **B 那一行**、照样返回 accepted | ⚠️ A **认为自己健康**，日志从此不再有任何输出 |
>
> 最后一行是最坑的，坑在**没有任何症状指向 A**：只要 A 的进程不重启，它就永远发现不了
> 自己被顶掉了——心跳被 accepted 是它唯一的健康判据，而那一行现在属于 B。日志不会说，
> 接口不会说，只有 MCP 工具面上它的工具悄悄没了。
>
> 实测：auth 重启一次，sql-executor 的工具在工具面上全部消失，而 sql-executor
> 自己的日志停在「已注册到中台」之后再无输出。
>
> 注销现在也要凭注册时下发的凭证认属主（`UnregisterRequest.state_token`），**第二行那条
> 路径已经堵死**：被拒的一方手里根本没有凭证，注销发不出去（旧版 SDK 发了也会被中台拒），
> 它那一次重启不再伤得到 A。但它堵不住**撞车本身**——只要 A 自己重启（发布就会），位置就
> 空出来，B 可能抢到、也可能被 A 的新进程抢回（这是竞速，不是谁稳拿）；区别在于 A 的新
> 进程这次会**显式**收到拒绝并一直重试，不再是无声的。
>
> **所以别指望中台帮你发现撞车**——每个插件都给一个不同的 `HUB_INSTANCE_ID`
> （见 `deploy/.env.example`）。协议这一层只保证「谁都删不掉别人的行」，
> 「两个插件别撞」仍然是配置的责任。

---

## L4 端到端调用

### 做什么

经**中台的 ingress** 调一次自己的插件——走的是「中台 → 插件」的完整真实链路，**不是直连插件**。L2 直连插件验的是插件本身，这里验的是**中台真的能调到它**。

### 怎么验

```bash
go -C $HUB_SDK run ./cmd/hubprobe e2e http://127.0.0.1:8092 order-reader \
  --payload '{"text":"你好"}'
```

`<中台地址>` 是**你要连的那个环境**的中台 HTTP 面，命令会把它**当前缀**再拼
`/ingress/<插件名>`，所以带不带 `/hub-api` 都对：

| 在哪敲 | `<中台地址>` 写什么 |
|---|---|
| 你自己机器上的本地中台 | `http://127.0.0.1:8092` |
| 开发机 → UAT | `https://hub.example.com:8081/hub-api` |

> 这两种**不能换着用**：本地写 8095 会 `connection refused`，UAT 写 8092 连的是你自己。
> 拿不准就先 `curl <中台地址>/health`，看到 `{"status":"ok",...}` 就对了。

### 全绿意味着什么

`hubprobe e2e` 判通过的标准是「状态码 `200`、响应是 JSON 对象、且里面的 `plugin` 是个**非空
字符串**」——中台的 200 成功响应按定义必然带它，缺了或为空就说明这份响应不出自中台（详见下面的注）。
过了就说明：中台认得这个插件、选中了一个健康实例、拨通了它、插件的校验器放行、插件体执行并返回了
信封。

> **判据为什么不挂在 `payload` 上**：`payload` 只在插件回 `Struct` 载荷时才有。回业务类型的
> 插件给的是 `payload_type_url` / `payload_base64`，载荷超内联上限时给的是 `payload_ref`——
> 拿 `payload` 当判据会把这两类正常插件判成失败。这是验收工具，误报（正常响应判成失败）
> 比漏报更糟：被误伤的人第一反应是怀疑工具本身。
>
> 反过来，判据也不能松到「200 + 任意 JSON 对象」就算过：中台自己的 404 错误体
> （`{"error":"not_found","message":"插件 x 未注册"}`）正是个 JSON 对象，中间层把一份错误
> 响应配成 200 转发过来，验收脚本就被骗过去了。

### 不过怎么办

按状态码：

| 状态码 | 含义 | 怎么办 |
|---|---|---|
| `200` | 成功 | — |
| `404` | 插件未注册 | 回 L3——注册没成功，或插件名拼错了。`<中台地址>/admin/plugins` 能列出已注册的名字 |
| `422` | 校验器拒绝 | 看返回的 `issues`，是你自己的校验规则拒的；换个满足规则的载荷。⚠️ **大载荷也会落到这一档**，见下面的「4MB / 8MB 两条线」 |
| `429` | 实例过载（并发到顶且排队超时，或总线堆积到顶） | 不是这份数据的错。稍后重试；一直如此就看 `<中台地址>/admin/governance` 里那个实例的 `available` 与 `in_flight` |
| `502` | 插件不可达 / 超时 | 实例还在但拨不通了——查 `HUB_ADVERTISE_ADDR`、插件进程是否还活着。先看 `/admin/plugins` 里它的 `instance_count` 是不是 0 |
| `503` | **熔断**——该实例连续失败后已被停止放行 | 稍后重试（冷却默认 10 秒后放一个探测请求过去）。去看 `/admin/governance` 里它的 `consecutive_failures` 与熔断状态 |
| `413` | 请求体超过 8MB | 减小载荷——但**先读下面那段**，靠 CLI 你打不到这一档 |

> 注意 `422` 与 `404` / `502` 的**语义完全不同**：`422` 说明**链路是通的**，是你的业务校验
> 主动拒绝——这其实是个好信号，说明前三关都过了。

#### 4MB / 8MB 两条线，以及为什么「改用引用通道」对多数插件不成立

中台对载荷有**两条**大小线，别混：

| 线 | 数值 | 越线后 |
|---|---|---|
| 内联上限 | **4MB** | 载荷不再放进信封，改落库并把 `payload_ref` 留在信封里（引用通道） |
| 请求体硬上限 | **8MB** | 在进中台 handler 之前就被挡回 **413**，插件根本收不到这次调用 |

于是有两个容易踩空的地方：

1. **8MB 这一档，靠 `hubprobe e2e --payload` 永远打不到。** 载荷要经命令行参数传，
   而 **macOS 的 `ARG_MAX` 是 1MB**（`getconf ARG_MAX` → `1048576`），参数根本长不到 8MB。
   真要看 413，得用文件传：`curl --data-binary @big.json -X POST <中台地址>/ingress/<插件名>`，
   响应是很朴素的一行纯文本 `Failed to buffer the request body: length limit exceeded`。
2. **「改用 `/blobs` 引用通道」对「只用 `google.protobuf.Struct` 承载 JSON」的插件不成立。**
   超 4MB 后中台把载荷挪走，信封里**只有 `payload_ref`**——实测（5MB 载荷，探针插件把看到的
   信封形状报回来）：

   ```
   约 1KB 载荷 → issues: 看到的是内联 payload，payload_ref 为空
   约 5MB 载荷 → issues: 看到的是引用：payload 为空，payload_ref.uri=/blobs/01M2S6BZF4X361ZPGAPSVPJ6S5
   ```

   而 `hubkit.PayloadJSON(env)` 取的是信封里的 `payload`，此时它是空的——校验器会直接返回
   **`422 需要 JSON 对象载荷`**（用脚手架生成的工程实测，5MB 载荷就是这个结果）。
   插件要想吃得下大载荷，得**自己按 `payload_ref.uri` 去取**（`GET /blobs/{id}`，TTL 1 小时）；
   在这件事落地之前，Struct 型插件实际能收的上限就是 **4MB**，不是 8MB。

---

## 排查索引

**先定位在哪一关，再往下查**——四关的失败原因几乎不重叠，混着查最费时间。

| 现象 | 大概率在哪一关 |
|---|---|
| `go test ./...` 红了 | L1 |
| `hubprobe conform` 有 `✗` | L2 |
| 插件日志里刷「注册未通过」 | L3 |
| `hubprobe e2e` 返回 404 / 502 | L3 或 L4（先看 L3 日志） |
| `hubprobe e2e` 返回 422 | 已接入成功，是业务校验拒绝（或载荷超了 4MB，见 L4） |
| `hubprobe e2e` 返回 429 / 503 | L4——中台的实例级治理（过载 / 熔断），不是插件没接好 |

**插件日志是 L3 的唯一入口**。接入过程中遇到问题，第一件事是看它——不是看中台日志，
也不是看控制台。中台把拒绝原因原样回给了插件，`hubkit` 已经把它们排开了。

## 接入之后

- **控制台**能看到你的插件、契约、MCP 工具与实例健康（见[主 README](../README.md) 的「控制台」）
- ✅ **manifest 里声明的工具会自动聚合进 MCP 工具面**，名字是 `插件名__工具名`。
  注册成功之后 agent 拉一次 `tools/list` 就能看见并调用，中台不用重启：

  ```
  POST /mcp → tools/list
    auth__check_login / auth__login           ← auth 插件声明的
    sql-executor__sql_query / __sql_submit / __sql_execute / __sql_audit
    list_plugins / trigger_flow / get_trace / …   ← 中台内置的 18 个
  ```

  实测数字（2026-09-18）：UAT 上装了 auth 插件 → **20 个**（18 内置 + 2 插件）；
  本地装上 SQL 执行器 → **22 个**（18 + 4）。此前是 17 个内置、一个插件工具都没有。

  **实例被摘除后它的工具随之消失**——`list_tools` 每次都按「有没有在线实例」现查，
  不会留下一个 agent 调得到却永远调不通的死工具。

  ⚠️ **工具面变了不会主动通知已连接的 agent**：`notifications/tools/list_changed`
  还没实现。所以 agent 要**重新拉一次** `tools/list` 才看得见新插件。对刚接入的插件
  通常不是问题（agent 会话一般是新开的），但**插件下线或重启时**要留意——正在跑的
  agent 可能在它消失之前又调了一次，那次会得到「没有在线实例」。

- **两条调用路径的载荷结构一致**，agent 用哪条都行：

  | 路径 | 载荷来源 | 工具短名从哪来 |
  |---|---|---|
  | 聚合工具 `sql-executor__sql_query` | `arguments` 本身 | 中台塞进 `meta["hub.tool"]` |
  | 通用入口 `invoke_plugin` | 整个 `payload` | 插件从 `payload["op"]` 自己取 |

  插件侧按 `hub.tool` 优先、`payload.op` 兜底来分发，两条路就都能走通。

- 要在**编排流**里当节点用，上下游的消息类型必须对得上——那是编排时的校验，比接入更严，
  但不在本文范围内

---

## 插件互调与发现

插件之间要互相调用时，**不直连**——直连会绕过中台的鉴权、审计、熔断与限额。同步互调
一律 **A→hub→B**：你把请求交给中台的 `PluginGateway.Invoke`，中台鉴权、防环、记账之后
替你调下游，复用与直调完全同一条 Invoker 链路。发现（谁在线、谁生产/消费什么消息、
契约长什么样）也只从中台查，不另立事实源。

### SDK 用法

五门 SDK 同构（Go / Python / Node / Rust / C#），概念逐个对应。骨架在**每次注册成功后**
把 `GatewayClient` 注入插件（Go 是可选接口 `GatewayAware.SetGateway`，Python 是
`set_gateway` 方法），凭证在客户端内部随注册轮换，插件作者不碰 token。

以 Go 为例（Python 见下，其余三门同名概念）：

```go
// 可选接口：实现了才会被注入；每次重新注册都会再调一次
func (p *Plugin) SetGateway(c *hubkit.GatewayClient) { p.mu.Lock(); p.gateway = c; p.mu.Unlock() }

// handle 里：先发现，再互调
online, _ := c.ListPlugins(ctx, false)               // 谁在线（缺省只列有健康实例的）
desc, _ := c.DescribeMessage(ctx, "wms.v1.OrderCreated") // 这个消息谁生产、谁消费
contract, _ := c.GetContract(ctx, "ping-callee", "", "") // 契约 + invokes + 字段级 schema

out, err := c.InvokePlugin(ctx, "ping-callee", hubkit.InvokeOptions{
    PayloadJSON:     map[string]any{"text": "hello"},
    CurrentEnvelope: env,   // 传「我正在处理的信封」：trace 贯通、调用链原样带上
})
```

Python 同一件事：

```python
def set_gateway(self, gateway):
    self._gateway = gateway                      # 见 Go：过锁、判 None

def handle(self, ctx, env):
    online = {s.name for s in self._gateway.list_plugins()}
    downstream = self._gateway.invoke_plugin("ping-callee", {"text": "hello"},
                                             current_envelope=env)
```

**`CurrentEnvelope` / `current_envelope` 必须传「你正在处理的那个信封」**：SDK 复制它的
`trace_id`/`run_id`/`node_id` 让整条链落进同一个 trace，并把 meta 里的调用链**原样**
带上。不要自己往链里追加自己——链的语义是「已处理过该消息的插件序列」，追加 caller
是中台在代调时做的事，客户端自己算是跟中台抢职责，算错就是「本地放行、中台拒绝」。

顶层发起（不是在处理某个信封，比如定时任务里主动调）就不传：SDK 生成新 trace、不带链。

**结果语义**：`HANDLED` 返回下游信封；`REJECTED`（下游校验拒绝，issues 随异常/错误
携带）与 `ERROR`（未声明授权、配额、成环、链深超限、下游出错）是**业务结果**，Go 翻成
`*InvokeRejected` / `*InvokeFailed`，Python 翻成同名异常——按 reason 决定改逻辑还是
退避重试，别无脑重发；基础设施故障（未鉴权、中台不可达）才是 gRPC 错误原样抛。

### 权限声明：manifest 的 `invokes`

manifest 新增可选字段 `invokes`（目标插件名列表），声明「本插件要调用谁」：

```go
&hubv1.PluginManifest{
    // ...
    Invokes: []string{"ping-callee"},
}
```

| 中台 `HUB_PLUGIN_CALL_POLICY` | 行为 |
|---|---|
| `allow`（**默认**） | 未声明也能调——存量插件零改动 |
| `declared` | 只放行 caller 注册版 manifest 里**声明过**的目标，其余一律拒绝 |

声明在 `allow` 下不是必须的，但它是**互调关系的注册表记录**：`get_plugin`（MCP /
管理面）逐版本输出 `invokes`，人工审计「谁被授权调谁」就看它。改了名单要**升版本号**——
同版本号的 manifest 不可变更。

### 限额与防环（中台治理，插件侧只读）

| 机制 | 值 | 撞上时的表现 |
|---|---|---|
| 每插件每分钟互调配额 | **60 次**（Redis 固定窗口，常量无环境变量旋钮） | `ERROR`，reason「本分钟调用已达上限」——业务结果，不是 gRPC 错误 |
| 互调链深度上限 | **8**（含本次 caller） | `ERROR`，reason「互调链已达上限」 |
| 成环检测 | caller 已在 `hub.call_chain` 链上 | `ERROR`，reason「检测到互调环」 |

链在 `Envelope.meta["hub.call_chain"]` 里逗号分隔传递。它只防**同一条消息**在插件间
来回接力；它与 Publish 的触发链（`hub.publish_chain`）是两套互不知晓的链，跨机制成环
（互调里发 Publish、Publish 的 flow 里再互调）靠双向深度上限 + 配额兜底，没有统一检测。

每次互调中台都会落一条审计 span（`gateway.invoke.{目标插件}`，带 caller / target /
outcome / 调用链），`GET /traces` 按 trace 聚合可见——互调不是黑洞。

### 可运行示例

[`examples/ping-chain`](../examples/ping-chain/)：Python 双插件（`ping-caller` 经中台
发现并互调 `ping-callee`），README 有从起真中台到 MCP 调用、观察 trace 贯通与调用链、
再演示 `declared` 策略下收回授权的完整步骤。
