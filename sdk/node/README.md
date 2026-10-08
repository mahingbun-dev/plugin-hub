# anc-hub 插件 SDK（Node / TypeScript）

用 Node 写 anc-hub 插件的工具箱。**五件套**：

| 件 | 位置 | 作用 |
|---|---|---|
| ① 服务端骨架 | `src/`（入口 `@example/hubkit`） | 实现四个方法就有个能跑的插件：gRPC 服务、自注册、心跳、被摘除后自愈、优雅退出全由它处理 |
| ② 契约定义 | `proto/hub/v1` | 与 `crates/hub-proto` **逐字相同**的 `.proto`，**不需要装 protoc** |
| ③ 契约自检 | `@example/hubkit/conformance` | 插件接入前必须跑通的检查，跑的就是中台注册期的规则 |
| ④ mock 中台 | `@example/hubkit/mockhub` | 本地 mock，离线开发与自动化测试用 |
| ⑤ 脚手架 | `src/cli.ts` | `new <name>` 生成可运行的插件工程，`conform <地址>` 跑运行时自检 |

> 与 Go 版 SDK 的差别：Go 版多一件 `cmd/hubprobe`（调试工具，含经中台打真实链路的 `e2e`）。
> 本 SDK 只把其中最有用的 `conform` 放进了脚手架 CLI；`e2e` 那一关目前请用 Go 版：
> `cd sdk/go && go run ./cmd/hubprobe e2e <中台地址> <插件名>`。

## 快速开始

```bash
# 生成一个可运行的插件工程（SDK 源码会一起拷进 <目录>/sdk，npm install 就能装）
node src/cli.ts new order-reader --dir /tmp/order-reader

cd /tmp/order-reader
npm install     # 依赖走 registry.npmmirror.com
npm test        # 含契约一致性检查（L1）
```

生成的插件**零 proto 工具链依赖**：它用 `google.protobuf.Struct` 承载 JSON 载荷，
也就是「直接调用」那条路（agent 经 MCP、外部系统经 HTTP）。需要与其他插件在 flow 里
传递类型化消息时，再加自己的 `.proto`。

## 最小插件

```ts
import { configFromEnv, emptyDescriptor, invalid, issue, payloadJSON, run, valid, withPayloadJSON } from '@example/hubkit'
import type { Plugin } from '@example/hubkit'

const plugin: Plugin = {
  manifest: () => ({
    name: 'order-reader',
    version: '1.0.0',
    consumes: [{ fqName: 'google.protobuf.Struct', description: '直接调用的 JSON 载荷' }],
  }),
  // 只用 Struct 载荷的插件没有自己的 proto，返回空即可
  descriptor: () => emptyDescriptor(),

  // 中台一定先调它，通过才进 handle
  validate: (_ctx, env) => {
    const [payload, ok] = payloadJSON(env)
    if (!ok) return invalid(issue('payload', '需要 JSON 对象载荷'))
    if (typeof payload?.['orderId'] !== 'string') return invalid(issue('payload.orderId', '缺少必填字段 orderId'))
    return valid()
  },

  // 插件体。返回 null 会被中台当成「插件异常」——要表达「拒绝处理」请抛错
  handle: (_ctx, env) => withPayloadJSON(env, { ok: true }),
}

await run(plugin, configFromEnv())
```

运行时直接用 Node 跑 `.ts`：**Node 22.18+ 默认剥离类型，没有构建步骤**。
代价是只能用**可擦除**的语法——`enum` / `namespace` / 构造器参数属性会在运行时直接报
`ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`。`tsconfig.json` 里的 `erasableSyntaxOnly` 会提前标红。

## 实现选型：为什么是 `@grpc/proto-loader` 而不是静态生成

| | 运行时加载 `.proto` | `pbjs -t static-module` 静态生成 |
|---|---|---|
| 契约原文 | 随包带 `.proto`，与中台仓库**逐字可比**（`test/proto_parity.test.ts` 守着） | 生成物几千行，契约漂移要靠人比对 |
| 工具链 | 无（`.proto` + well-known 类型都随包下发） | 需要 protobufjs-cli 生成并提交产物 |
| 体积 | 四份 `.proto` 几十 KB | 生成物比它大一个数量级 |
| 代价 | 没有静态生成的类型 | 类型开箱即用 |

代价那一栏由 `src/types.ts` 手写的一份接口补上（字段名与 `keepCase: false` 一致），
插件作者仍有补全与类型检查。

**⚠️ 一个必须知道的坑：`google.protobuf.Any` 的字段名。**

