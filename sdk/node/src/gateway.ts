import * as grpc from '@grpc/grpc-js'

import { DEFAULT_STATE_CALL_TIMEOUT_MS } from './config.ts'
import { newEnvelope, newULID, packAny, withPayloadJSON } from './envelope.ts'
import { STATE_TOKEN_METADATA, hubServices } from './proto.ts'
import type { Unary } from './state.ts'
import type { JsonObject } from './envelope.ts'
import type {
  DescribeMessageRequest,
  DescribeMessageResponse,
  Envelope,
  GetContractRequest,
  GetContractResponse,
  InvokeRequest,
  InvokeResponse,
  ListPluginsRequest,
  ListPluginsResponse,
  ValidationIssue,
} from './types.ts'

/**
 * 互调链的 meta 键：链上是「已处理过该消息的插件名」，逗号分隔。
 *
 * 信封的 meta 是唯一的自由携带通道，链只能随它走。与中台
 * `crates/hub-grpc/src/gateway.rs` 的 `CALL_CHAIN_META` 同名。
 *
 * 通过 {@link GatewayClient.invokePlugin} 发起调用时**不要**自己往链里追加自己：
 * 链的语义由中台维护，SDK 只负责把调用方收到的链原样带下去。
 */
export const CALL_CHAIN_META = 'hub.call_chain'

/**
 * 互调链的长度上限（含本次 caller）。
 *
 * SDK 侧不主动校验它——链的校验在中台（超了会以 outcome=ERROR 拒绝）——
 * 导出它只为 mock 中台与插件自测能对齐同一个数。
 */
export const MAX_INVOKE_DEPTH = 8

/** `hub.v1.PluginGateway` 的客户端。 */
export interface PluginGatewayClient extends grpc.Client {
  ListPlugins: Unary<ListPluginsRequest, ListPluginsResponse>
  DescribeMessage: Unary<DescribeMessageRequest, DescribeMessageResponse>
  GetContract: Unary<GetContractRequest, GetContractResponse>
  Invoke: Unary<InvokeRequest, InvokeResponse>
}

/** 由地址构造一个 PluginGateway 客户端。骨架自己用；插件作者不该自己构造它。 */
export function newPluginGatewayClient(
  address: string,
  credentials: grpc.ChannelCredentials,
): PluginGatewayClient {
  const Ctor = hubServices().hub.v1.PluginGateway as unknown as new (
    address: string,
    credentials: grpc.ChannelCredentials,
  ) => PluginGatewayClient
  return new Ctor(address, credentials)
}

/** 目标插件的校验器拒绝了这次调用（outcome=REJECTED）。 */
export class InvokeRejected extends Error {
  /** 中台给的人类可读原因。 */
  readonly reason: string
  /**
   * 结构化的校验问题，`path` 能定位到具体字段——调用方照着改数据即可，
   * 不必去翻下游日志。
   */
  readonly issues: ValidationIssue[]

  constructor(reason: string, issues: ValidationIssue[]) {
    super(reason ? `互调被目标插件拒绝：${reason}` : '互调被目标插件拒绝（未给出原因）')
    this.name = 'InvokeRejected'
    this.reason = reason
    this.issues = issues
  }
}

/** 这次互调被中台判为失败（outcome=ERROR）。 */
export class InvokeFailed extends Error {
  readonly reason: string

  constructor(reason: string) {
    super(reason ? `互调失败：${reason}` : '互调失败（未给出原因）')
    this.name = 'InvokeFailed'
    this.reason = reason
  }
}

/** {@link GatewayClient.invokePlugin} 的入参。 */
export interface InvokeOptions {
  /** 目标插件的版本，空 = 最新已注册版本。 */
  readonly version?: string

  /**
   * 本次调用的超时预算（毫秒），不填由中台给默认预算兜底。
   *
   * 它会同时落到两处：信封的 deadline（与 currentEnvelope 的 deadline 取较早者，
   * 中台侧还会再夹一次）与请求的 timeout_ms。
   */
  readonly timeoutMs?: number

  /**
   * 插件当前正在处理的信封（可空）。
   *
   * 传入它，trace 才能从上游贯通到下游：其 trace_id / run_id / node_id 会被
   * 复制进新信封，meta 里的调用链（{@link CALL_CHAIN_META}）原样带上——**不追加
   * 自己**，中台负责把 caller 记进链。不传则生成全新 trace_id、不带链：那是一次
   * 顶层发起的调用，不是当前处理的延续。
   */
  readonly currentEnvelope?: Envelope | null

  /** JSON 对象载荷（直接调用场景），与 {@link payload} 二选一。 */
  readonly payloadJSON?: JsonObject

  /**
   * 业务 proto 消息载荷（flow 内部传递语义）：全限定类型 + 已序列化的字节。
   * 与 {@link payloadJSON} 二选一。
   */
  readonly payload?: { readonly typeUrl: string; readonly value: Uint8Array }
}

