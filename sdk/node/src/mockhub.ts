import * as grpc from '@grpc/grpc-js'
import net from 'node:net'

import { CALL_CHAIN_META, MAX_INVOKE_DEPTH } from './gateway.ts'
import { cloneEnvelope, newULID } from './envelope.ts'
import { validStateSegment } from './rules.ts'
import { STATE_TOKEN_METADATA, hubServices } from './proto.ts'
import type { Unary } from './state.ts'
import type {
  DescribeMessageRequest,
  DescribeMessageResponse,
  Envelope,
  GetContractRequest,
  GetContractResponse,
  HandleRequest,
  HandleResponse,
  HeartbeatRequest,
  HeartbeatResponse,
  InvokeRequest,
  InvokeResponse,
  KvDeleteRequest,
  KvDeleteResponse,
  KvGetRequest,
  KvGetResponse,
  KvEntry,
  KvPutRequest,
  KvPutResponse,
  KvScanRequest,
  KvScanResponse,
  ListPluginsRequest,
  ListPluginsResponse,
  PluginManifest,
  PublishRequest,
  PublishResponse,
  RegisterRequest,
  RegisterResponse,
  Rejection,
  UnregisterRequest,
  UnregisterResponse,
  ValidateRequest,
  ValidateResponse,
} from './types.ts'

/**
 * 本地 mock 中台：给插件开发者的「L3 替身」。
 *
 * 用途有二：
 *
 *   - **离线开发**：插件团队不必等中台就绪，也不必连测试环境就能把插件跑起来。
 *   - **自动化测试**：断言「注册请求里到底报了什么」「心跳有没有按时发」「被摘除后
 *     有没有重新注册」这类行为。
 *
 * ⚠️ 它与**真中台**能证明的东西不同：不落库、不做心跳超时摘除、契约校验是简化版。
 * 中台仓库里那个 Rust 版 mock（`crates/hub-mock`）才是与真中台**同源判据**的那个
 * （它的注册校验直接调 `hub-registry` 里真中台也在调的函数），本文件是它在本语言内的
 * 等价物，用于 SDK 自身的单测——**不能拿它代替真中台或 Rust 版 mock 做接入验收**。
 *
 * 一条纪律：**mock 不该比真中台宽松**。离线 mock 一旦比真环境宽容，问题就会推迟到
 * 上线才爆。所以状态面的认证、键字符集、上限、TTL 都照中台来。
 */
export interface MockHubOptions {
  /** 中台指定的心跳周期（秒），缺省 1（测试里要快）。 */
  heartbeatIntervalSeconds?: number

  /**
   * 非空时用它来拒绝注册。
   *
   * 用来验证插件侧对拒绝的处理：拒绝原因有没有打全、会不会一直重试。
   */
  rejectRegister?: (req: RegisterRequest) => Rejection[]

  /**
   * 大于 0 时，收到这么多拍心跳后开始要求插件重新注册。
   *
   * 用来验证「实例被摘除后插件能自愈」。
   */
  reregisterAfterBeats?: number

  /**
   * 为 true 时，状态面的每一次调用都以 UNAUTHENTICATED 拒绝，凭证本身照常下发
   * （模拟「凭证被中台吊销/轮换后旧凭证还在插件手里」）。
   */
  denyStateToken?: boolean

  /** 状态凭证，缺省 `mock-state-token`。 */
  stateToken?: string
}

/** mock 里固定的插件名。真中台是拿凭证查库反查出插件名的，mock 不做那件事。 */
const MOCK_STATE_PLUGIN = 'mock-plugin'

/** 与中台一致：HubState 不是对象存储。 */
const MAX_STATE_VALUE_BYTES = 1024 * 1024

/** 与中台一致：防止一次拉走整个命名空间。 */
const MAX_STATE_SCAN_LIMIT = 1000

/** 与中台一致：crates/hub-grpc/src/gateway.rs 的 INVOKE_QUOTA_PER_MINUTE。 */
const INVOKE_QUOTA_PER_MINUTE = 60

/** 与中台一致：timeout_ms=0 是「没填」而不是「不等结果」。 */
const DEFAULT_INVOKE_TIMEOUT_MS = 30_000

interface InstanceState {
  plugin: string
  version: string
  advertiseAddr: string
  lastHeartbeatAt: number
}

/** 插件最新一次注册的登记项。实例全被摘掉后仍保留（离线 ≠ 没注册过）。 */
interface PluginRegistration {
  version: string
  manifest: PluginManifest | null
  advertiseAddr: string
}

