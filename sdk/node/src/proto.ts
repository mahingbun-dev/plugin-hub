import path from 'node:path'
import { fileURLToPath } from 'node:url'

import * as grpc from '@grpc/grpc-js'
import protoLoader from '@grpc/proto-loader'
import protobuf from 'protobufjs'

import type { ManifestInput, PluginManifest } from './types.ts'

/**
 * `.proto` 随包下发的根目录。
 *
 * **契约的原文就在这个目录里**（`proto/hub/v1/*.proto`），与 `crates/hub-proto/proto/hub/v1/`
 * 逐字相同——`test/proto_parity.test.ts` 会在中台仓库里跑一条逐字节比对的用例。
 * 开发者不需要装 protoc：加载是运行时的，读的就是这几份源文件。
 */
export const PROTO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', 'proto')

/**
 * 本 SDK 要加载的文件。
 *
 * 只列**入口**：`envelope.proto` 由 `plugin.proto` / `gateway.proto` 等传递引用进来，
 * 不需要单独列。`google/protobuf/*.proto` 是 well-known 类型——它们不在任一
 * 中台 proto 的 import 里能被顺带加载到（我们只在 type_url 层面提到 `Struct`），
 * 所以必须显式列出来，否则 `google.protobuf.Struct` 的编解码器根本不存在。
 *
 * 自带的这份 WKT 是为了**离线**：内网机器上未必装着 protoc 的 include 目录，
 * 而 proto-loader 找不到 import 时会直接抛错。
 */
export const PROTO_FILES = [
  'hub/v1/plugin.proto',
  'hub/v1/registry.proto',
  'hub/v1/state.proto',
  'hub/v1/gateway.proto',
  'google/protobuf/struct.proto',
]

/**
 * 状态凭证的 metadata 键，必须与中台侧一致
 * （`crates/hub-grpc/src/state.rs` 的 `STATE_TOKEN_METADATA`）。
 *
 * 导出它而不是让各处各写一份字面量：写错的话中台会判成「无凭证」，而插件侧只会
 * 看到一个 401，很难查。mock 中台与契约测试用的都是这一个。
 */
export const STATE_TOKEN_METADATA = 'x-hub-state-token'

/** 「直接调用」载荷的类型标识。 */
export const STRUCT_TYPE_URL = 'type.googleapis.com/google.protobuf.Struct'

/**
 * 上面那个载荷的全限定消息名。
 *
 * 在 manifest 里声明 `consumes: [{ fqName: STRUCT_FQ_NAME }]` 表示「本插件接受
 * 直接调用的 JSON 载荷」。它是 well-known 类型，中台不要求它出现在插件自己的
 * descriptor 里。
 */
export const STRUCT_FQ_NAME = 'google.protobuf.Struct'

/** 加载选项。 */
const LOADER_OPTIONS: protoLoader.Options = {
  // camelCase 字段名（proto 里是 snake_case），与手写的 types.ts 对齐
  keepCase: false,
  // int64 → number。本契约里的 int64 只有 deadline_ms / ttl_seconds 这种量级
  longs: Number,
  // 枚举 → 字符串名。用数字的话，日志里会是一串没人认得的整数
  enums: String,
  // 补齐缺省值：没设的标量是零值、没设的消息是 null，读的时候不必到处 ?.
  defaults: true,
  // 把 oneof 的判别字段也带上（目前契约里没有 oneof，但 google.protobuf.Value 有）
  oneofs: true,
}

let packageDefinition: protoLoader.PackageDefinition | undefined
let hubNamespace: HubNamespace | undefined
let structType: protobuf.Type | undefined

/** `grpc.loadPackageDefinition` 出来的东西的形状（只写我们用到的部分）。 */
interface HubNamespace {
  hub: {
    v1: {
      PluginRuntime: grpc.ServiceClientConstructor
      PluginRegistry: grpc.ServiceClientConstructor
      HubState: grpc.ServiceClientConstructor
      PluginGateway: grpc.ServiceClientConstructor
    }
  }
}

/** 加载 `.proto`，返回 proto-loader 的包定义。进程内只做一次。 */
export function packageDefinitionOf(): protoLoader.PackageDefinition {
  packageDefinition ??= protoLoader.loadSync(PROTO_FILES, {
    ...LOADER_OPTIONS,
    includeDirs: [PROTO_ROOT],
  })
  return packageDefinition
}

/** 加载出来的服务构造器。 */
export function hubServices(): HubNamespace {
  hubNamespace ??= grpc.loadPackageDefinition(packageDefinitionOf()) as unknown as HubNamespace
  return hubNamespace
}

/**
 * `google.protobuf.Struct` 的编解码器。
 *
 * 用 protobufjs 而不是 grpc 那套消息类：Struct ↔ 普通 JS 对象之间要做一次
 * 结构转换（`{ fields: { text: { stringValue: 'x' } } }` → `{ text: 'x' }`），
 * 而那个转换的入口是 protobufjs 的 `toObject`。
 */
export function structCodec(): protobuf.Type {
  if (!structType) {
    const root = new protobuf.Root()
    // 默认的解析是「相对于引用文件所在目录」，而这里是「相对于 PROTO_ROOT」——
    // 不覆盖它的话，`hub/v1/envelope.proto` 里的 import 会被拼成
    // `hub/v1/hub/v1/envelope.proto` 而找不到
    root.resolvePath = (_origin: string, target: string) => path.join(PROTO_ROOT, target)
    root.loadSync(PROTO_FILES)
    structType = root.lookupType(STRUCT_FQ_NAME)
  }
  return structType
}

/**
 * 把插件作者写的 manifest 补齐成完整的 `hub.v1.PluginManifest` 形状。
 *
 * `defaults: true` 只在**解码**时补默认值，手写的对象不会经过它，所以插件只写
 * `name` / `version` / `consumes` 时，其余字段得由这里补——否则模板与自检里会到处
 * 出现 `manifest.tools ?? []` 这种噪音。
 *
 * 工具声明的两个可选字段在这里落成显式的默认值而不是留 `undefined`：中台侧拿到的
 * 是编码后的报文，缺字段与零值在线上没有区别，但在**本地**（自检、日志）有区别。
 */
export function normalizeManifest(input: ManifestInput): PluginManifest {
  return {
    name: input.name,
    version: input.version,
    description: input.description ?? '',
    owner: input.owner ?? '',
    consumes: input.consumes ?? [],
    produces: input.produces ?? [],
    // 调用授权名单：声明了它，中台才允许本插件调用名单里的目标（见 gateway.ts）
    invokes: input.invokes ?? [],
    tools: (input.tools ?? []).map((tool) => ({
      name: tool.name,
      description: tool.description,
      // 缺省给一个合法的空对象 schema，而不是空串——空串不是 JSON，
      // 中台侧的 MCP 工具聚合会在解析它时炸
      inputSchemaJson: tool.inputSchemaJson ?? '{"type":"object","properties":{}}',
      requiresApproval: tool.requiresApproval ?? false,
    })),
    validation: input.validation ?? null,
  }
}
