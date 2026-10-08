import process from 'node:process'

import * as grpc from '@grpc/grpc-js'

import {
  HEARTBEAT_FALLBACK_INTERVAL_MS,
  validateConfig,
  withDefaults,
  type Config,
  type ResolvedConfig,
} from './config.ts'
import { isWellKnownFQName, normalizeAnyForWire } from './envelope.ts'
import { GatewayClient, newPluginGatewayClient, type PluginGatewayClient } from './gateway.ts'
import { Logger } from './log.ts'
import { isGatewayAware, isStateAware, type CallContext, type Plugin } from './plugin.ts'
import { hubServices, normalizeManifest } from './proto.ts'
import { newHubStateClient, StateClient, type HubStateClient } from './state.ts'
import type {
  Envelope,
  HeartbeatRequest,
  HeartbeatResponse,
  PluginManifest,
  RegisterRequest,
  RegisterResponse,
  UnregisterRequest,
  UnregisterResponse,
  ValidateRequest,
  ValidateResponse,
} from './types.ts'

/** {@link run} 的可选参数。 */
export interface RunOptions {
  /**
   * 由调用方决定何时结束。
   *
   * 测试与嵌入式场景用它：给一个可中止的 signal 就能把插件干净地停掉，不必真的发信号。
   * **不给**时 {@link run} 会自己挂上 SIGINT / SIGTERM。
   */
  signal?: AbortSignal
}

/**
 * 启动插件，直到收到 SIGINT / SIGTERM（或 `opts.signal` 中止）。
 *
 * 它做四件事：起 gRPC 服务、向中台自注册、维持心跳、优雅退出时注销。
 * 插件作者只需要实现 {@link Plugin}。
 *
 * 三个刻意的行为：
 *
 *   - **注册会一直重试**：中台可能比插件晚起来，插件先启动是常态。
 *   - **被摘除后自动重新注册**：心跳响应里带 `reregisterRequired` 时重走注册流程，
 *     这是实例掉线后能自愈的关键。
 *   - **状态凭证失效后自动重新注册**：HubState 调用收到 UNAUTHENTICATED 时重走注册
 *     流程换新凭证。这是防御层，不是主恢复路径——中台把凭证落了库，重启对插件是透明的。
 */
export async function run(plugin: Plugin, cfg: Config, opts: RunOptions = {}): Promise<void> {
  validateConfig(cfg)
  const config = withDefaults(cfg)
  const log = config.logger

  checkManifest(plugin)

  // 监听地址归一化放在绑定之前：`HUB_LISTEN_ADDR=:9000` 是 Go 的写法（也是本 SDK 的
  // 缺省值），而 grpc-js 的地址解析要求有主机名，`:9000` 会被判成非法地址。
  const listenAddr = normalizeListenAddr(config.listenAddr)

  const server = new grpc.Server()
  server.addService(hubServices().hub.v1.PluginRuntime.service, pluginRuntimeHandlers(plugin, log))

  const credentials = hubCredentials(config.hubAddr)
  const target = hubTarget(config.hubAddr)
  const registry = newRegistryClient(target, credentials)

  // 状态客户端与注册共用一个 channel：两件事都打同一个中台地址，多开一条连接
  // 只会多一处要维护的断开重连
  const notifier = new Notifier()
  const state = new StateClient(
    newHubStateClient(target, credentials) as HubStateClient,
    () => notifier.notify(),
    config.stateCallTimeoutMs,
  )
  // 网关客户端挂在同一条连接上；onDenied 与状态客户端共用——凭证是同一份，
  // 中台重启时两面一起失效、一起换。callTimeout 只约束发现类调用（互调的预算
  // 是信封 deadline，见 GatewayClient 的说明）
  const gateway = new GatewayClient(
    newPluginGatewayClient(target, credentials) as PluginGatewayClient,
    () => notifier.notify(),
    config.stateCallTimeoutMs,
  )

  const boundPort = await bind(server, listenAddr)
  // 日志里报**实际**监听地址而不是配置里那份：配 `:0` 时真实端口由系统分配，
  // 照抄配置会打出 `127.0.0.1:0` 这种拨不通的地址，把排查引向错误的方向
  log.info('插件 gRPC 已监听', {
    listen: effectiveListenAddr(listenAddr, boundPort),
    advertise: config.advertiseAddr,
  })

  const controller = new AbortController()
  const detachSignals = attachSignals(controller, log, opts.signal)

  const registrar = new Registrar(plugin, config, registry, state, gateway, notifier, log)
  const registrarDone = registrar.loop(controller.signal)

  log.info('插件已启动', { hub: config.hubAddr, instance: config.instanceId })

  try {
    await aborted(controller.signal)
  } finally {
    log.info('收到退出信号，开始优雅退出')

    // 主动注销：中台据此立刻摘掉实例，不必等心跳超时。
    // 没拿到过注册凭证（注册被拒、或还没注册上）时它整个跳过，不发这个注定被拒的
    // 请求——见 Registrar.unregister
    await registrar.unregister(config.instanceId)
    await registrarDone
    await shutdown(server)

    detachSignals()
    log.info('插件已退出')
  }
}

