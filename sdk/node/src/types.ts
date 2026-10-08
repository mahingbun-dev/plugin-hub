// 契约的 TypeScript 形状。
//
// 这些接口是**手写**的，不是从 proto 生成的——本 SDK 走的是
// `@grpc/proto-loader` 的运行时加载（选型理由见 README），代价就是没有静态类型。
// 手写一份的意义在于：插件作者拿到的是 `env.payload.typeUrl` 这样的补全，而不是
// `any`。字段名与 proto-loader 的 `keepCase: false` 一致（snake_case → camelCase），
// 枚举是**字符串**（`enums: String`），int64 是 `number`（`longs: Number`）。
//
// 字段全部非可选且可空：加载器开了 `defaults: true`，没设的标量落成零值、
// 没设的消息落成 `null`，所以读的时候不需要到处 `?.`。
//
// ⚠️ 大整数：`longs: Number` 把 int64 落成 JS number，安全范围是 2^53。本契约里
// 的 int64 只有 deadline_ms / ttl_seconds 这类秒毫秒量级的值，都在范围内。

/**
 * `google.protobuf.Any`：契约标识 = typeUrl 的最后一段。
 *
 * ⚠️ **别直接读写 `typeUrl`**：在线上（以及从 gRPC 收到的对象里）它叫 **`type_url`**——
 * protobufjs 对 `google/protobuf/*.proto` 用它内置的那份 JSON 定义，而那份定义里
 * Any 的字段名是 snake_case 的（`Struct` / `Value` 反而是 camelCase）。
 * 打包用 `packAny`、读取用 `anyTypeUrl`，写载荷用 `withPayloadJSON`。
 */
export interface Any {
  typeUrl: string
  value: Uint8Array
}

/** 调用主体：谁触发了这次数据流。 */
export interface Subject {
  kind: SubjectKind
  id: string
  tenant: string
  scopes: string[]
  origin: string
}

export type SubjectKind =
  | 'SUBJECT_KIND_UNSPECIFIED'
  | 'SUBJECT_KIND_HUMAN'
  | 'SUBJECT_KIND_AGENT'
  | 'SUBJECT_KIND_PLUGIN'
  | 'SUBJECT_KIND_SYSTEM'

/** 载荷在数据流中的语义。 */
export type PayloadType =
  | 'PAYLOAD_TYPE_UNSPECIFIED'
  | 'PAYLOAD_TYPE_REQUEST'
  | 'PAYLOAD_TYPE_EVENT'
  | 'PAYLOAD_TYPE_COMMAND'
  | 'PAYLOAD_TYPE_RESULT'

/** 超限载荷的引用（载荷改用引用传递时 payload 为空、本项填写）。 */
export interface PayloadRef {
  uri: string
  sha256: string
  sizeBytes: number
  contentType: string
}

/**
 * 统一信封：所有在插件之间、以及插件与中台之间流动的数据都是它。
 *
 * 中台强制这个形状，插件不感知 HTTP / MQ / cron 等触发来源的差异。
 */
export interface Envelope {
  /** ULID，同时作为幂等键来源（总线是 at-least-once，去重靠它）。 */
  messageId: string
  /** W3C Trace Context。 */
  traceId: string
  spanId: string
  flowId: string
  runId: string
  nodeId: string
  tenant: string
  subject: Subject | null
  /** 绝对 deadline（Unix 毫秒），逐跳递减。 */
  deadlineMs: number
  type: PayloadType
  payload: Any | null
  payloadRef: PayloadRef | null
  meta: Record<string, string>
}

/** 声明的一条消息契约。 */
export interface MessageContract {
  /** 全限定消息名，例如 `wms.v1.OrderCreated` —— 契约的唯一标识。 */
  fqName: string
  description: string
}

/** 暴露给 agent 的一项能力。中台聚合时名称加 `插件名__` 前缀。 */
export interface ToolDecl {
  name: string
  description: string
  /** MCP 工具的入参 JSON Schema（字符串形式）。 */
  inputSchemaJson: string
  /** 是否属于「改变生产走向」的操作。为 true 时 agent 只能提交草稿，需人工审批后生效。 */
  requiresApproval: boolean
}

/** 插件作者写工具声明时的形状：只有 name 与 description 必填。 */
export type ToolDeclInput = Pick<ToolDecl, 'name' | 'description'> & Partial<Omit<ToolDecl, 'name' | 'description'>>

export type FailureAction =
  | 'FAILURE_ACTION_UNSPECIFIED'
  | 'FAILURE_ACTION_REJECT'
  | 'FAILURE_ACTION_SHADOW'

export interface ValidationPolicy {
  onFailure: FailureAction
}