/** 指向某个插件的运行时客户端，互调的替身用它去调 Validate / Handle。 */
interface PluginRuntimeClient extends grpc.Client {
  Validate: Unary<ValidateRequest, ValidateResponse>
  Handle: Unary<HandleRequest, HandleResponse>
}

export class MockHub {
  /** 收到的全部注册请求（含被拒的），按时间顺序。 */
  readonly registrations: RegisterRequest[] = []

  /** 收到的被拒注册的拒绝原因，按时间顺序。 */
  readonly rejections: Rejection[] = []

  /** 收到的注销请求里的实例 id，按时间顺序（**含被拒的**）。 */
  readonly unregisters: string[] = []

  /**
   * 收到的注销请求里携带的状态凭证，与 {@link unregisters} **同序**。
   *
   * `instanceId` 是插件自报的、可以撞，凭证才是属主证明——两者分开记是因为
   * 「注销带没带凭证」与「注销的是哪一个 id」是两条独立的事。
   */
  readonly unregisterTokens: string[] = []

  /**
   * 被拒的注销请求里的实例 id，按时间顺序。
   *
   * 两种情形会进来：凭证为空或不符（中台认不出属主），以及压根没有这一行。
   * 两者都**不删任何行**——真中台也是如此。
   */
  readonly refusedUnregisters: string[] = []

  /** 收到的心跳拍数。 */
  heartbeats = 0

  private readonly server: grpc.Server
  private readonly instances = new Map<string, InstanceState>()
  /** 插件名 → 最新一次注册。真中台的「最新版本 / 契约 / 实例数」查询都建立在它上面。 */
  private readonly plugins = new Map<string, PluginRegistration>()
  /** 互调配额的固定窗口：窗口 id（epoch 分钟）→ 本窗口内各 caller 的调用次数。 */
  private invokeWindow = 0
  private invokeCounts = new Map<string, number>()
  private readonly stateKV = new Map<string, Uint8Array>()
  private readonly stateExpiry = new Map<string, number>()
  private readonly opts: MockHubOptions & { heartbeatIntervalSeconds: number }
  /**
   * 实际绑上的 `host:port`。
   *
   * **不能直接用传入的监听地址**：传 `127.0.0.1:0` 时系统会分配一个真实端口，而
   * `:0` 本身不是能拨的目标——照抄它会让插件一直连一个不存在的端口，报出来的还是
   * `EADDRNOTAVAIL` 这种看不出所以然的错。
   */
  private address: string

  constructor(opts: MockHubOptions & { heartbeatIntervalSeconds: number }, listenAddr: string) {
    this.opts = opts
    this.address = listenAddr
    this.server = new grpc.Server()
    const services = hubServices().hub.v1
    this.server.addService(services.PluginRegistry.service, this.registryHandlers() as never)
    this.server.addService(services.HubState.service, this.stateHandlers() as never)
    this.server.addService(services.PluginGateway.service, this.gatewayHandlers() as never)
  }

  /** 起一个 mock 中台，监听在回环的随机端口上。 */
  static async start(opts: MockHubOptions = {}): Promise<MockHub> {
    return await MockHub.startOn('127.0.0.1:0', opts)
  }

  /**
   * 起一个 mock 中台，监听在指定地址上。
   *
   * 给「中台比插件晚起来」这类用例用：插件先连的那个端口必须**先存在再消失**，
   * 才能证明它是靠重试接上的，而不是碰巧一次就连上了。
   */
  static async startOn(address: string, opts: MockHubOptions = {}): Promise<MockHub> {
    const server = new MockHub(
      {
        ...opts,
        heartbeatIntervalSeconds: opts.heartbeatIntervalSeconds ?? 1,
        stateToken: opts.stateToken ?? 'mock-state-token',
      },
      address,
    )
    await server.bind()
    return server
  }

  /** 插件应当连接的中台地址（含 `http://` 前缀）。 */
  get addr(): string {
    return `http://${this.address}`
  }

  /**
   * grpc-js 的 target（`host:port`，不带协议头）。
   *
   * 与 {@link addr} 分开是因为两者的用途不同：`addr` 是配给 `HUB_ADDR` 的（协议头
   * 决定了走不走 TLS），而 grpc-js 的 `bindAsync` 只认 `host:port`——把带协议头的
   * 串丢给它会当成 DNS 名去解析，报出来的是「Name resolution failed for target
   * dns:http://...」这种一眼看不出所以然的错。
   */
  get target(): string {
    return this.address
  }

  /** 当前下发的状态凭证，供测试断言。 */
  get stateToken(): string {
    return this.opts.stateToken ?? 'mock-state-token'
  }