/**
 * 插件间发现与互调（PluginGateway 服务）的客户端。
 *
 * 与 {@link StateClient} 同款：由 `run()` 在注册成功后注入给实现了 GatewayAware
 * 的插件，凭证只有中台知道、且随每次重新注册轮换；调用撞上 UNAUTHENTICATED 时
 * 通过 `onDenied` 叫醒注册循环去换新凭证。
 *
 * 与 StateClient 的一点刻意差别：**Invoke 不吃 callTimeout**。状态调用是毫秒级的
 * 管理面操作，卡 2 秒就该放弃；互调是业务调用，预算就是信封的 deadline——下游
 * 真的要处理 20 秒时，客户端先超时等于让下游白干。所以发现三件套用 callTimeout
 * 截断，Invoke / invokePlugin 的超时完全由信封预算决定。
 */
export class GatewayClient {
  private token = ''
  private readonly client: PluginGatewayClient
  private readonly onDenied: () => void
  private readonly callTimeoutMs: number

  // 构造器参数属性是不可擦除的 TS 语法，本 SDK 靠类型剥离直接跑 .ts，所以显式赋值
  constructor(
    client: PluginGatewayClient,
    /**
     * 凭证被中台拒绝时的回调。注册循环据此重走注册流程换新凭证。
     *
     * 与 StateClient 共用同一个回调：凭证是同一份，中台重启时两面一起失效、一起换。
     */
    onDenied: () => void = () => {},
    /** **发现类**调用的时间上限，<= 0 表示不设上限。对 Invoke 不生效。 */
    callTimeoutMs: number = DEFAULT_STATE_CALL_TIMEOUT_MS,
  ) {
    this.client = client
    this.onDenied = onDenied
    this.callTimeoutMs = callTimeoutMs
  }

  /** 更新凭证。由注册循环在每次注册成功后调用。 */
  setToken(token: string): void {
    this.token = token
  }

  /** 当前凭证，供测试断言。 */
  currentToken(): string {
    return this.token
  }

  /** 返回插件清单。includeOffline=false 只列在线（有健康实例）的。 */
  async listPlugins(includeOffline = false): Promise<ListPluginsResponse> {
    // 插件用它回答「能调谁」，不要旁路维护硬编码名单——那会跟注册表漂移
    return await this.discoveryCall<ListPluginsRequest, ListPluginsResponse>(
      (client, req, md, options, cb) => client.ListPlugins(req, md, options, cb),
      { includeOffline },
    )
  }

  /** 查一个消息类型由谁生产、由谁消费。 */
  async describeMessage(fqName: string): Promise<DescribeMessageResponse> {
    return await this.discoveryCall<DescribeMessageRequest, DescribeMessageResponse>(
      (client, req, md, options, cb) => client.DescribeMessage(req, md, options, cb),
      { fqName },
    )
  }

  /**
   * 查一个插件的契约。
   *
   * version 传空取最新已注册版本；fqName 传空不展开单个消息的字段级 schema。
   */
  async getContract(plugin: string, version = '', fqName = ''): Promise<GetContractResponse> {
    return await this.discoveryCall<GetContractRequest, GetContractResponse>(
      (client, req, md, options, cb) => client.GetContract(req, md, options, cb),
      { plugin, version, fqName },
    )
  }

  /**
   * 发起一次同步互调，返回中台的原始应答。
   *
   * 业务结果（HANDLED/REJECTED/ERROR）全在应答字段里，本方法不做映射——
   * 要「HANDLED 给信封、其余给类型化异常」的便捷语义请用 {@link invokePlugin}。
   */
  async invoke(req: InvokeRequest): Promise<InvokeResponse> {
    const options: grpc.CallOptions = {}
    // 信封预算就是本次 gRPC 调用的预算：到点客户端先放弃，而不是等中台或下游超时
    if (req.envelope && req.envelope.deadlineMs > 0) options.deadline = req.envelope.deadlineMs
    return await this.rawCall((md, opts, cb) => this.client.Invoke(req, md, opts, cb), options)
  }

