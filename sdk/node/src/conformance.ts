import * as grpc from '@grpc/grpc-js'

import { descriptorMessages } from './descriptor.ts'
import { emptyEnvelope, isWellKnownFQName, withPayloadJSON } from './envelope.ts'
import type { Plugin } from './plugin.ts'
import { hubServices, normalizeManifest } from './proto.ts'
import { validPluginName } from './rules.ts'
import type {
  Envelope,
  HealthResponse,
  PluginManifest,
  ValidateResponse,
} from './types.ts'

/**
 * 插件的契约一致性自测套件。
 *
 * 插件接入中台之前必须跑通它。检查的都是「中台在真实调用时依赖、但等生产才发现
 * 代价太大」的约定：契约自洽、校验器不崩、插件体真的返回信封。
 *
 * 分两部分，因为它们的输入不同：
 *
 *   - {@link local}：只看插件对象，不需要把它跑起来。检查 manifest 与 descriptor
 *     是否自洽——中台的注册期校验就是这一套，本地先跑能省一轮「改完推上去才发现被拒」。
 *   - {@link runtime}：对运行中的插件跑，检查它在真实调用下的行为。
 *
 * ⚠️ 这两关全绿**不等于**能接入：它们验的都是「插件自己做得对不对」，而最难的
 * 那一关是「中台能不能按你上报的 `HUB_ADVERTISE_ADDR` 拨通你」——地址填错时
 * 这两关照样全绿，插件却永远接不进来。那一关的判据是中台的回执（插件启动日志里的
 * 「已注册到中台」），只有真中台或 `crates/hub-mock` 给得了。
 */

/** 各项运行时检查的时间上限。插件是外部进程，卡住的检查要尽快暴露而不是把测试挂死。 */
const CHECK_TIMEOUT_MS = 5_000

/** 工具名要拼进 MCP 的工具标识，字符集更窄。 */
const TOOL_NAME_PATTERN = /^[A-Za-z0-9_-]+$/

/** 一项检查的结果。 */
export interface Check {
  name: string
  passed: boolean
  detail: string
}

/** 一套检查的结果。 */
export class Report {
  readonly checks: Check[] = []
  // 不是 readonly：找不到 manifest 时要把 subject 改成「（未命名插件）」
  subject: string

  constructor(subject: string) {
    this.subject = subject
  }

  add(name: string, passed: boolean, detail = ''): void {
    this.checks.push({ name, passed, detail })
  }

  /** 是否全部通过。 */
  passed(): boolean {
    return this.checks.every((c) => c.passed)
  }

  /** 未通过的检查。 */
  failures(): Check[] {
    return this.checks.filter((c) => !c.passed)
  }

  toString(): string {
    const lines = [`契约一致性检查 ${this.subject}`]
    for (const check of this.checks) {
      const mark = check.passed ? '✓' : '✗'
      lines.push(`  ${mark} ${check.name}${check.detail ? ` —— ${check.detail}` : ''}`)
    }
    return `${lines.join('\n')}\n`
  }
}

/**
 * 检查插件对象自身的自洽性，不需要把它跑起来。
 *
 * 复现的是中台注册期的那几条校验，因此它能挡住绝大多数「推上去才发现被拒」的问题。
 */
export function local(plugin: Plugin): Report {
  const report = new Report('（本地）')

  const raw = plugin.manifest()
  if (!raw) {
    report.subject = '（未命名插件）'
    report.add('manifest 存在', false, 'manifest() 返回了空值')
    return report
  }

  const manifest = normalizeManifest(raw)
  report.subject = `${manifest.name}@${manifest.version}`
  report.add('manifest 存在', true)

  const name = manifest.name
  if (!name.trim()) {
    report.add('插件名合法', false, '缺少插件名 name')
  } else if (!isValidPluginName(name)) {
    report.add('插件名合法', false, '只允许字母数字与 -_，最长 64 字符，且以字母数字开头')
  } else {
    report.add('插件名合法', true, name)
  }

  if (!manifest.version.trim()) {
    report.add('版本号存在', false, '缺少版本号 —— flow 靠它锁定实例')
  } else {
    report.add('版本号存在', true, manifest.version)
  }

  // descriptor 是中台做字段级兼容检查的依据。
  // 空的是合法的：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 proto。
  const descriptor = plugin.descriptor()
  let messages = new Set<string>()
  if (!descriptor || descriptor.length === 0) {
    report.add('descriptor 可用', true, '无自有 proto（只用 well-known 载荷）')
  } else {
    try {
      messages = descriptorMessages(descriptor)
      report.add('descriptor 可用', true, `${descriptor.length} 字节、${messages.size} 个消息类型`)
    } catch (err) {
      report.add('descriptor 可用', false, err instanceof Error ? err.message : String(err))
      return report
    }
  }

  checkDeclaredMessages(report, manifest, messages)
  checkTools(report, manifest)

  return report
}