  /** 最近一次注册请求；没有则返回 undefined。 */
  get lastRegistration(): RegisterRequest | undefined {
    return this.registrations.at(-1)
  }

  /** 在册的实例快照。 */
  instancesSnapshot(): { instanceId: string; plugin: string; advertiseAddr: string }[] {
    return [...this.instances.entries()].map(([instanceId, inst]) => ({
      instanceId,
      plugin: inst.plugin,
      advertiseAddr: inst.advertiseAddr,
    }))
  }

  /**
   * 摘掉一个实例，模拟中台的心跳超时巡检，或中台侧把实例删掉。
   *
   * 真中台有巡检任务（本 mock 不做），这是它在测试里的替身：摘掉之后插件再发心跳就会
   * 收到 `reregisterRequired`，于是重走注册流程——**这就是「被摘除后自愈」**，
   * 也是这条链路里最值得测的一个分支。
   */
  forgetInstance(instanceId: string): void {
    this.instances.delete(instanceId)
  }

  /** 状态存储的快照，供测试断言。键是 `命名空间\x00键`。 */
  stateSnapshot(): Map<string, Uint8Array> {
    this.purgeExpired()
    return new Map(this.stateKV)
  }

  /** 等到至少收到 n 次注册，或超时。 */
  async waitForRegistration(n: number, timeoutMs = 5_000): Promise<void> {
    await this.waitUntil(() => this.registrations.length >= n, n, '注册', timeoutMs)
  }

  /** 等到至少收到 n 拍心跳，或超时。 */
  async waitForHeartbeats(n: number, timeoutMs = 5_000): Promise<void> {
    await this.waitUntil(() => this.heartbeats >= n, n, '心跳', timeoutMs)
  }

  /** 等到某个实例重新出现在册上，或超时。 */
  async waitUntilInstanceBack(instanceId: string, timeoutMs = 5_000): Promise<void> {
    await this.waitUntil(
      () => this.instances.has(instanceId),
      1,
      `实例 ${instanceId} 重新注册`,
      timeoutMs,
    )
  }

  /** 停掉 mock 中台。 */
  async close(): Promise<void> {
    await new Promise<void>((resolve) => this.server.tryShutdown(() => resolve()))
    this.server.forceShutdown()
  }

  // ---------------------------------------------------------------- 服务端实现

  private registryHandlers(): Record<string, unknown> {
    return {
      register: (
        call: grpc.ServerUnaryCall<RegisterRequest, RegisterResponse>,
        callback: grpc.sendUnaryData<RegisterResponse>,
      ) => {
        const req = call.request
        this.registrations.push(req)

        // 与真中台一致：拒绝走结构化 rejections，**不用 gRPC 错误码**。
        // 插件侧只需要一套处理逻辑，且拒绝原因要能原样展示给人看。
        const rejections = this.opts.rejectRegister?.(req) ?? []
        if (rejections.length > 0) {
          this.rejections.push(...rejections)
          callback(null, {
            accepted: false,
            instanceId: '',
            heartbeatIntervalSeconds: this.opts.heartbeatIntervalSeconds,
            rejections,
            warnings: [],
            stateToken: '',
          })
          return
        }

        this.instances.set(req.instanceId, {
          plugin: req.pluginName,
          version: req.version,
          advertiseAddr: req.advertiseAddr,
          lastHeartbeatAt: Date.now(),
        })
        // 与真中台的注册表同语义：同名重复注册视为进程重启，刷新版本与地址；
        // 实例全被摘掉后登记仍在（ListPlugins 还要靠它报「离线插件」）
        this.plugins.set(req.pluginName, {
          version: req.version,
          manifest: req.manifest ?? null,
          advertiseAddr: req.advertiseAddr,
        })

        callback(null, {
          accepted: true,
          instanceId: req.instanceId,
          heartbeatIntervalSeconds: this.opts.heartbeatIntervalSeconds,
          rejections: [],
          warnings: [],
          stateToken: this.stateToken,
        })
      },

      heartbeat: (
        call: grpc.ServerUnaryCall<HeartbeatRequest, HeartbeatResponse>,
        callback: grpc.sendUnaryData<HeartbeatResponse>,
      ) => {
        this.heartbeats += 1
        const requireReregister =
          (this.opts.reregisterAfterBeats ?? 0) > 0 &&
          this.heartbeats > (this.opts.reregisterAfterBeats ?? 0)

        // 与真中台一致：认不出来就要求重新注册，而不是回一个 gRPC 错误
        const known = this.instances.has(call.request.instanceId)
        const instance = this.instances.get(call.request.instanceId)
        if (instance) instance.lastHeartbeatAt = Date.now()

        callback(null, {
          accepted: known && !requireReregister,
          heartbeatIntervalSeconds: this.opts.heartbeatIntervalSeconds,
          reregisterRequired: !known || requireReregister,
        })
      },

      unregister: (
        call: grpc.ServerUnaryCall<UnregisterRequest, UnregisterResponse>,
        callback: grpc.sendUnaryData<UnregisterResponse>,
      ) => {
        const req = call.request
        this.unregisters.push(req.instanceId)
        this.unregisterTokens.push(req.stateToken)

        // 与真中台一致：凭证是「你是这一行的主人」的证明，判定与删除是同一步。
        //
        // 空凭证或不符一律**拒绝并拒绝删任何行**，但不回 gRPC 错误——真中台也是返回
        // 空响应，拒绝只体现在「那一行还在」上（理由是打进中台日志的）。
        //
        // 这道校验必须有，否则 mock 会比真中台宽松：一个只按 instanceId 删行的骨架
        // 在它下面全绿，上线后却会删掉撞了 id 的**别人**那一行，而对方毫无察觉
        // （心跳仍按 instanceId 命中、返回 accepted）。
        if (
          !this.instances.has(req.instanceId) ||
          req.stateToken === '' ||
          req.stateToken !== this.stateToken
        ) {
          this.refusedUnregisters.push(req.instanceId)
          callback(null, {})
          return
        }

        this.instances.delete(req.instanceId)
        callback(null, {})
      },
    }
  }

