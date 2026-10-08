import { randomBytes } from 'node:crypto'

import { STRUCT_FQ_NAME, STRUCT_TYPE_URL, structCodec } from './proto.ts'
import type { Any, Envelope, Severity, ValidateResponse, ValidationIssue } from './types.ts'

/**
 * 判断是否属于 protobuf 平台提供的 well-known 类型。
 *
 * 中台对 `google.protobuf.*` 豁免「声明必须出现在自己的 descriptor 里」这条检查；
 * 插件侧的自检套件用同一个判断，避免两边规则漂移。
 */
export function isWellKnownFQName(fqName: string): boolean {
  return fqName.startsWith('google.protobuf.')
}

/**
 * 取出信封里的 JSON 载荷。
 *
 * 载荷不是 Struct（例如 flow 内部传的业务类型）时返回 `[undefined, false]`，
 * 此时插件应改为按自己的业务类型去解码。
 *
 * ⚠️ 数值一律是 JS number——`google.protobuf.Struct` 只有一种数值类型（double）。
 * 大单号这类超出 2^53 的整数请用字符串承载，别指望 JSON 数字。
 */
export function payloadJSON(env: Envelope | null | undefined): [JsonObject | undefined, boolean] {
  const payload = env?.payload
  if (!payload || anyTypeUrl(payload) !== STRUCT_TYPE_URL) return [undefined, false]

  const decoded = structCodec().decode(payload.value)
  const asObject = structCodec().toObject(decoded, { oneofs: true }) as {
    fields?: Record<string, ValueObject>
  }

  const out: JsonObject = {}
  for (const [key, value] of Object.entries(asObject.fields ?? {})) {
    out[key] = valueToJS(value)
  }
  return [out, true]
}

/** JSON 对象载荷。 */
export type JsonObject = Record<string, unknown>

/**
 * 打包一个 `google.protobuf.Any`。
 *
 * ⚠️ **两种拼写都写**，这不是偷懒，是被 protobufjs 逼的：它对
 * `google/protobuf/*.proto` 用的是自己内置的那份 JSON 定义
 * （`node_modules/protobufjs/src/common.js`），而那份定义里 `Any` 的字段名是
 * **snake_case 的 `type_url`**（`Struct` / `Value` 却是 camelCase）——于是
 * proto-loader 的 `keepCase: false` 在 Any 上根本不生效。
 *
 * 只写 `typeUrl` 的后果是**静默的**：序列化会把 `type_url` 落成空串，中台收到一个
 * 「没有类型的载荷」，而报错里完全看不出是拼写问题。两个都写，无论对面按哪种拼写取
 * 都拿得到；读的时候走 {@link anyTypeUrl}。
 */
export function packAny(typeUrl: string, value: Uint8Array): Any {
  return { typeUrl, type_url: typeUrl, value } as unknown as Any
}

/** 读 Any 的 type_url，两种拼写都认。 */
export function anyTypeUrl(any: Any | null | undefined): string {
  if (!any) return ''
  const raw = any as unknown as { typeUrl?: string; type_url?: string }
  return raw.type_url || raw.typeUrl || ''
}

/**
 * 把准备写到线上的信封里的 Any 规整一遍。
 *
 * 插件作者手写载荷（而不是用 {@link withPayloadJSON}）时只可能写 `typeUrl`，
 * 那个字段在序列化时会被整个丢掉。这里兜住它——中台侧看到「载荷没有类型」时，
 * 排查方向会被引到契约上，而真正的原因是 Node 侧的一个字段名。
 */
export function normalizeAnyForWire(env: Envelope | null): Envelope | null {
  if (!env?.payload) return env
  const out = cloneEnvelope(env)
  out.payload = packAny(anyTypeUrl(env.payload), env.payload.value)
  return out
}

/**
 * 把 JSON 对象装进信封的载荷。
 *
 * 返回**新信封**，原信封不被修改——链路里可能有别的持有者。
 */
export function withPayloadJSON(env: Envelope, payload: JsonObject): Envelope {
  const fields: Record<string, ValueObject> = {}
  for (const [key, value] of Object.entries(payload)) {
    fields[key] = jsToValue(value)
  }

  const bytes = structCodec().encode({ fields }).finish()

  const out = cloneEnvelope(env)
  out.payload = packAny(STRUCT_TYPE_URL, bytes)
  return out
}