  /**
   * 发起一次同步互调的便捷入口：装配信封（幂等键、trace 上下文、调用链、deadline）、
   * 打包载荷、调用中台、把业务结果映射成类型化异常。
   *
   * 结果语义：
   *   - HANDLED → 返回下游的信封（载荷用 `payloadJSON` 取）；
   *   - REJECTED → {@link InvokeRejected}（reason + issues）；
   *   - ERROR → {@link InvokeFailed}（reason）；
   *   - 基础设施故障（未鉴权、中台不可达）→ 原样的 gRPC 错误。
   */
  async invokePlugin(plugin: string, opts: InvokeOptions = {}): Promise<Envelope> {
    if (!plugin.trim()) throw new Error('hubkit: invokePlugin 缺少目标插件名')
    if (opts.payloadJSON !== undefined && opts.payload !== undefined) {
      throw new Error('hubkit: invokePlugin 的 payloadJSON 与 payload 只能二选一')
    }
    if (opts.timeoutMs !== undefined && opts.timeoutMs < 0) {
      throw new Error('hubkit: invokePlugin 的 timeoutMs 不能为负')
    }

    const env = newEnvelope()
    // message_id 是幂等键，每次调用都要是新的；type 对互调语义固定为 REQUEST
    env.type = 'PAYLOAD_TYPE_REQUEST'

    let deadlineMs = 0
    const cur = opts.currentEnvelope
    if (cur) {
      env.traceId = cur.traceId
      env.runId = cur.runId
      env.nodeId = cur.nodeId
      // 链原样复制，不追加自己：链上记的是「已处理过该消息的插件」，
      // 追加 caller 是中台的事，SDK 抢着做会让链上出现重复节点
      const chain = cur.meta[CALL_CHAIN_META]
      if (chain) env.meta = { [CALL_CHAIN_META]: chain }
      // 整体预算比本次预算更早时以它为准——调用不该活得比触发它的那次处理更久
      deadlineMs = cur.deadlineMs
    }
    if (!env.traceId) env.traceId = newULID()
    if (opts.timeoutMs !== undefined && opts.timeoutMs > 0) {
      // 中台会取 min(传入 deadline, now+timeout)，客户端先夹一次能更早放弃
      const ceiling = Date.now() + opts.timeoutMs
      if (deadlineMs <= 0 || ceiling < deadlineMs) deadlineMs = ceiling
    }
    env.deadlineMs = deadlineMs

    if (opts.payloadJSON !== undefined) {
      return await this.send(plugin, opts, withPayloadJSON(env, opts.payloadJSON))
    }
    if (opts.payload !== undefined) {
      return await this.send(plugin, opts, {
        ...env,
        payload: packAny(opts.payload.typeUrl, opts.payload.value),
      })
    }
    return await this.send(plugin, opts, env)
  }

  /** 装配请求、发出调用并把业务结果映射成返回值或类型化异常。 */
  private async send(plugin: string, opts: InvokeOptions, env: Envelope): Promise<Envelope> {
    const resp = await this.invoke({
      plugin,
      version: opts.version ?? '',
      envelope: env,
      timeoutMs: opts.timeoutMs ?? 0,
    })

    switch (resp.outcome) {
      case 'HANDLED':
        if (!resp.envelope) throw new Error('hubkit: 中台报告 HANDLED 但未返回信封')
        return resp.envelope
      case 'REJECTED':
        throw new InvokeRejected(resp.reason, resp.issues ?? [])
      case 'ERROR':
        throw new InvokeFailed(resp.reason)
      default:
        throw new Error(`hubkit: 中台返回了未知的结果分类 ${resp.outcome}`)
    }
  }

  /** 发现类调用：带凭证与发现类时间上限。 */
  private discoveryCall<Req, Res>(
    invoke: (
      client: PluginGatewayClient,
      req: Req,
      md: grpc.Metadata,
      options: grpc.CallOptions,
      cb: (err: grpc.ServiceError | null, response: Res) => void,
    ) => grpc.ClientUnaryCall,
    request: Req,
  ): Promise<Res> {
    const options: grpc.CallOptions = {}
    if (this.callTimeoutMs > 0) options.deadline = Date.now() + this.callTimeoutMs
    return this.rawCall((md, opts, cb) => invoke(this.client, request, md, opts, cb), options)
  }

  /**
   * 发一次调用，带上凭证；额外 options（如 Invoke 的信封预算）由调用方给。
   *
   * 超时产生的是 `DEADLINE_EXCEEDED` 而不是 `UNAUTHENTICATED`，所以不会触发
   * `onDenied`——中台的一次卡顿不该把注册循环叫醒去重注册。
   */
  private rawCall<Res>(
    invoke: (
      md: grpc.Metadata,
      options: grpc.CallOptions,
      cb: (err: grpc.ServiceError | null, response: Res) => void,
    ) => grpc.ClientUnaryCall,
    options: grpc.CallOptions = {},
  ): Promise<Res> {
    const metadata = new grpc.Metadata()
    // 取当前凭证的快照：await 期间它可能被重新注册换掉，而这一次调用该用的
    // 是发出时的那个
    metadata.set(STATE_TOKEN_METADATA, this.token)

    return new Promise<Res>((resolve, reject) => {
      invoke(metadata, options, (err, response) => {
        if (err) {
          if (err.code === grpc.status.UNAUTHENTICATED) this.onDenied()
          reject(err)
          return
        }
        resolve(response)
      })
    })
  }
}
