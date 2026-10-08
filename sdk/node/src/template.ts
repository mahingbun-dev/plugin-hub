// 脚手架模板的渲染：把 `@@key@@` 占位符替换成实际取值。
//
// 同一份规则在中台侧（`crates/hub-templates/src/render.rs`）与 Go 侧
// （`sdk/go/cmd/hub-plugin/render.go`）各有一份实现，三边的行为必须一致——
// CLI 生成的工程与网页下载的工程是同一批文件，渲染结果不一致就是两条产品线。
// **改动这里请同步改那两处**，并把占位符集合一起改。
//
// 用 `@@key@@` 而不是各种模板引擎的语法：`@@` 与任何语言的语法都不冲突，两侧各
// 三十行就能实现同一套规则；而 EJS / handlebars 那种"模板里能写代码"的东西，
// 会让「生成工程」这件事多出一门要学的语言与一片要审的逻辑。

/** 占位符的定界符。三侧相同。 */
export const PLACEHOLDER = '@@'

/** 认得的占位符。模板里写了别的会报错，而不是留空。 */
export const KNOWN_KEYS = [
  /** 插件名，同时是 manifest.name、目录名与 MCP 工具前缀的来源。 */
  'name',
  /** 包名（Node 侧就是 npm 包名）。 */
  'package',
  /** SDK 在生成工程里的**引用路径**（Node 给包名，如 `@example/hubkit`）。 */
  'sdk_module',
  /** SDK 源码在包内的**相对路径**（由打包器注入为 `./sdk`）。 */
  'sdk_path',
  /** 中台插件面地址。 */
  'hub_addr',
  /** 接入指南共通层的挂载点，只有 AGENTS.md 里有。 */
  'onboarding',
] as const

export type PlaceholderKey = (typeof KNOWN_KEYS)[number]

/** 渲染用的取值。 */
export type RenderValues = Record<PlaceholderKey, string>

/** 渲染失败。 */
export class RenderError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'RenderError'
  }
}

/**
 * 把模板里的 `@@key@@` 替换成 `values` 里对应的值。
 *
 * **未定义的占位符直接报错，不留空**：留空会生成一个 manifest 里插件名为空、
 * 或 import 路径残缺的工程，那种工程要等到编译或注册时才炸——而那时已经离
 * 「生成」很远了，人不会想到回头怀疑模板。
 *
 * 分两趟：第一趟把 `@@onboarding@@` 展开成共通层正文，第二趟替换**正文里自己的**
 * 占位符。单趟做不到——扫描是自左向右的，刚插入的文本不会被重新扫一遍，于是共通层
 * 里的 `@@name@@` 会原样留在产物里。（Rust 侧同样只做一趟，所以共通层正文里不要
 * 写占位符；Go 侧的两趟实现是为了兜住历史遗留，这里跟着它。）
 */
export function render(raw: string, values: RenderValues): string {
  const once = renderOnce(raw, values)
  if (!once.includes(PLACEHOLDER)) return once
  return renderOnce(once, values)
}

function renderOnce(raw: string, values: RenderValues): string {
  let out = ''
  let rest = raw

  for (;;) {
    const start = rest.indexOf(PLACEHOLDER)
    if (start < 0) return out + rest

    out += rest.slice(0, start)
    rest = rest.slice(start + PLACEHOLDER.length)

    const end = rest.indexOf(PLACEHOLDER)
    if (end < 0) {
      throw new RenderError(`模板里有一个没有收尾的 ${PLACEHOLDER}：${head(rest, 40)}`)
    }

    const key = rest.slice(0, end)
    rest = rest.slice(end + PLACEHOLDER.length)

    if (!(KNOWN_KEYS as readonly string[]).includes(key)) {
      throw new RenderError(
        `模板里有未定义的占位符 ${PLACEHOLDER}${key}${PLACEHOLDER}（认得的只有 ` +
          KNOWN_KEYS.map((k) => `${PLACEHOLDER}${k}${PLACEHOLDER}`).join(' / ') +
          '）',
      )
    }

    out += values[key as PlaceholderKey]
  }
}

/** 截断一段文本用于报错，避免把整个模板正文甩进错误信息里。 */
function head(s: string, n: number): string {
  return [...s].length <= n ? s : `${[...s].slice(0, n).join('')}…`
}

/** 模板里出现的全部占位符。给「模板与 KNOWN_KEYS 是否一致」这条测试用。 */
export function placeholderKeysIn(raw: string): string[] {
  const keys: string[] = []
  let rest = raw
  for (;;) {
    const start = rest.indexOf(PLACEHOLDER)
    if (start < 0) return keys
    rest = rest.slice(start + PLACEHOLDER.length)
    const end = rest.indexOf(PLACEHOLDER)
    if (end < 0) return keys
    keys.push(rest.slice(0, end))
    rest = rest.slice(end + PLACEHOLDER.length)
  }
}