  private stateHandlers(): Record<string, unknown> {
    return {
      // Publish 明确不做——与中台一致（防环与限流是独立课题，见 docs/design.md）。
      //
      // 这里**不校验凭证**：中台对 Publish 一律直接返回 Unimplemented，mock 若先查
      // 凭证，插件就会照着「不带凭证调 Publish 会先撞 401」去写，上真环境才发现不是。
      Publish: (
        _call: grpc.ServerUnaryCall<PublishRequest, PublishResponse>,
        callback: grpc.sendUnaryData<PublishResponse>,
      ) => {
        callback(
          statusError(
            grpc.status.UNIMPLEMENTED,
            'Publish 尚未实现：防环与限流是独立课题，见 docs/design.md',
          ),
        )
      },

      KvGet: (
        call: grpc.ServerUnaryCall<KvGetRequest, KvGetResponse>,
        callback: grpc.sendUnaryData<KvGetResponse>,
      ) => {
        if (!this.authenticate(call, callback)) return
        const key = this.stateKeyOf(call, callback)
        if (!key) return
        this.purgeExpired()
        const value = this.stateKV.get(stateKey(key.namespace, key.key))
        // found=false 与"键在、值是空字节"是两回事，别把它们合并
        callback(null, value ? { found: true, value } : { found: false, value: new Uint8Array(0) })
      },

      KvPut: (
        call: grpc.ServerUnaryCall<KvPutRequest, KvPutResponse>,
        callback: grpc.sendUnaryData<KvPutResponse>,
      ) => {
        if (!this.authenticate(call, callback)) return
        const key = this.stateKeyOf(call, callback)
        if (!key) return
        const value = call.request.value ?? new Uint8Array(0)
        if (value.length > MAX_STATE_VALUE_BYTES) {
          callback(this.invalidArgument<KvPutResponse>(`value 超过上限 ${MAX_STATE_VALUE_BYTES} 字节`))
          return
        }
        const ttl = call.request.ttlSeconds ?? 0
        if (ttl < 0) {
          callback(this.invalidArgument<KvPutResponse>('ttl_seconds 不能为负'))
          return
        }

        const full = stateKey(key.namespace, key.key)
        this.stateKV.set(full, value)
        // ttl=0 是"不过期"：覆盖旧值时要把残留的过期时间清掉，否则旧 TTL 会把新值带走
        if (ttl > 0) this.stateExpiry.set(full, Date.now() + ttl * 1000)
        else this.stateExpiry.delete(full)

        callback(null, {})
      },

      KvDelete: (
        call: grpc.ServerUnaryCall<KvDeleteRequest, KvDeleteResponse>,
        callback: grpc.sendUnaryData<KvDeleteResponse>,
      ) => {
        if (!this.authenticate(call, callback)) return
        const key = this.stateKeyOf(call, callback)
        if (!key) return
        const full = stateKey(key.namespace, key.key)
        const existed = this.stateKV.delete(full)
        this.stateExpiry.delete(full)
        // 删不存在的键返回 deleted=false，不是错误：调用方的意图（这键没了）已达成
        callback(null, { deleted: existed })
      },

      KvScan: (
        call: grpc.ServerUnaryCall<KvScanRequest, KvScanResponse>,
        callback: grpc.sendUnaryData<KvScanResponse>,
      ) => {
        if (!this.authenticate(call, callback)) return
        const { namespace, prefix, limit } = call.request
        if (!validStateSegment(namespace)) {
          callback(this.invalidArgument<KvScanResponse>('namespace 只允许 [A-Za-z0-9_.-] 且非空'))
          return
        }
        // prefix 允许为空（扫整个命名空间），非空时同样只接受白名单字符
        if (prefix && !validStateSegment(prefix)) {
          callback(this.invalidArgument<KvScanResponse>('prefix 只允许 [A-Za-z0-9_.-]'))
          return
        }
        if (!limit || limit > MAX_STATE_SCAN_LIMIT) {
          callback(this.invalidArgument<KvScanResponse>(`limit 必须在 1..=${MAX_STATE_SCAN_LIMIT} 之间`))
          return
        }

        this.purgeExpired()
        // 前缀里含插件名，匹配被限制在本插件的命名空间内——
        // 少了它，扫出来的会是**别人的**键（或者一个都扫不到，因为内层键还带着一层前缀）
        const namespacePrefix = stateKey(namespace, '')
        const pattern = `${namespacePrefix}${prefix}`
        const entries: KvEntry[] = []
        for (const [full, value] of [...this.stateKV.entries()].sort(([a], [b]) => (a < b ? -1 : 1))) {
          if (!full.startsWith(pattern)) continue
          // 对外只暴露插件自己的键名，不带前缀
          entries.push({ key: full.slice(namespacePrefix.length), value })
          if (entries.length >= limit) break
        }
        callback(null, { entries })
      },
    }
  }