// ---------------------------------------------------------------- 配置与检查

/**
 * 在启动时就把 manifest 的明显问题挡住。
 *
 * 这些问题中台也会拒，但那是网络往返之后的事——本地先炸能省一轮排查。
 */
export function checkManifest(plugin: Plugin): void {
  const manifest = plugin.manifest()
  if (!manifest) throw new Error('hubkit: manifest() 返回了空值')
  if (!manifest.name?.trim()) throw new Error('hubkit: manifest 缺少插件名（name）')
  if (!manifest.version?.trim()) {
    throw new Error('hubkit: manifest 缺少版本号（version）——flow 靠它锁定实例')
  }

  // 空 descriptor 本身合法：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的
  // proto。但声明了自有类型的就必须提供，否则中台会拒（声明的类型找不到出处）。
  const raw = plugin.descriptor()
  if (!raw || raw.length === 0) {
    for (const contract of [...(manifest.produces ?? []), ...(manifest.consumes ?? [])]) {
      if (!isWellKnownFQName(contract.fqName)) {
        throw new Error(
          `hubkit: manifest 声明了自有类型 ${contract.fqName}，但 descriptor() 返回空——` +
            '要么把它从 produces/consumes 里去掉，要么提供它的 proto',
        )
      }
    }
  }
}

/**
 * 把监听地址归一化成 grpc-js 认得的形状。
 *
 * `:9000` → `0.0.0.0:9000`。这是 Go 的 `net.Listen("tcp", ":9000")` 的等价写法，
 * 而 grpc-js 的地址解析要求主机名不能为空——不归一化的话，缺省配置直接绑不上。
 */
export function normalizeListenAddr(addr: string): string {
  if (addr.startsWith(':')) return `0.0.0.0${addr}`
  return addr
}

/** 把配置里的监听地址与系统实际分配的端口拼成能拨的地址，只用于日志与自检。 */
export function effectiveListenAddr(addr: string, port: number): string {
  const host = addr.slice(0, addr.lastIndexOf(':')) || '0.0.0.0'
  return `${host}:${port}`
}

/**
 * 中台地址 → grpc-js 的 target。
 *
 * `http://127.0.0.1:8093` → `127.0.0.1:8093`。协议头交给凭据去表达（见
 * {@link hubCredentials}），路径要丢掉：插件面是独立的端口（不经 nginx 的
 * `grpc_pass` 前缀路径），带上路径反而是把 HTTP 面的 `/hub-api` 误配到这里。
 */
