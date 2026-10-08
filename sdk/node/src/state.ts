import * as grpc from '@grpc/grpc-js'

import { DEFAULT_STATE_CALL_TIMEOUT_MS } from './config.ts'
import { STATE_TOKEN_METADATA, hubServices } from './proto.ts'
import type { Envelope } from './types.ts'
import type {
  KvDeleteRequest,
  KvDeleteResponse,
  KvEntry,
  KvGetRequest,
  KvGetResponse,
  KvPutRequest,
  KvPutResponse,
  KvScanRequest,
  KvScanResponse,
  PublishRequest,
  PublishResponse,
} from './types.ts'

/** 扫描返回的一项。 */
export interface StateEntry {
  key: string
  value: Uint8Array
}

/** 一次状态调用的可选参数。 */
export interface StateCallOptions {
  /**
   * 调用方自己的截止预算（毫秒）。与 {@link StateClient} 的时间上限取**更早**的那个。
   *
   * 为什么不直接收一个 AbortSignal：grpc-js 的取消是 deadline/信号两条路，而
   * 「还剩多少预算」正是信封里那个绝对 deadline 能直接给出的东西。
   */
  readonly budgetMs?: number
}

/**
 * grpc-js 经 proto-loader 加载出来的客户端方法都是这个形状。
 *
 * 导出而不是各文件各写一份：网关客户端（`gateway.ts`）的方法一模一样。
 */
export type Unary<Req, Res> = (
  request: Req,
  metadata: grpc.Metadata,
  options: grpc.CallOptions,
  callback: (err: grpc.ServiceError | null, response: Res) => void,
) => grpc.ClientUnaryCall

/** `hub.v1.HubState` 的客户端。 */
export interface HubStateClient extends grpc.Client {
  KvGet: Unary<KvGetRequest, KvGetResponse>
  KvPut: Unary<KvPutRequest, KvPutResponse>
  KvDelete: Unary<KvDeleteRequest, KvDeleteResponse>
  KvScan: Unary<KvScanRequest, KvScanResponse>
  Publish: Unary<PublishRequest, PublishResponse>
}

/** 由地址构造一个 HubState 客户端。骨架自己用；插件作者不该自己构造它。 */
export function newHubStateClient(address: string, credentials: grpc.ChannelCredentials): HubStateClient {
  const Ctor = hubServices().hub.v1.HubState as unknown as new (
    address: string,
    credentials: grpc.ChannelCredentials,
  ) => HubStateClient
  return new Ctor(address, credentials)
}

/**
 * 中台没有受理一次 Publish（accepted=false）。
 *
 * 这是**业务结果**而不是网络故障：防环、超限、目标 flow 不存在都属于此类，
 * `reason` 会说明原因。调用方该据此改逻辑或换目标，而不是退避重试——重试是
 * 给「中台/总线坏了」（gRPC 错误）准备的，两者被中台刻意分在两个通道里。
 */
export class PublishRejected extends Error {
  readonly reason: string

  constructor(reason: string) {
    super(reason ? `中台未受理 Publish：${reason}` : '中台未受理 Publish（未给出原因）')
    this.name = 'PublishRejected'
    this.reason = reason
  }
}

/**
 * 中台外置状态（HubState）的客户端。
 *
 * 插件被**强制无状态**——实例内存不保证跨调用保留，因此热切换零代价、水平扩展无障碍、
 * 重放与测试都简单。所有需要跨调用保留的东西都走这里。
 *
 * **键前缀不含版本号**：中台把键拼成 `hub:state:{插件名}:{namespace}:{key}`，同一插件的
 * **所有版本共用一个状态空间**。升版本不会清空状态（对登录缓存这类状态正是要的），
 * 但两个版本往同一个 namespace 写就是**互相覆盖**。要按版本隔离，请自己把版本写进
 * namespace。
 *
 * 凭证会被重新注册轮换（中台重启、实例被摘除后自愈），而注册循环跑在另一条异步链上，
 * 所以凭证的读写都要过锁——Node 是单线程，一个赋值就是原子的，但 `setToken` 与
 * `withToken` 之间仍可能被 await 打断，所以取当前值时读的必须是同一次快照。
 */
export class StateClient {
  private token = ''
  private readonly client: HubStateClient
  private readonly onDenied: () => void
  private readonly callTimeoutMs: number