/** 插件自述清单。中台据此做契约校验、编排校验与 MCP 工具聚合。 */
export interface PluginManifest {
  /** 插件名（逻辑插件），多版本共存时同名不同 version。 */
  name: string
  /** 语义化版本。flow 通过版本约束（如 ^2.1）锁定实例。 */
  version: string
  description: string
  /** 负责团队，用于治理与告警归口。 */
  owner: string
  /** 消费的消息类型：上游 produces 必须与这里的 fq_name 完全一致才能连成一条 flow。 */
  consumes: MessageContract[]
  produces: MessageContract[]
  /**
   * 本插件声明可调用的目标插件名。
   *
   * 中台的调用策略为 `declared` 时，只有名单内的目标放行；`allow`（缺省）时不看它。
   * 名单写在 manifest 里随版本注册，审计与授权因此有据可查。
   */
  invokes: string[]
  tools: ToolDecl[]
  validation: ValidationPolicy | null
}

/**
 * 插件作者写 manifest 时用的形状：`name` 与 `version` 必填，其余可省。
 *
 * 骨架在 `run()` 里用 {@link normalizeManifest} 补成完整的 {@link PluginManifest}——
 * 模板里因此不必堆一串 `?? []`。
 */
export type ManifestInput = Pick<PluginManifest, 'name' | 'version'> &
  Omit<Partial<PluginManifest>, 'tools'> & { tools?: ToolDeclInput[] }

export type Severity = 'SEVERITY_UNSPECIFIED' | 'SEVERITY_ERROR' | 'SEVERITY_WARNING'
/** 一条校验问题。 */
export interface ValidationIssue {
  /** 字段路径，例如 `payload.items[2].sku`。 */
  path: string
  message: string
  severity: Severity
}

export interface ValidateResponse {
  valid: boolean
  issues: ValidationIssue[]
}

export interface DescribeRequest {}

export interface ValidateRequest {
  envelope: Envelope | null
}

export interface HandleRequest {
  envelope: Envelope | null
}

export interface HandleResponse {
  /** 输出信封。中台会校验其 payload 的全限定名是否落在 manifest.produces 内。 */
  envelope: Envelope | null
}

export interface HealthResponse {
  healthy: boolean
  message: string
}

/** 中台的拒绝码。字符串形式，前缀 `REJECT_CODE_`。 */
export type RejectCode =
  | 'REJECT_CODE_UNSPECIFIED'
  | 'REJECT_CODE_UNREACHABLE'
  | 'REJECT_CODE_DESCRIPTOR_INVALID'
  | 'REJECT_CODE_BREAKING_CHANGE'
  | 'REJECT_CODE_TOOL_CONFLICT'
  | 'REJECT_CODE_VERSION_CONFLICT'
  | 'REJECT_CODE_MANIFEST_INVALID'
  | 'REJECT_CODE_INTERNAL'
  | 'REJECT_CODE_INSTANCE_CONFLICT'

/** 一条拒绝原因。 */
export interface Rejection {
  code: RejectCode
  message: string
  /** 具体细节，例如「字段 wms.v1.Order.sku 由 string 改为 int64」。 */
  detail: string
}

export interface RegisterRequest {
  pluginName: string
  version: string
  instanceId: string
  advertiseAddr: string
  manifest: PluginManifest | null
  /** `google.protobuf.FileDescriptorSet` 的序列化字节。 */
  descriptorSet: Uint8Array
}

export interface RegisterResponse {
  accepted: boolean
  instanceId: string
  /** 中台指定的心跳周期，插件应遵守。 */
  heartbeatIntervalSeconds: number
  rejections: Rejection[]
  warnings: string[]
  /**
   * 状态凭证。每次注册轮换；空值表示该实例不可用 HubState。
   *
   * 它同时是**注销时的属主凭证**：中台侧 HubState 的认证与注销的属主判定查的是
   * 同一列（`plugin_instances.state_token`），所以骨架只存一份（见
   * {@link UnregisterRequest.stateToken}）。
   */
  stateToken: string
}

export interface HeartbeatRequest {
  instanceId: string
}

export interface HeartbeatResponse {
  accepted: boolean
  heartbeatIntervalSeconds: number
  /** 中台可要求插件重新注册（例如契约基线变更后、或实例已被摘除）。 */
  reregisterRequired: boolean
}

export interface UnregisterRequest {
  instanceId: string
  reason: string
  /**
   * 注册时中台下发的状态凭证（`RegisterResponse.stateToken`）。
   *
   * **注销要凭它认出「你是这一行的主人」。** `instanceId` 由插件自己生成、不同插件
   * 之间可以撞，而注销只按 `instanceId` 删行会让先退出的一方删掉**对方**那一行——
   * 对方的心跳仍按 `instanceId` 命中、返回 accepted，完全察觉不到自己从注册表里
   * 消失了。留空一律被中台拒绝且不删任何行。
   */
  stateToken: string
}