  // ---------------------------------------------------------------- 网关面实现

  /**
   * 插件间发现与互调的替身。四个方法都先过与状态面同一套凭证校验；
   * Invoke 的流程（鉴权 → 配额 → 链校验 → 解析目标 → 整备信封 → Validate → Handle）
   * 与真中台 gateway.rs 同序、规则同源。业务结果一律走 outcome + reason，
   * 只有基础设施故障才回 gRPC 错误——这两条纪律 mock 同样不放宽。
   *
   * 与真中台的一处已知差异：身份反查。真中台拿凭证查 PG 得到 caller 插件名，
   * mock 里凭证持有者一律是 mock-plugin（与状态面同一取舍）。
   */
  private gatewayHandlers(): Record<string, unknown> {
    return {
      ListPlugins: (
        call: grpc.ServerUnaryCall<ListPluginsRequest, ListPluginsResponse>,
        callback: grpc.sendUnaryData<ListPluginsResponse>,
      ) => {
        if (!this.authenticate(call, callback)) return
        const plugins = [...this.plugins.keys()].sort().map((name) => {
          // mock 不做健康探测：注册过即在线。真中台的 instance_count 统计
          // 健康实例，被摘除的实例会在那里消失——这一层 mock 不追。
          const count = this.instanceCountOf(name)
          const reg = this.plugins.get(name)!
          return {
            name,
            latestVersion: reg.version,
            online: count > 0,
            instanceCount: count,
            description: reg.manifest?.description ?? '',
          }
        })
        callback(null, {
          plugins: call.request.includeOffline
            ? plugins
            : plugins.filter((p) => p.online),
        })
      },

      DescribeMessage: (
        call: grpc.ServerUnaryCall<DescribeMessageRequest, DescribeMessageResponse>,
        callback: grpc.sendUnaryData<DescribeMessageResponse>,
      ) => {
        if (!this.authenticate(call, callback)) return
        const producers: { plugin: string; version: string }[] = []
        const consumers: { plugin: string; version: string }[] = []
        for (const name of [...this.plugins.keys()].sort()) {
          const reg = this.plugins.get(name)!
          for (const c of reg.manifest?.produces ?? []) {
            if (c.fqName === call.request.fqName) producers.push({ plugin: name, version: reg.version })
          }
          for (const c of reg.manifest?.consumes ?? []) {
            if (c.fqName === call.request.fqName) consumers.push({ plugin: name, version: reg.version })
          }
        }
        callback(null, { producers, consumers })
      },

      GetContract: (
        call: grpc.ServerUnaryCall<GetContractRequest, GetContractResponse>,
        callback: grpc.sendUnaryData<GetContractResponse>,
      ) => {
        if (!this.authenticate(call, callback)) return
        const name = call.request.plugin.trim()
        if (!name) {
          callback(this.invalidArgument<GetContractResponse>('plugin 不能为空'))
          return
        }
        const reg = this.plugins.get(name)
        if (!reg) {
          callback(statusError(grpc.status.NOT_FOUND, `插件 ${name} 未注册`))
          return
        }
        if (call.request.version && reg.version !== call.request.version) {
          callback(statusError(grpc.status.NOT_FOUND, `插件 ${name} 没有版本 ${call.request.version}`))
          return
        }
        const manifest = reg.manifest
        callback(null, {
          name,
          version: reg.version,
          produces: manifest?.produces ?? [],
          consumes: manifest?.consumes ?? [],
          invokes: manifest?.invokes ?? [],
          tools: manifest?.tools ?? [],
          // schema_json 要拿注册的 descriptor 做字段级摊平，mock 不复刻那套——
          // 依赖它的行为请连真中台验证。
          schemaJson: '',
        })
      },

      Invoke: (
        call: grpc.ServerUnaryCall<InvokeRequest, InvokeResponse>,
        callback: grpc.sendUnaryData<InvokeResponse>,
      ) => this.invoke(call, callback),
    }
  }