export function hubTarget(hubAddr: string): string {
  const withoutScheme = hubAddr.trim().replace(/^https?:\/\//, '')
  const slash = withoutScheme.indexOf('/')
  return slash < 0 ? withoutScheme : withoutScheme.slice(0, slash)
}

/**
 * 与中台之间的传输凭据。
 *
 * `https://` 走 TLS，其余一律明文——与 Go 侧的判定完全一致。
 *
 * ⚠️ 中台证书由内网 CA 签发时，Node 侧要经 `NODE_EXTRA_CA_CERTS=/path/ca.crt`
 * 把它加进信任链（Linux 与 macOS 都认这个变量）。Go 侧走的是 `SSL_CERT_FILE`，
 * 而它**在 macOS 上不生效**——两个运行时各有一套，别把变量名记混了。
 */
export function hubCredentials(hubAddr: string): grpc.ChannelCredentials {
  return hubAddr.trim().startsWith('https://')
    ? grpc.credentials.createSsl()
    : grpc.credentials.createInsecure()
}

// ---------------------------------------------------------------- gRPC 服务

/** `hub.v1.PluginRuntime` 的五个方法。 */
function pluginRuntimeHandlers(plugin: Plugin, log: Logger): grpc.UntypedServiceImplementation {
  return {
    describe(_call: grpc.ServerUnaryCall<unknown, PluginManifest>, callback: grpc.sendUnaryData<PluginManifest>) {
      callback(null, normalizeManifest(plugin.manifest()))
    },

    async validate(
      call: grpc.ServerUnaryCall<ValidateRequest, ValidateResponse>,
      callback: grpc.sendUnaryData<ValidateResponse>,
    ) {
      const env = call.request?.envelope
      if (!env) {
        callback(null, { valid: false, issues: [issueOfEnvelopeMissing()] })
        return
      }
      try {
        const resp = await plugin.validate(callContext(call, env), env)
        callback(null, resp)
      } catch (err) {
        // 校验器自己崩了与"数据不合法"是两回事：前者是插件缺陷，要让中台看到 5xx
        // 而不是一个 valid:false——否则一条写错的校验规则会静默地拒掉所有数据
        callback(internalError('校验器执行失败', err))
      }
    },

    async handle(
      call: grpc.ServerUnaryCall<{ envelope?: Envelope }, unknown>,
      callback: grpc.sendUnaryData<{ envelope: Envelope | null }>,
    ) {
      const env = call.request?.envelope
      if (!env) {
        callback(invalidArgument('缺少信封'))
        return
      }
      try {
        const out = await plugin.handle(callContext(call, env), env)
        // 兜住手写载荷的字段名问题（见 normalizeAnyForWire 的说明）
        callback(null, { envelope: normalizeAnyForWire(out) })
      } catch (err) {
        // 插件自己的错误原样上报：中台会把它归到「插件调用失败」，调用方据此重试
        callback(internalError('插件处理失败', err))
      }
    },

    handleStream(call: grpc.ServerWritableStream<unknown, unknown>) {
      call.emit('error', {
        code: grpc.status.UNIMPLEMENTED,
        details: '流式处理尚未实现（见 docs/design.md 的载荷边界，随 M3 落地）',
      })
    },

    health(_call: grpc.ServerUnaryCall<unknown, unknown>, callback: grpc.sendUnaryData<{ healthy: boolean; message: string }>) {
      callback(null, { healthy: true, message: 'ok' })
    },
  } as unknown as grpc.UntypedServiceImplementation
}

/**
 * 把一次 gRPC 调用翻成插件看得懂的 {@link CallContext}。
 *
 * 中台取消调用（客户端断连、上游超时）时会触发 `cancelled`，转成 AbortSignal 之后
 * 插件可以用 `ctx.signal` 提前中止自己发起的外部调用——否则那些调用会一直挂到
 * 自己的超时，而结果已经没人要了。
 */
function callContext(call: grpc.ServerUnaryCall<unknown, unknown>, env: Envelope): CallContext {
  const controller = new AbortController()
  if (call.cancelled) controller.abort()
  else call.once('cancelled', () => controller.abort())

  const budgetMs = env.deadlineMs > 0 ? Math.max(0, env.deadlineMs - Date.now()) : Number.POSITIVE_INFINITY

  return { budgetMs, signal: controller.signal }
}

function issueOfEnvelopeMissing(): { path: string; message: string; severity: 'SEVERITY_ERROR' } {
  return { path: 'envelope', message: '缺少信封', severity: 'SEVERITY_ERROR' }
}

function invalidArgument(details: string): grpc.ServiceError {
  return Object.assign(new Error(details), {
    code: grpc.status.INVALID_ARGUMENT,
    details,
    metadata: new grpc.Metadata(),
  }) as grpc.ServiceError
}

function internalError(prefix: string, err: unknown): grpc.ServiceError {
  const message = err instanceof Error ? err.message : String(err)
  const details = `${prefix}: ${message}`
  return Object.assign(new Error(details), {
    code: grpc.status.INTERNAL,
    details,
    metadata: new grpc.Metadata(),
  }) as grpc.ServiceError
}

// ---------------------------------------------------------------- 注册与心跳

/** `hub.v1.PluginRegistry` 的客户端。 */
interface RegistryClient extends grpc.Client {
  Register: (
    req: RegisterRequest,
    md: grpc.Metadata,
    options: grpc.CallOptions,
    cb: (err: grpc.ServiceError | null, res: RegisterResponse) => void,
  ) => grpc.ClientUnaryCall
  Heartbeat: (
    req: HeartbeatRequest,
    md: grpc.Metadata,
    options: grpc.CallOptions,
    cb: (err: grpc.ServiceError | null, res: HeartbeatResponse) => void,
  ) => grpc.ClientUnaryCall
  Unregister: (
    req: UnregisterRequest,
    md: grpc.Metadata,
    options: grpc.CallOptions,
    cb: (err: grpc.ServiceError | null, res: UnregisterResponse) => void,
  ) => grpc.ClientUnaryCall
}

function newRegistryClient(target: string, credentials: grpc.ChannelCredentials): RegistryClient {
  const Ctor = hubServices().hub.v1.PluginRegistry as unknown as new (
    address: string,
    credentials: grpc.ChannelCredentials,
  ) => RegistryClient
  return new Ctor(target, credentials)
}

/**
 * 中台拒绝注册时抛出的错误。
 *
 * 拒绝原因是结构化的，`message` 会把它们逐条排开——插件作者照着改就行，不必去翻
 * 中台的日志。日志是另一个呈现渠道，走 {@link Registrar.logRegisterFailure}，
 * 把同一条信息摊成结构化的多行。
 */
export class RegistrationRejected extends Error {
  readonly rejections: { code: string; message: string; detail: string }[]

  constructor(rejections: { code: string; message: string; detail: string }[]) {
    super(RegistrationRejected.describe(rejections))
    this.name = 'RegistrationRejected'
    this.rejections = rejections
  }

  private static describe(rejections: { code: string; message: string; detail: string }[]): string {
    if (rejections.length === 0) return '中台拒绝了注册（未给出原因）'
    const lines = rejections.map((r) => {
      const head = `  - ${rejectCodeName(r.code)}: ${r.message}`
      return r.detail ? `${head}\n      ${r.detail}` : head
    })
    return `中台拒绝了注册：\n${lines.join('\n')}`
  }
}

/** 把拒绝码翻成人话（去掉 `REJECT_CODE_` 前缀）。 */
export function rejectCodeName(code: string | number): string {
  if (typeof code === 'number') return `未知拒绝码(${code})`
  return code.startsWith('REJECT_CODE_') ? code.slice('REJECT_CODE_'.length) : code
}

/**
 * 维持「注册 → 心跳 → 被摘除则重新注册」的循环。
 *
 * 三条返回心跳循环的路径（都表示需要重新走一遍注册）：中台要求重注册、心跳被拒、
 * 以及插件侧状态调用撞上 401。最后一条受 `retryIntervalMs` 这个冷却窗口约束。
 */
class Registrar {
  private intervalMs = HEARTBEAT_FALLBACK_INTERVAL_MS
  private lastRegisterAt = 0

  private readonly plugin: Plugin
  private readonly cfg: ResolvedConfig
  private readonly registry: RegistryClient
  private readonly state: StateClient
  private readonly gateway: GatewayClient
  private readonly notifier: Notifier
  private readonly log: Logger

  constructor(
    plugin: Plugin,
    cfg: ResolvedConfig,
    registry: RegistryClient,
    state: StateClient,
    gateway: GatewayClient,
    notifier: Notifier,
    log: Logger,
  ) {
    this.plugin = plugin
    this.cfg = cfg
    this.registry = registry
    this.state = state
    this.gateway = gateway
    this.notifier = notifier
    this.log = log
  }

  async loop(signal: AbortSignal): Promise<void> {
    while (!signal.aborted) {
      try {
        await this.registerOnce(signal)
      } catch (err) {
        if (signal.aborted) return
        this.logRegisterFailure(err)
        if (!(await sleep(this.cfg.retryIntervalMs, signal))) return
        continue
      }
      await this.heartbeatLoop(signal)
    }
  }

  /**
   * 把一次注册失败讲成「看一眼就懂」的样子。
   *
   * 中台拒绝时给的是结构化原因，这里让每条原因各占一行。日志走的是 JSON handler，
   * 一整段多行文本会被转义成 `\n` 塞进单个字段——一行里糊着 N 条原因，得靠人脑
   * 反解析才看得出哪条是哪条。拆成 `code` / `message` / `detail` 三列之后，每行本身
   * 就是完整的一条，读日志不需要 jq，也不需要任何工具。
   *
   * 重试间隔单独收尾一行，而不是跟在每条原因后面：它是「接下来会怎样」，与「错在哪」
   * 不是一回事，逐条重复只会把原因行淹掉。`reasons` 是原因条数，用来兜底——日志被
   * 截断时，一眼能看出还有几条没打出来。
   */
  private logRegisterFailure(err: unknown): void {
    if (err instanceof RegistrationRejected && err.rejections.length > 0) {
      for (const rejection of err.rejections) {
        this.log.error('中台拒绝了注册', {
          code: rejectCodeName(rejection.code),
          message: rejection.message,
          // 字段集是稳定的：某条没有 detail 时它是空串，不会整个字段消失
          detail: rejection.detail,
        })
      }
      this.log.error('注册未通过，稍后重试', {
        reasons: err.rejections.length,
        retry_in: `${this.cfg.retryIntervalMs / 1000}s`,
      })
      return
    }

    // 连不上中台、网络抖动这类错误本身就是单行的，和重试间隔打在同一行里正好
    this.log.error('注册未通过，稍后重试', {
      err,
      retry_in: `${this.cfg.retryIntervalMs / 1000}s`,
    })
  }

  private async registerOnce(signal: AbortSignal): Promise<void> {
    const manifest = normalizeManifest(this.plugin.manifest())
    const request: RegisterRequest = {
      pluginName: manifest.name,
      version: manifest.version,
      instanceId: this.cfg.instanceId,
      advertiseAddr: this.cfg.advertiseAddr,
      manifest,
      descriptorSet: this.plugin.descriptor(),
    }

    const resp = await this.call<RegisterResponse>((cb) => this.registry.Register(request, new grpc.Metadata(), {}, cb))

    if (!resp.accepted) throw new RegistrationRejected(resp.rejections ?? [])

    // 记下成功注册的时刻：denial 触发的强制重注册靠它做速率下限（见 heartbeatLoop）
    this.lastRegisterAt = Date.now()

    // 凭证随每次注册轮换，这里覆盖旧的。
    //
    // 它只写这一处，注销时也从这里取（见 unregister）——**状态凭证与注销凭证是
    // 同一个东西**：中台侧 HubState 的认证与注销的属主判定查的是同一列
    // （plugin_instances.state_token）。存两份就有两份可能对不上，而"对不上"的表现
    // 是注销被静默拒绝、退化成等心跳超时摘除，不会报错。
    this.state.setToken(resp.stateToken ?? '')
    // 网关面与状态面共用同一份凭证，轮换时一起换
    this.gateway.setToken(resp.stateToken ?? '')
    if (!resp.stateToken) this.log.warn('中台未下发状态凭证，HubState 将不可用')
    // 每次注册后都注入一次：插件可能还持有上一个凭证时期的客户端引用
    if (isStateAware(this.plugin)) this.plugin.setState(this.state)
    if (isGatewayAware(this.plugin)) this.plugin.setGateway(this.gateway)

    if (resp.heartbeatIntervalSeconds > 0) this.intervalMs = resp.heartbeatIntervalSeconds * 1000

    for (const warning of resp.warnings ?? []) this.log.warn('中台提示', { warning })
    this.log.info('已注册到中台', {
      plugin: manifest.name,
      version: manifest.version,
      instance: resp.instanceId,
    })
  }

  /**
   * 按中台指定的周期续期。返回即表示需要重新注册（或 signal 已中止）。
   *
   * 三条触发的路径见类注释；denial 那条必须有速率下限：denial 可能连续不断（中台校验
   * 滞后、撤销尚未传播，或非凭证原因也回 401）。没有它，一次成功注册之后紧接着排空
   * denial 就是零延迟，注册速率等于 Register RPC 的延迟——无限自旋，同时还在反复探测
   * 插件自己的地址。窗口内到达的 denial 直接丢掉。
   */
  private async heartbeatLoop(signal: AbortSignal): Promise<void> {
    for (;;) {
      const outcome = await this.waitTick(this.intervalMs, signal)

      if (outcome === 'aborted') return

      if (outcome === 'denied') {
        const since = Date.now() - this.lastRegisterAt
        if (since < this.cfg.retryIntervalMs) {
          this.log.warn('状态凭证被拒，但距上次注册不足冷却窗口，忽略本次', {
            since_ms: since,
            cooldown_ms: this.cfg.retryIntervalMs,
          })
          continue
        }
        this.log.warn('状态凭证被拒，重新注册以换取新凭证')
        return
      }

      try {
        const resp = await this.call<HeartbeatResponse>((cb) =>
          this.registry.Heartbeat({ instanceId: this.cfg.instanceId }, new grpc.Metadata(), {}, cb),
        )
        if (!resp.accepted || resp.reregisterRequired) {
          this.log.warn('中台要求重新注册（实例可能已被摘除）')
          return
        }
        // 中台可以在心跳回执里改周期（例如它自己刚被重新配置过）
        if (resp.heartbeatIntervalSeconds > 0) {
          this.intervalMs = resp.heartbeatIntervalSeconds * 1000
        }
      } catch (err) {
        if (signal.aborted) return
        // 网络抖动不该让插件停止心跳，下一拍继续
        this.log.warn('心跳失败', { err })
      }
    }
  }

  /**
   * 等下一拍，或等到被中止 / 被 denial 唤醒。
   *
   * 用「谁先到算谁」的单一出口，而不是三个各自 await 的分支：三个分支里任何一个
   * 先返回都会把定时器与监听器漏在身后，而这是一个要跑几个月的循环。
   */
  private waitTick(intervalMs: number, signal: AbortSignal): Promise<'tick' | 'aborted' | 'denied'> {
    return new Promise((resolve) => {
      let settled = false
      let unregister = (): void => {}

      const settle = (outcome: 'tick' | 'aborted' | 'denied'): void => {
        if (settled) return
        settled = true
        clearTimeout(timer)
        signal.removeEventListener('abort', onAbort)
        unregister()
        // denial 是边沿触发的信号，读走即消费——与 Go 侧那个容量 1 的 channel 一致
        if (outcome === 'denied') this.notifier.reset()
        resolve(outcome)
      }

      const timer = setTimeout(() => settle('tick'), intervalMs)
      const onAbort = (): void => settle('aborted')
      signal.addEventListener('abort', onAbort, { once: true })
      unregister = this.notifier.register(() => settle('denied'))

      if (signal.aborted) settle('aborted')
    })
  }

  /**
   * 主动注销。中台据此立刻摘掉实例，不必等心跳超时。
   *
   * **没拿到过凭证就整个跳过**。凭证只在注册成功时下发，为空说明本实例压根没进过
   * 注册表（被中台拒了、或还没注册上就退出了），没有实例行可摘除。而 `instanceId`
   * 是插件自报的、可以跟别的插件撞（缺省「主机名-PID」，同一 host 网络下容器 PID
   * 又都是 1）——此时发一次不带身份的注销，中台若只按 `instanceId` 删行，删掉的
   * 正是**对方**那一行：对方的心跳仍按 `instanceId` 命中、返回 accepted，毫无察觉。
   *
   * 中台侧现在也会拒（见 `UnregisterRequest.stateToken`），插件侧这一道是别发这个
   * 注定被拒的请求。
   *
   * 这也决定了**插件先于中台升级**时的行为：旧中台的 `RegisterResponse` 没有这个
   * 字段，SDK 拿到的是空串，于是注销整个跳过——对旧中台也就不再有优雅注销，只能等
   * 它心跳超时摘除。窗口有界（中台升级完就恢复），而反过来放行的代价是可能删掉别的
   * 插件的实例行。
   */
  async unregister(instanceId: string): Promise<void> {
    // 凭证从 StateClient 取：注册循环把它写在唯一一处（registerOnce），
    // 复用它就不存在"注销凭证与状态凭证对不上"这种没人会注意到的状态
    const stateToken = this.state.currentToken()
    if (stateToken === '') {
      this.log.info('本实例没有注册凭证，跳过主动注销（没注册成功就没有可摘除的实例行）')
      return
    }

    try {
      await this.call<UnregisterResponse>(
        (cb) =>
          this.registry.Unregister(
            { instanceId, reason: '插件优雅退出', stateToken },
            new grpc.Metadata(),
            { deadline: Date.now() + 3_000 },
            cb,
          ),
      )
    } catch (err) {
      // 注销失败不该改变退出码：中台的心跳超时兜底会把实例摘掉，插件这边的活已经干完了
      this.log.warn('注销失败（中台会在心跳超时后自行摘除）', { err })
    }
  }

  /** 发一次注册面上的 RPC，把回调式 API 收成 Promise。 */
  private call<T>(invoke: (cb: (err: grpc.ServiceError | null, res: T) => void) => grpc.ClientUnaryCall): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      invoke((err, res) => (err ? reject(err) : resolve(res)))
    })
  }
}

