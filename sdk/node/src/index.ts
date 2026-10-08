// plugin-hub 插件 SDK 的入口。
//
// 用 TypeScript 写 plugin-hub 插件的工具箱。**五件套**：
//
//   | 件 | 位置 | 作用 |
//   |---|---|---|
//   | ① 服务端骨架 | 本文件 | 实现两个方法就有个能跑的插件：gRPC 服务、自注册、心跳、被摘除后自愈、优雅退出全由它处理 |
//   | ② 契约定义 | `proto/hub/v1` | 与 `crates/hub-proto` 逐字相同的 `.proto`，**不需要装 protoc** |
//   | ③ 一致性套件 | `@example/hubkit/conformance` | 插件接入前必须跑通的检查，跑的就是中台注册期的规则 |
//   | ④ mock 中台 | `@example/hubkit/mockhub` | 本地 mock，离线开发与自动化测试用 |
//   | ⑤ 脚手架 | `src/cli.ts` | `new <name>` 生成可运行的插件工程 |
//
// 最小插件：
//
//	import { configFromEnv, run, valid, invalid, issue, payloadJSON, withPayloadJSON } from '@example/hubkit'
//
//	const plugin = {
//	  manifest: () => ({ name: 'order-reader', version: '0.1.0' }),
//	  descriptor: () => new Uint8Array(0),
//	  validate: (_ctx, env) => ('orderId' in (payloadJSON(env)[0] ?? {}) ? valid() : invalid(issue('payload.orderId', '缺少'))),
//	  handle: (_ctx, env) => withPayloadJSON(env, { ok: true }),
//	}
//
//	await run(plugin, configFromEnv())

export {
  DEFAULT_LISTEN_ADDR,
  DEFAULT_STATE_CALL_TIMEOUT_MS,
  HEARTBEAT_FALLBACK_INTERVAL_MS,
  REGISTER_RETRY_INTERVAL_MS,
  ConfigError,
  configFromEnv,
  defaultInstanceId,
  validateConfig,
  withDefaults,
} from './config.ts'
export type { Config, ResolvedConfig } from './config.ts'

export { DescriptorError, descriptorMessages, emptyDescriptor } from './descriptor.ts'

export {
  SEVERITY_WARNING,
  anyTypeUrl,
  budgetOf,
  cloneEnvelope,
  deadlineOf,
  emptyEnvelope,
  expired,
  invalid,
  isWellKnownFQName,
  issue,
  normalizeAnyForWire,
  newEnvelope,
  newULID,
  packAny,
  payloadJSON,
  structContract,
  valid,
  warn,
  withPayloadJSON,
} from './envelope.ts'
export type { JsonObject } from './envelope.ts'

export {
  CALL_CHAIN_META,
  GatewayClient,
  InvokeFailed,
  InvokeRejected,
  MAX_INVOKE_DEPTH,
  newPluginGatewayClient,
} from './gateway.ts'
export type { InvokeOptions, PluginGatewayClient } from './gateway.ts'

export { LEVELS, Logger, formatTime, levelFromEnv } from './log.ts'
export type { Attrs, Level } from './log.ts'

export { isGatewayAware, isStateAware } from './plugin.ts'
export type { CallContext, GatewayAware, Plugin, StateAware } from './plugin.ts'

export {
  PROTO_FILES,
  PROTO_ROOT,
  STATE_TOKEN_METADATA,
  STRUCT_FQ_NAME,
  STRUCT_TYPE_URL,
  hubServices,
  normalizeManifest,
  packageDefinitionOf,
  structCodec,
} from './proto.ts'

export { validPluginName, validStateSegment } from './rules.ts'

export {
  RegistrationRejected,
  checkManifest,
  hubCredentials,
  hubTarget,
  normalizeListenAddr,
  rejectCodeName,
  run,
} from './run.ts'
export type { RunOptions } from './run.ts'

export { PublishRejected, StateClient, newHubStateClient } from './state.ts'
export type { HubStateClient, StateCallOptions, StateEntry, Unary } from './state.ts'

export type * from './types.ts'