export interface UnregisterResponse {}

/** HubState 的键。字符集与长度约束见 {@link validStateSegment}。 */
export interface KvKey {
  namespace: string
  key: string
}

export interface KvGetRequest {
  key: KvKey | null
}
export interface KvGetResponse {
  found: boolean
  value: Uint8Array
}
export interface KvPutRequest {
  key: KvKey | null
  value: Uint8Array
  /** 0 表示不过期。 */
  ttlSeconds: number
}
export interface KvPutResponse {}
export interface KvDeleteRequest {
  key: KvKey | null
}
export interface KvDeleteResponse {
  deleted: boolean
}
export interface KvScanRequest {
  namespace: string
  prefix: string
  limit: number
}
export interface KvScanResponse {
  entries: KvEntry[]
}
export interface KvEntry {
  key: string
  value: Uint8Array
}

export interface PublishRequest {
  /** 目标 flow 名或订阅 topic。 */
  target: string
  /** 待投递的信封。subject 由中台按调用方身份覆盖，自报无效。 */
  envelope: Envelope | null
}

export interface PublishResponse {
  accepted: boolean
  /** 受理后产生的执行 id，供后续查链路。 */
  runId: string
  /** 被拒绝时的原因（例如检测到环）。 */
  reason: string
}

// ---------------------------------------------------------------- 插件间发现与互调
//
// 对应 hub/v1/gateway.proto。业务结果（拒绝/未授权/超限/成环…）走响应字段
// （outcome + reason/issues），只有基础设施故障才走 gRPC 错误——所以
// InvokeResponse 是普通返回值而不是异常，映射成异常是 GatewayClient 的事。

export interface ListPluginsRequest {
  /** 缺省只列在线（有健康实例）的插件；置 true 时附上离线插件。 */
  includeOffline: boolean
}

export interface PluginSummary {
  name: string
  /** 已注册的最新版本。多版本共存时在线的不一定是它。 */
  latestVersion: string
  online: boolean
  /** 当前健康实例数。 */
  instanceCount: number
  description: string
}

export interface ListPluginsResponse {
  plugins: PluginSummary[]
}

export interface DescribeMessageRequest {
  /** 全限定消息名，与 manifest 契约里的 fqName 同口径（如 `wms.v1.OrderCreated`）。 */
  fqName: string
}

export interface MessageEndpoint {
  /** 生产 / 消费该消息的插件及其版本。 */
  plugin: string
  version: string
}

export interface DescribeMessageResponse {
  /** 生产者来自 manifest.produces，消费者来自 manifest.consumes。 */
  producers: MessageEndpoint[]
  consumers: MessageEndpoint[]
}

export interface GetContractRequest {
  plugin: string
  /** 可空：空 = 取最新已注册版本。 */
  version: string
  /** 可空：空 = 不展开单个消息的字段级 schema（只要整份契约）。 */
  fqName: string
}

export interface GetContractResponse {
  name: string
  version: string
  produces: MessageContract[]
  consumes: MessageContract[]
  /** 本插件声明可调用的目标插件名（语义同 PluginManifest.invokes）。 */
  invokes: string[]
  tools: ToolDecl[]
  /** fq_name 非空时：该消息的字段级 schema（JSON 字符串）；否则为空。 */
  schemaJson: string
}

export interface InvokeRequest {
  /** 目标插件名。 */
  plugin: string
  /** 可空 = 目标最新版本。 */
  version: string
  /** 调用方构造的信封；subject 由中台无条件覆盖为调用方。 */
  envelope: Envelope | null
  /** 本次调用的超时预算（毫秒）；实际 deadline 取 min(传入 deadline, now+timeout_ms)。 */
  timeoutMs: number
}

/** 互调的业务结果分类。基础设施故障不走这里——那会直接抛 gRPC 错误。 */
export type InvokeOutcome = 'INVOKE_OUTCOME_UNSPECIFIED' | 'HANDLED' | 'REJECTED' | 'ERROR'

export interface InvokeResponse {
  outcome: InvokeOutcome
  /** 仅 HANDLED 时有值：下游 Handle 返回的信封。 */
  envelope: Envelope | null
  /** 仅 REJECTED 时有值：目标插件 Validate 的 issues。 */
  issues: ValidationIssue[]
  /** ERROR / REJECTED 的人类可读原因。 */
  reason: string
  /** 中台侧整备 + 下游处理的总耗时。 */
  elapsedMs: number
}