  /** 同步互调的替身实现。流程与判定规则的说明见 {@link gatewayHandlers}。 */
  private async invoke(
    call: grpc.ServerUnaryCall<InvokeRequest, InvokeResponse>,
    callback: grpc.sendUnaryData<InvokeResponse>,
  ): Promise<void> {
    if (!this.authenticate(call, callback)) return
    const req = call.request
    if (!req.plugin) {
      callback(this.invalidArgument<InvokeResponse>('plugin 不能为空'))
      return
    }
    if (!req.envelope) {
      callback(this.invalidArgument<InvokeResponse>('缺少信封'))
      return
    }

    const started = Date.now()
    const elapsed = (): number => Date.now() - started
    // 业务结果（超限/成环/链深/解析不到目标/下游失败）一律走 outcome=ERROR，
    // 绝不悄悄变成 gRPC Status——与真中台同一分工
    const invokeError = (reason: string): InvokeResponse => ({
      outcome: 'ERROR',
      envelope: null,
      issues: [],
      reason,
      elapsedMs: elapsed(),
    })

    // ---- 配额：每插件每分钟固定窗口，与状态面的配额同一形状 ----
    const window = Math.floor(Date.now() / 60_000)
    if (window !== this.invokeWindow) {
      this.invokeWindow = window
      this.invokeCounts = new Map()
    }
    const count = (this.invokeCounts.get(MOCK_STATE_PLUGIN) ?? 0) + 1
    this.invokeCounts.set(MOCK_STATE_PLUGIN, count)
    if (count > INVOKE_QUOTA_PER_MINUTE) {
      callback(null, invokeError(`本分钟互调已达上限 ${INVOKE_QUOTA_PER_MINUTE} 次（本次是第 ${count} 次）`))
      return
    }

    // ---- 链校验：caller 已在链上就是环；深度含本次 caller ----
    const env = cloneEnvelope(req.envelope)
    const chain = callChainOf(env.meta)
    if (chain.includes(MOCK_STATE_PLUGIN)) {
      callback(
        null,
        invokeError(`检测到互调环: ${MOCK_STATE_PLUGIN} 已在调用链上（${chain.join(' → ')}）`),
      )
      return
    }
    if (chain.length + 1 > MAX_INVOKE_DEPTH) {
      callback(
        null,
        invokeError(
          `互调链已达上限（${MAX_INVOKE_DEPTH}），当前 ${chain.length} 段（${chain.join(' → ')}）`,
        ),
      )
      return
    }

    // ---- 解析目标 ----
    const reg = this.plugins.get(req.plugin)
    if (!reg) {
      callback(null, invokeError(`目标插件 ${req.plugin} 未注册`))
      return
    }
    if (req.version && reg.version !== req.version) {
      callback(null, invokeError(`目标插件 ${req.plugin} 没有版本 ${req.version}`))
      return
    }

    // ---- 整备（与真中台同一批规则）：链追加 caller、subject 覆盖为 caller、
    // deadline 夹紧、trace 补齐、type 未指定时置 REQUEST ----
    chain.push(MOCK_STATE_PLUGIN)
    env.meta = { ...env.meta, [CALL_CHAIN_META]: chain.join(',') }
    env.subject = {
      kind: 'SUBJECT_KIND_PLUGIN',
      id: MOCK_STATE_PLUGIN,
      tenant: '',
      scopes: [],
      origin: '',
    }
    env.deadlineMs = clampDeadline(env.deadlineMs, req.timeoutMs, Date.now())
    if (!env.traceId) env.traceId = newULID()
    if (env.type === 'PAYLOAD_TYPE_UNSPECIFIED') env.type = 'PAYLOAD_TYPE_REQUEST'

    // ---- 执行：Validate → Handle，与真中台的 Invoker 同序 ----
    const client = dialPlugin(reg.advertiseAddr)
    try {
      // 信封预算就是下游调用的预算：到点先放弃，而不是拖到插件自己的超时
      const options: grpc.CallOptions =
        env.deadlineMs > Date.now() ? { deadline: env.deadlineMs } : {}

      let vresp: ValidateResponse
      try {
        // 不能把 client.Validate 摘下来直接传：grpc-js 的客户端方法依赖 this，
        // 脱离对象调用会在内部炸出「checkOptionalUnaryResponseArguments」这类
        // 与业务毫无关系的错误
        vresp = await unary<ValidateRequest, ValidateResponse>(
          (req, md, opts, cb) => client.Validate(req, md, opts, cb),
          { envelope: env },
          options,
        )
      } catch (err) {
        callback(null, invokeError(`目标插件 ${req.plugin} 校验器调用失败: ${errorMessage(err)}`))
        return
      }
      if (!vresp.valid) {
        callback(null, {
          outcome: 'REJECTED',
          envelope: null,
          issues: vresp.issues ?? [],
          reason: '目标插件校验未通过，见 issues',
          elapsedMs: elapsed(),
        })
        return
      }

      let out: HandleResponse
      try {
        out = await unary<HandleRequest, HandleResponse>(
          (req, md, opts, cb) => client.Handle(req, md, opts, cb),
          { envelope: env },
          options,
        )
      } catch (err) {
        callback(null, invokeError(`目标插件 ${req.plugin} 调用失败: ${errorMessage(err)}`))
        return
      }
      if (!out.envelope) {
        callback(null, invokeError(`目标插件 ${req.plugin} 未返回信封`))
        return
      }
      callback(null, { outcome: 'HANDLED', envelope: out.envelope, issues: [], reason: '', elapsedMs: elapsed() })
    } finally {
      client.close()
    }
  }