  // 构造器参数属性是不可擦除的 TS 语法，本 SDK 靠类型剥离直接跑 .ts，所以显式赋值
  constructor(
    client: HubStateClient,
    /**
     * 凭证被中台拒绝时的回调。注册循环据此重走注册流程换新凭证。
     *
     * 只认 UNAUTHENTICATED：其它错误（网络抖动、参数非法）与凭证无关，重注册解决不了。
     */
    onDenied: () => void = () => {},
    /** 单次调用的时间上限，<= 0 表示不设上限。 */
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

  /**
   * 当前凭证。
   *
   * 读它的有两处：测试断言，以及骨架在注销时取属主凭证（见 `Registrar.unregister`）
   * ——中台侧 HubState 的认证与注销的属主判定查的是同一列，所以这里是唯一的存放点。
   */
  currentToken(): string {
    return this.token
  }

  /** 读取一个键。found 区分「键不存在」与「值是空字节」。 */
  async get(
    namespace: string,
    key: string,
    opts: StateCallOptions = {},
  ): Promise<[Uint8Array | undefined, boolean]> {
    const resp = await this.call<KvGetRequest, KvGetResponse>(
      (client, req, md, options, cb) => client.KvGet(req, md, options, cb),
      { key: { namespace, key } },
      opts,
    )
    return [resp.found ? resp.value : undefined, resp.found]
  }

  /**
   * 写入一个键。`ttlMs` 为 0 表示不过期。
   *
   * ⚠️ TTL 的粒度是**秒**（线协议是 `ttl_seconds`），不足 1 秒的 ttl 会被截断成 0，
   * 也就是**永不过期**。想让键很快消失，请传 >= 1000ms 的值。
   */
  async put(
    namespace: string,
    key: string,
    value: Uint8Array,
    ttlMs = 0,
    opts: StateCallOptions = {},
  ): Promise<void> {
    await this.call<KvPutRequest, KvPutResponse>(
      (client, req, md, options, cb) => client.KvPut(req, md, options, cb),
      { key: { namespace, key }, value, ttlSeconds: Math.floor(ttlMs / 1000) },
      opts,
    )
  }

  /** 删除一个键。删不存在的键返回 `false`，不是错误。 */
  async delete(namespace: string, key: string, opts: StateCallOptions = {}): Promise<boolean> {
    const resp = await this.call<KvDeleteRequest, KvDeleteResponse>(
      (client, req, md, options, cb) => client.KvDelete(req, md, options, cb),
      { key: { namespace, key } },
      opts,
    )
    return resp.deleted
  }

  /** 扫描一个命名空间下前缀匹配的键，上限 limit（服务端另有硬上限）。 */
  async scan(
    namespace: string,
    prefix: string,
    limit: number,
    opts: StateCallOptions = {},
  ): Promise<StateEntry[]> {
    const resp = await this.call<KvScanRequest, KvScanResponse>(
      (client, req, md, options, cb) => client.KvScan(req, md, options, cb),
      { namespace, prefix, limit },
      opts,
    )
    return resp.entries.map((e: KvEntry) => ({ key: e.key, value: e.value }))
  }

  /**
   * 把一条信封投递到目标 flow / topic，异步触发、拿不到业务结果。
   *
   * 这是插件间**异步**协作的通道；需要下游处理结果的同步场景走
   * {@link GatewayClient.invokePlugin}。信封用 {@link newEnvelope} 起步、载荷用
   * {@link withPayloadJSON} 装；subject 由中台覆盖为调用方身份，自己填了也没用。
   *
   * 受理成功返回中台分配的 run_id（本次触发的 flow 执行标识）；未受理抛
   * {@link PublishRejected}，reason 随异常携带。
   */
  async publish(target: string, env: Envelope, opts: StateCallOptions = {}): Promise<string> {
    const resp = await this.call<PublishRequest, PublishResponse>(
      (client, req, md, options, cb) => client.Publish(req, md, options, cb),
      { target, envelope: env },
      opts,
    )
    if (!resp.accepted) throw new PublishRejected(resp.reason)
    return resp.runId
  }

  /**
   * 发一次调用，带上凭证与时间上限。
   *
   * 超时产生的是 `DEADLINE_EXCEEDED` 而不是 `UNAUTHENTICATED`，所以不会触发
   * `onDenied`——中台的一次卡顿不该把注册循环叫醒去重注册（那会连带重新探测
   * 插件自己的地址）。
   */
  private call<Req, Res>(
    invoke: (
      client: HubStateClient,
      req: Req,
      md: grpc.Metadata,
      options: grpc.CallOptions,
      cb: (err: grpc.ServiceError | null, response: Res) => void,
    ) => grpc.ClientUnaryCall,
    request: Req,
    opts: StateCallOptions,
  ): Promise<Res> {
    const metadata = new grpc.Metadata()
    // 取当前凭证的快照：await 期间它可能被重新注册换掉，而这一次调用该用的
    // 是发出时的那个
    metadata.set(STATE_TOKEN_METADATA, this.token)

    const options: grpc.CallOptions = {}
    const deadline = this.deadlineFor(opts)
    if (deadline !== undefined) options.deadline = deadline

    return new Promise<Res>((resolve, reject) => {
      invoke(this.client, request, metadata, options, (err, response) => {
        if (err) {
          this.noteDenied(err)
          reject(err)
          return
        }
        resolve(response)
      })
    })
  }

  /** 时间上限取「插件配的上限」与「调用方剩余预算」里更早的那个。 */
  private deadlineFor(opts: StateCallOptions): number | undefined {
    const candidates: number[] = []
    if (this.callTimeoutMs > 0) candidates.push(Date.now() + this.callTimeoutMs)
    if (opts.budgetMs !== undefined && Number.isFinite(opts.budgetMs)) {
      candidates.push(Date.now() + opts.budgetMs)
    }
    return candidates.length > 0 ? Math.min(...candidates) : undefined
  }

  private noteDenied(err: grpc.ServiceError): void {
    if (err.code === grpc.status.UNAUTHENTICATED) this.onDenied()
  }
}