/** 空信封。探针与测试里用得多。 */
export function emptyEnvelope(): Envelope {
  return {
    messageId: '',
    traceId: '',
    spanId: '',
    flowId: '',
    runId: '',
    nodeId: '',
    tenant: '',
    subject: null,
    deadlineMs: 0,
    type: 'PAYLOAD_TYPE_UNSPECIFIED',
    payload: null,
    payloadRef: null,
    meta: {},
  }
}

/**
 * 信封的绝对截止时间（毫秒）。未设置时返回 undefined。
 *
 * deadline 逐跳递减：插件应据此提前放弃，而不是把时间耗光后让上层的超时兜底。
 */
export function deadlineOf(env: Envelope): number | undefined {
  return env.deadlineMs > 0 ? env.deadlineMs : undefined
}

/** 距离截止时间还剩多久（毫秒）。未设置时 ok=false；已过期时返回 0。 */
export function budgetOf(env: Envelope): [number, boolean] {
  const deadline = deadlineOf(env)
  if (deadline === undefined) return [0, false]
  return [Math.max(0, deadline - Date.now()), true]
}

/** 信封是否已过截止时间。 */
export function expired(env: Envelope): boolean {
  const [budget, ok] = budgetOf(env)
  return ok && budget === 0
}

/** 构造「校验通过」的响应。 */
export function valid(): ValidateResponse {
  return { valid: true, issues: [] }
}

/**
 * 构造「校验不通过」的响应。
 *
 * 每个 issue 的 path 要能定位到具体字段（例如 `payload.items[2].sku`），中台会原样
 * 把它回给调用方，agent 靠它改数据重试。
 */
export function invalid(...issues: ValidationIssue[]): ValidateResponse {
  return { valid: false, issues }
}

/** 构造一条错误级校验问题。 */
export function issue(path: string, message: string): ValidationIssue {
  return { path, message, severity: 'SEVERITY_ERROR' }
}

/**
 * 构造一条警告级校验问题。
 *
 * 警告不会让校验失败——用它标记「能放行但值得记一笔」的情况。
 */
export function warn(path: string, message: string): ValidationIssue {
  return { path, message, severity: 'SEVERITY_WARNING' }
}

/** 警告级问题的 severity 取值，供插件自己构造 issue 时用。 */
export const SEVERITY_WARNING: Severity = 'SEVERITY_WARNING'

/** `google.protobuf.Struct` 声明成「本插件接受直接调用的 JSON 载荷」。 */
export function structContract(description = '直接调用的 JSON 载荷'): {
  fqName: string
  description: string
} {
  return { fqName: STRUCT_FQ_NAME, description }
}

/**
 * 复制一份信封。
 *
 * 手写而不是 `structuredClone`：后者会把 Buffer 拷成 Uint8Array、遇到
 * `undefined` 之类的边角行为也不好预期，而这里只需要「顶层字段 + 几个嵌套结构
 * 不共享引用」这一件事。
 */
export function cloneEnvelope(env: Envelope): Envelope {
  return {
    ...env,
    subject: env.subject ? { ...env.subject, scopes: [...env.subject.scopes] } : null,
    payload: env.payload ? { ...env.payload } : null,
    payloadRef: env.payloadRef ? { ...env.payloadRef } : null,
    meta: { ...env.meta },
  }
}

// ---------------------------------------------------------------- id 生成
//
// 与 Go / Rust / Python 各门同源：message_id 与 trace_id 都用 ULID。
// message_id 同时是总线的幂等键（at-least-once 下去重靠它），trace_id 串起一次
// 调用经过的所有插件——两者都要「同一毫秒内也不重复」。

/** Crockford Base32 字母表（ULID 标准，I/L/O 这类易混字符被剔除）。 */
const ULID_ENCODING = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'

/**
 * 生成一个 ULID（48 位毫秒时间戳 + 80 位随机数，26 字符）。
 *
 * 随机位来自 `crypto.randomBytes`：Date.now() 只有毫秒分辨率，同一毫秒内的两次
 * 生成必须靠随机位区分。熵源坏了 `randomBytes` 会直接抛错——生成的 id 可能撞车、
 * 幂等去重可能误伤，带着这种 id 上路比立刻失败更糟。
 */