protobufjs 对 `google/protobuf/*.proto` 用的是它**内置**的那份 JSON 定义
（`node_modules/protobufjs/src/common.js`），而那份定义里 `Any` 的字段名是
snake_case 的 `type_url`（`Struct` / `Value` 反而是 camelCase）。于是
`keepCase: false` 在 Any 上根本不生效：**只写 `typeUrl` 的载荷会被静默序列化成空串**，
中台看到的是「载荷没有类型」。

所以：打包用 `packAny`、读取用 `anyTypeUrl`、写载荷用 `withPayloadJSON`，
不要自己拼 `payload` 对象。骨架在返回信封前还会过一次 `normalizeAnyForWire` 兜底。

## 环境变量

| 变量 | 必填 | 说明 |
|---|---|---|
| `HUB_ADDR` | 是 | 中台插件面地址（本地 `http://127.0.0.1:8093`） |
| `HUB_ADVERTISE_ADDR` | 是 | **中台能拨通**的本插件地址 |
| `HUB_LISTEN_ADDR` | 否 | 本插件监听地址，缺省 `:9000`（`:` 开头会归一成 `0.0.0.0:`，grpc-js 的地址解析要求有主机名） |
| `HUB_INSTANCE_ID` | 否 | 实例标识，缺省「主机名-PID」 |
| `HUB_LOG_LEVEL` | 否 | `debug` / `info` / `warn` / `error`（缺省 info） |

**与 Go 侧的两处运行时差异**（都是运行时的，不是本 SDK 的选择）：

- **TLS 的 CA**：中台证书由内网 CA 签发时，用 `NODE_EXTRA_CA_CERTS=/path/ca.crt`
  （Linux 与 macOS 都认它）。Go 侧用的是 `SSL_CERT_FILE`，而那个变量**在 macOS 上不生效**——
  两个运行时各有一套，别把变量名记混了。
- **`HUB_TLS_MAX_VERSION` 不存在**：那是 Go 侧为「中间设备重置 TLS 1.3 握手」留的逃生口
  （见 Go 版 README）；grpc-js 走的是另一套 TLS 栈，这个变量在这里没有对应物。

## 调试

```bash
# 直连插件的运行时自检（L2，不需要中台在线）
node src/cli.ts conform http://127.0.0.1:9000
```

五项全 ✓ 时退出码 0，有任何 ✗ 退出码 1。注意「校验器可处理 JSON 载荷 —— **拒绝**」
是正常结果：探针载荷本来就不满足你的业务规则。

⚠️ L1、L2 全绿**不等于**能接入：它们验的是「插件自己做得对不对」，而中台能不能按你上报的
`HUB_ADVERTISE_ADDR` 拨通你，这两关都够不着。真正的判据是**插件启动日志里的
「已注册到中台」**（L3）。四道关的完整说明见生成的工程里的 `AGENTS.md`。

## 依赖与镜像

`@grpc/grpc-js`、`@grpc/proto-loader`、`protobufjs` 从 `registry.npmmirror.com` 装；
本机的 npm 已指向它，换机器时 `npm config set registry https://registry.npmmirror.com`。

本 SDK **不提交 lockfile**：随包下发的 SDK 是要被开发者的 `npm install` 重新解析的，
钉死一份为 CI 机器准备的解析结果没有意义，反而会把镜像地址固化进去。

## 开发这个 SDK 本身

```bash
npm install
npm test                 # node --test（含跨语言规则、契约原文比对、注册/心跳/自愈）
npx tsc --noEmit         # 类型检查
node src/cli.ts new demo --dir /tmp/demo   # 手动跑一遍脚手架
```

内网没有 npm 代理之外的依赖，也不需要 protoc——**改了 proto 之后只需把
`crates/hub-proto/proto/hub/v1/*.proto` 复制到 `proto/hub/v1/`**，
`test/proto_parity.test.ts` 会在中台仓库里逐字节守住这件事。

### 随包下发的排除项

生成工程时（以及中台侧 `crates/hub-templates/build.rs` 的 `SDK_SOURCES` 打包时），
`node_modules` / `.git` / `templates` 不随包下发——清单的事实来源是
`src/scaffold.ts` 的 `SDK_COPY_EXCLUDED`，中台侧的跳过清单必须与它一致。
`node_modules` 尤其不能漏：那是几十兆与平台绑定的二进制。

## 已知缺口

- **类型化契约的 descriptor 要自己准备**。只用 `google.protobuf.Struct` 的插件返回空
  descriptor 即可；要用自有消息类型，得提供它的 `FileDescriptorSet`（Go 侧靠 protoc 生成的
  `protoreflect.FileDescriptor`，Node 侧没有等价物）。`src/descriptor.ts` 只实现了**解析**
  （给自检用），没实现构造。
- **`HandleStream` 返回 UNIMPLEMENTED**，与 Go 侧一致（流式随 M3 落地）。
- **没有 `hubprobe e2e`**（L4）。那一关目前要用 Go 版 SDK。