  /** 某个插件当前在册的实例数。 */
  private instanceCountOf(plugin: string): number {
    let count = 0
    for (const inst of this.instances.values()) {
      if (inst.plugin === plugin) count += 1
    }
    return count
  }

  /**
   * 校验凭证，顺带取出 `KvKey`。
   *
   * 顺序与中台一致：**先认证、再校验其余参数**。反过来的话，"没凭证 + 参数非法"的请求
   * 会看到与真中台不同的错误码，插件侧的容错就会照着 mock 写错。
   *
   * `KvScan` 的请求里没有 `KvKey`（只有一个裸的 namespace 字符串），所以取键这一步
   * **只对有键的四个方法做**——把它放进认证里会让 KvScan 永远撞「缺少 key」。
   */
  private authenticate<Res>(
    call: grpc.ServerUnaryCall<unknown, Res>,
    callback: grpc.sendUnaryData<Res>,
  ): boolean {
    if (this.opts.denyStateToken) {
      callback(this.unauthenticated<Res>('状态凭证无效或已失效'))
      return false
    }
    const values = call.metadata.get(STATE_TOKEN_METADATA)
    const token = values[0]
    if (!token) {
      callback(this.unauthenticated<Res>(`缺少 ${STATE_TOKEN_METADATA}`))
      return false
    }
    if (token !== this.stateToken) {
      callback(this.unauthenticated<Res>('状态凭证无效或已失效'))
      return false
    }
    return true
  }

  /** 取 `KvKey` 并校验两端。缺失或非法时回错误并返回 undefined。 */
  private stateKeyOf<Res>(
    call: grpc.ServerUnaryCall<unknown, Res>,
    callback: grpc.sendUnaryData<Res>,
  ): { namespace: string; key: string } | undefined {
    const key = (call.request as { key?: { namespace: string; key: string } }).key
    if (!key) {
      callback(this.invalidArgument<Res>('缺少 key'))
      return undefined
    }
    if (!validStateSegment(key.namespace) || !validStateSegment(key.key)) {
      callback(this.invalidArgument<Res>('namespace 与 key 只允许 [A-Za-z0-9_.-] 且非空'))
      return undefined
    }
    return key
  }

  private unauthenticated<Res>(details: string): grpc.ServiceError {
    return statusError(grpc.status.UNAUTHENTICATED, details)
  }