/**
 * 验证「自述与编译产物一致」。
 *
 * 这是最有价值的一条：改了 proto 忘了重新生成、或者消息改名后忘了同步 manifest，
 * 都在这里被抓住，而不是等注册时被中台拒。
 */
function checkDeclaredMessages(
  report: Report,
  manifest: PluginManifest,
  messages: Set<string>,
): void {
  const missing: string[] = []
  const check = (direction: string, contracts: { fqName: string }[]): void => {
    for (const contract of contracts) {
      if (isWellKnownFQName(contract.fqName)) continue
      if (!messages.has(contract.fqName)) {
        missing.push(`${direction} 里的 ${contract.fqName}`)
      }
    }
  }
  check('produces', manifest.produces)
  check('consumes', manifest.consumes)

  if (missing.length > 0) {
    report.add(
      '声明的类型都在 descriptor 中',
      false,
      `${missing.join('；')} —— manifest 的自述必须与提交的 proto 一致`,
    )
    return
  }

  if (manifest.produces.length === 0 && manifest.consumes.length === 0) {
    report.add(
      '声明了契约',
      false,
      '既没有 produces 也没有 consumes —— 至少声明一个，接受直接调用的插件请声明 google.protobuf.Struct',
    )
    return
  }

  report.add(
    '声明的类型都在 descriptor 中',
    true,
    `produces ${manifest.produces.length} 个、consumes ${manifest.consumes.length} 个`,
  )
}

function checkTools(report: Report, manifest: PluginManifest): void {
  if (manifest.tools.length === 0) {
    // 不暴露工具是合法的：插件可能只参与 flow
    report.add('工具声明合法', true, '未声明工具（仅参与 flow）')
    return
  }

  const seen = new Set<string>()
  for (const tool of manifest.tools) {
    if (!TOOL_NAME_PATTERN.test(tool.name)) {
      report.add(
        '工具声明合法',
        false,
        `工具名 ${JSON.stringify(tool.name)} 含非法字符 —— 它要拼进 MCP 的工具标识`,
      )
      return
    }
    if (seen.has(tool.name)) {
      report.add('工具声明合法', false, `工具 ${tool.name} 重复声明 —— 同一插件内工具名必须唯一`)
      return
    }
    seen.add(tool.name)
  }

  report.add('工具声明合法', true, `${seen.size} 个工具`)
}

/** `PluginRuntime` 的客户端，供 conformance 与调试用。 */
export class PluginClient {
  private readonly runtime: Record<string, (...args: unknown[]) => grpc.ClientUnaryCall>
  private readonly client: grpc.Client

  constructor(client: grpc.Client) {
    this.client = client
    this.runtime = client as unknown as Record<string, (...args: unknown[]) => grpc.ClientUnaryCall>
  }

