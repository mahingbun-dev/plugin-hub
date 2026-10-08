// 结构化日志：一行一个 JSON 对象，打到 stderr。
//
// 形状刻意与 Go 侧（`log/slog` 的 JSONHandler）对齐：
//
//	{"time":"...","level":"INFO","msg":"已注册到中台","plugin":"...","version":"..."}
//
// 为什么必须是这个形状：中台的接入指南里引用的就是它（`docs/plugin-onboarding.md`
// 的 L3 一节直接贴了这三行日志当验收判据）。两侧形状一旦不同，"看日志确认接入成功"
// 这件事就得按语言分别记一遍，而人的记忆是靠不住的。
//
// 为什么打到 stderr：stdout 留给业务输出（`hubprobe e2e` 那种把中台响应原样打到
// stdout、人话走 stderr 的约定），日志混进去会污染管道里的 jq。

/** 日志级别。数值越大越严重，与 slog 的 Level 取值同序。 */
export const LEVELS = ['DEBUG', 'INFO', 'WARN', 'ERROR'] as const
export type Level = (typeof LEVELS)[number]

/** 一条日志的附加字段。 */
export type Attrs = Record<string, unknown>

/**
 * 把 `HUB_LOG_LEVEL` 的取值翻成级别。认不出来的一律按 INFO——
 * 拼错一个级别不该让插件起不来。
 */
export function levelFromEnv(raw: string | undefined): Level {
  switch ((raw ?? '').trim().toLowerCase()) {
    case 'debug':
      return 'DEBUG'
    case 'warn':
      return 'WARN'
    case 'error':
      return 'ERROR'
    default:
      return 'INFO'
  }
}

export class Logger {
  private readonly min: number
  private readonly sink: (line: string) => void

  constructor(level: Level = 'INFO', sink: (line: string) => void = defaultSink) {
    this.min = LEVELS.indexOf(level)
    this.sink = sink
  }

  /**
   * 什么都不打的日志器。
   *
   * 给测试用：把插件跑起来时它自己会打注册 / 心跳日志，混进 `node --test` 的输出里
   * 会把断言失败的信息淹掉。要断言日志本身，就用带自定义 sink 的构造器。
   */
  static silent(): Logger {
    return new Logger('ERROR', () => {})
  }

  debug(msg: string, attrs?: Attrs): void {
    this.write('DEBUG', msg, attrs)
  }

  info(msg: string, attrs?: Attrs): void {
    this.write('INFO', msg, attrs)
  }

  warn(msg: string, attrs?: Attrs): void {
    this.write('WARN', msg, attrs)
  }

  error(msg: string, attrs?: Attrs): void {
    this.write('ERROR', msg, attrs)
  }

  /** 当前级别以下的日志是否会被丢掉。调用方用它避免白算一份昂贵的字段。 */
  enabled(level: Level): boolean {
    return LEVELS.indexOf(level) >= this.min
  }

  private write(level: Level, msg: string, attrs?: Attrs): void {
    if (!this.enabled(level)) return

    // 键序与 slog 一致：time / level / msg 在前，附加字段按调用方给的顺序跟在后面。
    // 顺序稳定才有意义——人眼扫日志时先看的是"哪一条"，不是"哪个插件"。
    const line: Record<string, unknown> = {
      time: formatTime(new Date()),
      level,
      msg,
    }
    for (const [key, value] of Object.entries(attrs ?? {})) {
      line[key] = normalize(value)
    }

    // JSON.stringify 默认会把 undefined 的键整个丢掉，与 slog「某条没有 detail 时
    // 它是空串而不是整个字段消失」的承诺不同。这里显式兜住：值为 undefined 时落成
    // 空串，字段集因此是稳定的，脚本可以无脑取。
    this.sink(`${JSON.stringify(line)}\n`)
  }
}

/** 缺省的输出通道：stderr。 */
function defaultSink(line: string): void {
  process.stderr.write(line)
}

/**
 * 本地时区的 RFC3339 时间戳。
 *
 * 不用 `toISOString()`：它给的是 UTC（`...Z`），而中台与插件都在同一个内网里，
 * 排查时人对的是本地墙上时钟。Go 侧 slog 也是本地偏移。
 *
 * 精度只到毫秒，不补足到 slog 那样的纳秒——Node 的 Date 本来就只有毫秒精度，
 * 补出来的位数是假的精度。
 */
export function formatTime(d: Date): string {
  const pad = (n: number, width = 2): string => String(n).padStart(width, '0')

  const offsetMinutes = -d.getTimezoneOffset()
  const sign = offsetMinutes < 0 ? '-' : '+'
  const abs = Math.abs(offsetMinutes)
  const offset = `${sign}${pad(Math.floor(abs / 60))}:${pad(abs % 60)}`

  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
    `T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}` +
    `.${pad(d.getMilliseconds(), 3)}${offset}`
  )
}

/**
 * 把任意值摊成能进 JSON 的东西。
 *
 * Error 走 `message`：Node 的 JSON.stringify(error) 给的是 `{}`（message/stack 都是
 * 不可枚举的），直接扔进去会得到一整行看不出所以然的日志——而那正是最需要日志的场合。
 */
function normalize(value: unknown): unknown {
  if (value instanceof Error) return value.message
  if (value === undefined) return ''
  if (value === null || typeof value !== 'object') return value
  if (Array.isArray(value)) return value.map(normalize)
  return value
}