export function newULID(): string {
  const raw = Buffer.alloc(16)
  // 时间戳占高 48 位（大端），低 80 位交给随机数
  raw.writeUIntBE(Date.now(), 0, 6)
  randomBytes(10).copy(raw, 6)

  // 128 位按 5 位一切编成 26 个字符：128 = 25×5 + 3，最高字符会余出 2 个零位，
  // 从低位往回拼正好把它空出来
  let n = BigInt(`0x${raw.toString('hex')}`)
  let out = ''
  for (let i = 0; i < 26; i++) {
    out = ULID_ENCODING[Number(n & 0x1fn)] + out
    n >>= 5n
  }
  return out
}

/**
 * 构造一个带全新 message_id / trace_id 的空信封。
 *
 * 自己发起一条数据流（Publish、或手动装配 Invoke 的信封）时用它起步，
 * 载荷再用 {@link withPayloadJSON} 装。type 留空不替调用方决定语义：
 * 触发路径各自知道这次携带的是请求还是事件。
 */
export function newEnvelope(): Envelope {
  return { ...emptyEnvelope(), messageId: newULID(), traceId: newULID() }
}

// ---------------------------------------------------------------- Struct ↔ JS
//
// 这一层转换是本 SDK 自己写的，没走 protobufjs 的 toObject：后者给的是
// `{ fields: { text: { stringValue: 'x' } } }` 这种「描述 protobuf 的形状」，
// 而插件作者要的是他写进 payload 的那个 JS 对象。

interface ValueObject {
  kind?: string
  nullValue?: number
  numberValue?: number
  stringValue?: string
  boolValue?: boolean
  structValue?: { fields?: Record<string, ValueObject> }
  listValue?: { values?: ValueObject[] }
}

/** `google.protobuf.Value` → JS 值。 */
function valueToJS(value: ValueObject): unknown {
  // 用 oneof 的判别字段而不是「猜哪个字段非空」：判别字段是 protobuf 的语义，
  // 而「非空」在 `stringValue: ''` 这种合法取值上会给出错误答案
  switch (value.kind) {
    case 'nullValue':
      return null
    case 'numberValue':
      return value.numberValue ?? 0
    case 'stringValue':
      return value.stringValue ?? ''
    case 'boolValue':
      return value.boolValue ?? false
    case 'structValue': {
      const out: JsonObject = {}
      for (const [key, nested] of Object.entries(value.structValue?.fields ?? {})) {
        out[key] = valueToJS(nested)
      }
      return out
    }
    case 'listValue':
      return (value.listValue?.values ?? []).map(valueToJS)
    default:
      // 没有判别字段：proto3 里 Value 一定恰好落在一个分支上，走到这里说明
      // 载荷是别的实现写出来的。返回 null 而不是抛错——校验器先于插件体跑，
      // 让业务规则去拒绝它，比在解载荷这一层炸掉更符合「校验器是规则出口」的设计
      return null
  }
}

/** JS 值 → `google.protobuf.Value`。 */
function jsToValue(value: unknown): ValueObject {
  if (value === null || value === undefined) return { nullValue: 0 }
  switch (typeof value) {
    case 'number':
      // NaN / Infinity 在 Struct 里表达不了（JSON 就没有这两个值）。
      // 落成 null 而不是抛错，与 JSON.stringify 的行为一致。
      return Number.isFinite(value) ? { numberValue: value } : { nullValue: 0 }
    case 'string':
      return { stringValue: value }
    case 'boolean':
      return { boolValue: value }
    case 'object': {
      if (Array.isArray(value)) {
        return { listValue: { values: value.map(jsToValue) } }
      }
      const fields: Record<string, ValueObject> = {}
      for (const [key, nested] of Object.entries(value as JsonObject)) {
        fields[key] = jsToValue(nested)
      }
      return { structValue: { fields } }
    }
    default:
      // bigint / symbol / function 都不是 JSON 能表达的东西
      return { nullValue: 0 }
  }
}