/**
 * 边沿触发的通知器，对应 Go 侧那个容量 1 的 `chan struct{}`。
 *
 * 缓冲 1 + 非阻塞发送：并发调用一起撞上 401 时只留一个信号，不会把注册循环叫成风暴。
 */
class Notifier {
  private pending = false
  private listeners = new Set<() => void>()

  notify(): void {
    this.pending = true
    const listeners = [...this.listeners]
    this.listeners.clear()
    for (const listener of listeners) listener()
  }

  register(onNotify: () => void): () => void {
    if (this.pending) {
      onNotify()
      return () => {}
    }
    this.listeners.add(onNotify)
    return () => this.listeners.delete(onNotify)
  }

  /** 消费掉待处理的信号。 */
  reset(): void {
    this.pending = false
  }
}

// ---------------------------------------------------------------- 生命周期

function attachSignals(controller: AbortController, log: Logger, external?: AbortSignal): () => void {
  if (external) {
    if (external.aborted) controller.abort()
    else external.addEventListener('abort', () => controller.abort(), { once: true })
    return () => {}
  }

  const onSignal = (signal: NodeJS.Signals): void => {
    log.info(`收到 ${signal}，准备退出`)
    controller.abort()
  }
  process.on('SIGINT', onSignal)
  process.on('SIGTERM', onSignal)
  return () => {
    process.off('SIGINT', onSignal)
    process.off('SIGTERM', onSignal)
  }
}

