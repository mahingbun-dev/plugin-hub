import os from 'node:os'
import process from 'node:process'

import { Logger, levelFromEnv, type Level } from './log.ts'

/** 本插件 gRPC 的缺省监听地址。与 Go 侧同名同值。 */
export const DEFAULT_LISTEN_ADDR = ':9000'

/**
 * 注册失败后的重试间隔。
 *
 * 注册会一直重试而不是放弃：中台可能比你晚起来，而插件进程先于中台启动是常态。
 */
export const REGISTER_RETRY_INTERVAL_MS = 5_000

/** 中台没告诉我们心跳周期时的兜底值。 */
export const HEARTBEAT_FALLBACK_INTERVAL_MS = 10_000

/**
 * 单次 HubState 调用的默认时间上限。
 *
 * 与 Go 侧取同一个值（2s）而不是随手给个 5s：它应当**短于**中台侧的 Redis 响应超时
 * （5s），由客户端先放弃，插件才有机会走 fail-open。
 */
export const DEFAULT_STATE_CALL_TIMEOUT_MS = 2_000

/**
 * 骨架的运行参数。除地址外都可省，缺省值见 {@link withDefaults}。
 *
 * 生产路径上不必手写它——用 {@link configFromEnv} 从环境变量读。
 */
export interface Config {
  /** 中台插件面地址，例如 `http://127.0.0.1:8093`。 */
  hubAddr: string

  /**
   * 中台可达的本插件地址，例如 `http://10.0.0.5:9000`。
   *
   * 中台在注册时会连它做**可达性探测**，所以必须是「从中台那边拨得通」的地址，
   * 而不是本机视角的 localhost——这是接入时最容易踩的坑。
   */
  advertiseAddr: string

  /** 本插件 gRPC 的监听地址，缺省 {@link DEFAULT_LISTEN_ADDR}。 */
  listenAddr?: string

  /**
   * 实例标识，缺省 `主机名-PID`。
   *
   * 同一 ID 重复注册视为进程重启（中台会刷新地址与心跳），不产生重复实例。
   */
  instanceId?: string

  /** 注册失败后的重试间隔，缺省 {@link REGISTER_RETRY_INTERVAL_MS}。测试里会调到毫秒级。 */
  retryIntervalMs?: number

  /** 单次 HubState 调用的时间上限，缺省 {@link DEFAULT_STATE_CALL_TIMEOUT_MS}。 */
  stateCallTimeoutMs?: number

  /** 结构化日志，缺省打到 stderr 的 JSON。 */
  logger?: Logger
}

/** 补齐过缺省值的配置：每个字段都有值。 */
export interface ResolvedConfig {
  hubAddr: string
  advertiseAddr: string
  listenAddr: string
  instanceId: string
  retryIntervalMs: number
  stateCallTimeoutMs: number
  logger: Logger
}

/** 配置缺项时抛出的错误。它的 message 是照着能直接改的口吻写的。 */
export class ConfigError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ConfigError'
  }
}

/**
 * 检查必填项，并给出能直接照做的提示。
 *
 * 与 Go 侧一样把「缺哪个」一次说全，而不是发现一个报一个：漏配两个变量的场合远多于
 * 只漏一个。
 */
export function validateConfig(cfg: Config): void {
  const missing: string[] = []
  if (!cfg.hubAddr?.trim()) missing.push('HUB_ADDR（中台插件面地址）')
  if (!cfg.advertiseAddr?.trim()) missing.push('HUB_ADVERTISE_ADDR（中台可达的本插件地址）')
  if (missing.length > 0) {
    throw new ConfigError(`hubkit: 缺少必填配置 ${missing.join('、')}`)
  }
}

/** 补齐缺省值。返回新对象，不改传入的那份。 */
export function withDefaults(cfg: Config): ResolvedConfig {
  return {
    hubAddr: cfg.hubAddr,
    advertiseAddr: cfg.advertiseAddr,
    listenAddr: cfg.listenAddr || DEFAULT_LISTEN_ADDR,
    instanceId: cfg.instanceId || defaultInstanceId(),
    retryIntervalMs:
      cfg.retryIntervalMs && cfg.retryIntervalMs > 0
        ? cfg.retryIntervalMs
        : REGISTER_RETRY_INTERVAL_MS,
    stateCallTimeoutMs:
      cfg.stateCallTimeoutMs && cfg.stateCallTimeoutMs > 0
        ? cfg.stateCallTimeoutMs
        : DEFAULT_STATE_CALL_TIMEOUT_MS,
    logger: cfg.logger ?? new Logger('INFO'),
  }
}

/** `主机名-PID`。取不到主机名时退化成 `unknown-host`，与 Go 侧一致。 */
export function defaultInstanceId(): string {
  let host = 'unknown-host'
  try {
    host = os.hostname()
  } catch {
    // 极少数容器里 hostname 会取不到；宁可给个能注册的 id 也不要抛
  }
  return `${host}-${process.pid}`
}

/**
 * 从环境变量读取配置；缺省值由 {@link withDefaults} 补齐。
 *
 * ```
 * HUB_ADDR            中台插件面地址（必填）
 * HUB_ADVERTISE_ADDR  本插件对外可达地址（必填）
 * HUB_LISTEN_ADDR     本插件监听地址（缺省 :9000）
 * HUB_INSTANCE_ID     实例标识（缺省 主机名-PID）
 * HUB_LOG_LEVEL       debug / info / warn / error（缺省 info）
 * ```
 *
 * ⚠️ `HUB_INSTANCE_ID=`（**空串**）等同没设：空串走 `withDefaults` 补成「主机名-PID」，
 * 注册得成。只有**纯空白**（如 `HUB_INSTANCE_ID=" "`）才会带着一个空白 id 去注册、
 * 被判「注册请求缺少 instance_id」——踩坑的是复制粘贴带上的空格，所以这里只把
 * 空串当没设、**不 trim**。
 */
export function configFromEnv(env: NodeJS.ProcessEnv = process.env): Config {
  const level: Level = levelFromEnv(env.HUB_LOG_LEVEL)
  return {
    hubAddr: env.HUB_ADDR ?? '',
    advertiseAddr: env.HUB_ADVERTISE_ADDR ?? '',
    listenAddr: env.HUB_LISTEN_ADDR || undefined,
    instanceId: env.HUB_INSTANCE_ID || undefined,
    logger: new Logger(level),
  }
}