  /** 连上一个插件的 gRPC 地址（如 `http://127.0.0.1:9000`）。 */
  static dial(addr: string): PluginClient {
    const target = addr.trim().replace(/^https?:\/\//, '')
    const Ctor = hubServices().hub.v1.PluginRuntime as unknown as new (
      address: string,
      credentials: grpc.ChannelCredentials,
    ) => grpc.Client
    return new PluginClient(new Ctor(target, grpc.credentials.createInsecure()))
  }

  close(): void {
    this.client.close()
  }

  describe(): Promise<PluginManifest> {
    return this.unary('Describe', {})
  }

  health(): Promise<HealthResponse> {
    return this.unary('Health', {})
  }

  validate(env: Envelope): Promise<ValidateResponse> {
    return this.unary('Validate', { envelope: env })
  }

  async handle(env: Envelope): Promise<Envelope | null> {
    const resp = await this.unary<{ envelope: Envelope | null }>('Handle', { envelope: env })
    return resp.envelope
  }

  private unary<T>(method: string, request: unknown): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const options: grpc.CallOptions = { deadline: Date.now() + CHECK_TIMEOUT_MS }
      // 必须**从客户端对象上调用**，不能先取出来再调：grpc-js 的方法内部要用 `this`
      // （`this.checkOptionalUnaryResponseArguments`），而 ESM 是严格模式，脱开对象
      // 调用时 `this` 是 undefined，报出来的是「Cannot read properties of undefined」，
      // 完全看不出是丢了 this
      if (typeof this.runtime[method] !== 'function') {
        reject(new Error(`插件不支持方法 ${method}`))
        return
      }
      this.runtime[method](request, new grpc.Metadata(), options, (err: grpc.ServiceError | null, res: T) => {
        if (err) reject(err)
        else resolve(res)
      })
    })
  }
}

/** 对运行中的插件检查运行时行为。`pluginAddr` 是插件的 gRPC 地址。 */
export async function runtime(pluginAddr: string): Promise<Report> {
  const report = new Report(pluginAddr)

  let client: PluginClient
  try {
    client = PluginClient.dial(pluginAddr)
  } catch (err) {
    // 本地检查用的是惰性连接（只登记地址不发包），所以"地址写错、端口没人听"
    // 当时不报错，要等第一次真正的 RPC 才浮出来
    report.add('插件地址可用', false, err instanceof Error ? err.message : String(err))
    return report
  }

  try {
    let health: HealthResponse
    try {
      health = await client.health()
    } catch (err) {
      report.add('Health 可应答', false, err instanceof Error ? err.message : String(err))
      return report
    }

    if (!health.healthy) {
      report.add('Health 可应答', false, `插件自报不健康: ${health.message}`)
      return report
    }
    report.add('Health 可应答', true)

    try {
      const manifest = await client.describe()
      report.add('Describe 可应答', true, `${manifest.name}@${manifest.version}`)
    } catch (err) {
      report.add('Describe 可应答', false, err instanceof Error ? err.message : String(err))
    }

    // 空信封：插件必须能处理，而不是 panic 或挂住
    try {
      await client.validate(emptyEnvelope())
      report.add('校验器对空信封不崩', true)
    } catch (err) {
      report.add('校验器对空信封不崩', false, err instanceof Error ? err.message : String(err))
    }

    const probe = withPayloadJSON(emptyEnvelope(), { conformance: true })
    probe.messageId = 'conformance-1'

    try {
      const resp = await client.validate(probe)
      if (resp.valid) {
        report.add('校验器可处理 JSON 载荷', true, '通过')
      } else {
        // 探针载荷本来就可能不满足业务规则，拒绝是合法结果
        report.add(
          '校验器可处理 JSON 载荷',
          true,
          `拒绝（${resp.issues.length} 条问题）—— 探针载荷不满足业务规则属正常`,
        )
      }
    } catch (err) {
      report.add('校验器可处理 JSON 载荷', false, err instanceof Error ? err.message : String(err))
    }

    try {
      const out = await client.handle(probe)
      if (!out) {
        report.add('插件体返回信封', false, '返回了空信封 —— 中台会把它当成插件异常')
      } else {
        report.add('插件体返回信封', true, '已返回信封')
      }
    } catch (err) {
      // 被业务逻辑拒掉是正常的，但必须是「明确报错」而不是超时或 panic
      const message = err instanceof Error ? err.message : String(err)
      const isTimeout = message.includes('DEADLINE_EXCEEDED')
      if (isTimeout) {
        report.add('插件体返回信封', false, `超时：${message}`)
      } else {
        report.add('插件体返回信封', true, `拒绝处理（${message}）—— 探针载荷不满足业务规则属正常`)
      }
    }
  } finally {
    client.close()
  }

  return report
}

/** 合法的插件名判定。与 `crates/hub-registry/src/validate.rs` 等价（见 `rules.ts`）。 */
function isValidPluginName(name: string): boolean {
  return validPluginName(name)
}