function aborted(signal: AbortSignal): Promise<never> {
  if (signal.aborted) return Promise.resolve() as unknown as Promise<never>
  return new Promise<never>((resolve) => signal.addEventListener('abort', resolve as () => void, { once: true }))
}

/** 绑定端口，返回实际端口号。 */
function bind(server: grpc.Server, addr: string): Promise<number> {
  return new Promise((resolve, reject) => {
    server.bindAsync(addr, grpc.ServerCredentials.createInsecure(), (err, port) => {
      if (err) reject(new Error(`hubkit: 监听 ${addr} 失败: ${err.message}`))
      else resolve(port)
    })
  })
}

/**
 * 优雅停机：等在途调用结束再关。
 *
 * 不能无限等——一个卡住的 handler 会让进程永远不退，而那时中台已经在按心跳超时
 * 摘你了。给一个上限，到点强制关。
 */
function shutdown(server: grpc.Server): Promise<void> {
  return new Promise((resolve) => {
    const force = setTimeout(() => {
      server.forceShutdown()
    }, 5_000)

    server.tryShutdown(() => {
      clearTimeout(force)
      resolve()
    })
  })
}

/** 睡 `ms` 毫秒。被中止时返回 false。 */
function sleep(ms: number, signal: AbortSignal): Promise<boolean> {
  if (signal.aborted) return Promise.resolve(false)
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      signal.removeEventListener('abort', onAbort)
      resolve(true)
    }, ms)
    const onAbort = (): void => {
      clearTimeout(timer)
      resolve(false)
    }
    signal.addEventListener('abort', onAbort, { once: true })
  })
}