  private invalidArgument<Res>(details: string): grpc.ServiceError {
    return statusError(grpc.status.INVALID_ARGUMENT, details)
  }

  private purgeExpired(): void {
    const now = Date.now()
    for (const [key, at] of this.stateExpiry) {
      if (at <= now) {
        this.stateKV.delete(key)
        this.stateExpiry.delete(key)
      }
    }
  }

  private async waitUntil(
    ready: () => boolean,
    target: number,
    what: string,
    timeoutMs: number,
  ): Promise<void> {
    const deadline = Date.now() + timeoutMs
    while (!ready()) {
      if (Date.now() > deadline) {
        throw new Error(`mockhub: 等待第 ${target} 次${what}超时`)
      }
      await new Promise((resolve) => setTimeout(resolve, 10))
    }
  }

  private bind(): Promise<void> {
    return new Promise((resolve, reject) => {
      this.server.bindAsync(this.address, grpc.ServerCredentials.createInsecure(), (err, port) => {
        if (err) {
          reject(err)
          return
        }
        // 把 `:0` 换成系统实际分配的端口，`addr` 才是能拨的
        this.address = `127.0.0.1:${port}`
        resolve()
      })
    })
  }
}

/** mock 内部的存储键。分隔符用 \x00：它不在白名单字符里，拼出来不会有歧义。 */
function stateKey(namespace: string, key: string): string {
  return `${MOCK_STATE_PLUGIN}\x00${namespace}\x00${key}`
}

/**
 * 解出信封 meta 里的互调链。空段必须滤掉：没设这个键时它可能是空串，
 * 空段会变成一个「叫空字符串的插件」参与环检测——脏数据不该有语义。
 * 与中台 gateway.rs 的 call_chain 同规则。
 */
function callChainOf(meta: Record<string, string>): string[] {
  return (meta[CALL_CHAIN_META] ?? '')
    .split(',')
    .map((seg) => seg.trim())
    .filter((seg) => seg !== '')
}

/**
 * 与中台 gateway.rs 的 clamp_deadline 同规则：
 * deadline 取 min(传入, now+timeout)；传入缺失（<=0）或已过期时用 now+timeout
 * 重新起算——沿用一个已过期的 deadline 会让调用瞬间超时；timeout=0 是「没填」，
 * 给 DEFAULT_INVOKE_TIMEOUT_MS 兜底。
 */
function clampDeadline(requestedMs: number, timeoutMs: number, nowMs: number): number {
  const budget = timeoutMs > 0 ? timeoutMs : DEFAULT_INVOKE_TIMEOUT_MS
  const ceiling = nowMs + budget
  if (requestedMs <= 0 || requestedMs < nowMs) return ceiling
  return Math.min(requestedMs, ceiling)
}

/** 连上一个插件的 gRPC 地址（如 http://127.0.0.1:9000）。 */
function dialPlugin(addr: string): PluginRuntimeClient {
  const target = addr.replace(/^https?:\/\//, '')
  const Ctor = hubServices().hub.v1.PluginRuntime as unknown as new (
    address: string,
    credentials: grpc.ChannelCredentials,
  ) => PluginRuntimeClient
  return new Ctor(target, grpc.credentials.createInsecure())
}

/** 把一个回调式的一元调用收成 Promise。 */
function unary<Req, Res>(
  invoke: (
    req: Req,
    md: grpc.Metadata,
    options: grpc.CallOptions,
    cb: (err: grpc.ServiceError | null, res: Res) => void,
  ) => grpc.ClientUnaryCall,
  req: Req,
  options: grpc.CallOptions,
): Promise<Res> {
  return new Promise<Res>((resolve, reject) => {
    invoke(req, new grpc.Metadata(), options, (err, res) => (err ? reject(err) : resolve(res)))
  })
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err)
}

/**
 * 探一个当前空闲的 `127.0.0.1:port`。
 *
 * 测试里插件要先知道自己的对外地址（中台会连它做可达性探测），而监听端口由系统分配时
 * 拿不到真实端口——用它先探一个再显式指定。存在极小的竞态窗口，测试场景可接受。
 */
export async function freeAddr(): Promise<string> {
  const server = net.createServer()
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
  const address = server.address() as net.AddressInfo
  await new Promise<void>((resolve) => server.close(() => resolve()))
  return `127.0.0.1:${address.port}`
}

function statusError(code: grpc.status, details: string): grpc.ServiceError {
  return Object.assign(new Error(details), {
    code,
    details,
    metadata: new grpc.Metadata(),
  }) as grpc.ServiceError
}
