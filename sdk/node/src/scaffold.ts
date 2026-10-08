import fs from 'node:fs/promises'
import { existsSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

import { validPluginName } from './rules.ts'
import { RenderError, render, type RenderValues } from './template.ts'

/** SDK 包的根目录（`src/` 的上一层）。 */
export const PACKAGE_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')

/** 模板目录。工程文件里的 `.tmpl` 后缀会在生成时去掉。 */
export const TEMPLATE_DIR = path.join(PACKAGE_ROOT, 'templates', 'plugin')

/** 模板文件的后缀。`.gitignore.tmpl` → `.gitignore`。 */
export const TEMPLATE_SUFFIX = '.tmpl'

/**
 * 随生成工程一起下发的 SDK 源码里，**不**拷过去的目录。
 *
 * 这份清单在中台侧（`crates/hub-templates/build.rs` 的 `SDK_SOURCES`）有一份对应的，
 * 两处必须一致——不一致的后果不是「多带或少带一个文件」那么轻：`node_modules` 被带进去
 * 时，包里会多出几十兆与平台绑定的二进制，而开发者的 `npm install` 又不认它。
 * Go 侧的同名清单是 `sdk/go/cmd/hub-plugin/sdkfiles.go` 的 `sdkExcluded`。
 *
 * - `node_modules` —— 依赖由开发者自己的 npm 解析（见 README 的镜像说明）
 * - `.git` —— 版本库元数据
 * - `templates` —— 生成工程用不到模板（它自己就是从模板生成的）
 */
export const SDK_COPY_EXCLUDED = ['node_modules', '.git', 'templates']

/** 生成一个工程所需的全部输入。 */
export interface ScaffoldOptions {
  /** 插件名，同时进 manifest.name 与包名。 */
  name: string
  /** 生成到哪个目录（不存在则创建）。 */
  outDir: string
  /** 覆盖包名，缺省用 `name`。 */
  packageName?: string
  /** SDK 在生成工程里的引用路径，缺省 `@example/hubkit`。 */
  sdkModule?: string
  /** SDK 源码在包内的相对路径，缺省 `./sdk`。 */
  sdkPath?: string
  /** 中台插件面地址，缺省 `http://127.0.0.1:8093`（本地 mock/中台的插件面）。 */
  hubAddr?: string
  /** 接入指南共通层正文。不给则自动去找（见 {@link resolveOnboarding}）。 */
  onboarding?: string
  /** 是否把 SDK 源码拷进 `<outDir>/sdk`，缺省 true。 */
  copySdk?: boolean
}

/** 与中台侧 `RenderValues` 同源的一组取值。 */
export function scaffoldValues(opts: ScaffoldOptions, onboarding: string): RenderValues {
  return {
    name: opts.name,
    package: opts.packageName ?? opts.name,
    sdk_module: opts.sdkModule ?? '@example/hubkit',
    sdk_path: opts.sdkPath ?? './sdk',
    hub_addr: opts.hubAddr ?? 'http://127.0.0.1:8093',
    onboarding,
  }
}

/**
 * 渲染模板目录，生成一个可运行的插件工程。
 *
 * 返回写出的文件（相对 `outDir`，已去 `.tmpl` 后缀），供 CLI 打印与测试断言。
 */
export async function scaffold(opts: ScaffoldOptions): Promise<string[]> {
  if (!validPluginName(opts.name)) {
    // 早失败：名字不合法的话中台注册时必拒（`MANIFEST_INVALID`），而那时工程
    // 已经生成好了、依赖也装完了，改名的代价远大于现在
    throw new Error(
      `插件名 ${JSON.stringify(opts.name)} 不合法：只允许字母数字与 -_，最长 64 字符，且以字母数字开头`,
    )
  }

  const onboarding = opts.onboarding ?? (await resolveOnboarding())
  const values = scaffoldValues(opts, onboarding)

  await fs.mkdir(opts.outDir, { recursive: true })

  const written: string[] = []
  for (const relative of await listTemplateFiles()) {
    const raw = await fs.readFile(path.join(TEMPLATE_DIR, relative), 'utf8')
    const output = relative.endsWith(TEMPLATE_SUFFIX)
      ? relative.slice(0, -TEMPLATE_SUFFIX.length)
      : relative

    const target = path.join(opts.outDir, output)
    ensureInside(opts.outDir, target)
    await fs.mkdir(path.dirname(target), { recursive: true })
    await fs.writeFile(target, render(raw, values), 'utf8')
    written.push(output)
  }

  if (opts.copySdk !== false) {
    await copySdk(path.join(opts.outDir, 'sdk'))
  }

  return written.sort()
}

/** 模板目录下的全部文件，相对路径、已排序。 */
export async function listTemplateFiles(dir = TEMPLATE_DIR): Promise<string[]> {
  const out: string[] = []

  const walk = async (current: string, prefix: string): Promise<void> => {
    const entries = await fs.readdir(current, { withFileTypes: true })
    for (const entry of entries) {
      const relative = prefix ? `${prefix}/${entry.name}` : entry.name
      if (entry.isDirectory()) await walk(path.join(current, entry.name), relative)
      else out.push(relative)
    }
  }

  await walk(dir, '')
  return out.sort()
}

/**
 * 把 SDK 源码拷进生成工程的 `sdk/`。
 *
 * 生成的工程用 `"@example/hubkit": "file:./sdk"` 引用 SDK，而开发者的机器上**没有**
 * 这份源码（内网没有私有 npm 源，包名也发布不出去）。带上源码，`npm install` 才能
 * 在离线环境里把它连成一个可解析的依赖。
 */
export async function copySdk(dest: string): Promise<void> {
  await fs.rm(dest, { recursive: true, force: true })
  await copyTree(PACKAGE_ROOT, dest, SDK_COPY_EXCLUDED)
}

async function copyTree(from: string, to: string, excluded: string[]): Promise<void> {
  const entries = await fs.readdir(from, { withFileTypes: true })
  await fs.mkdir(to, { recursive: true })

  for (const entry of entries) {
    // 排除项按**路径前缀**判，不能只看第一段——只看第一段时 `a/b` 这种会漏掉，
    // 而漏掉的后果是包里悄悄多出几十兆（Go 侧踩过同一个坑，见 build.rs 的注释）
    if (excluded.includes(entry.name)) continue

    const source = path.join(from, entry.name)
    const target = path.join(to, path.basename(entry.name))
    ensureInside(to, target)
    if (entry.isDirectory()) await copyTree(source, target, [])
    else await fs.copyFile(source, target)
  }
}

/**
 * 写路径的统一边界：目标必须落在 `root` 之内。
 *
 * 调用点的拼段今天都来自受控来源（readdir 条目、模板目录遍历），但这条边界
 * 不依赖那个假设——任何一段将来改成外部输入时，越界在这里炸而不是写穿目标根。
 */
function ensureInside(root: string, target: string): void {
  const rel = path.relative(path.resolve(root), path.resolve(target))
  if (rel.startsWith('..') || path.isAbsolute(rel)) {
    throw new Error(`写路径越界：${target} 不在 ${root} 之内`)
  }
}

/**
 * 找到接入指南的共通层正文。
 *
 * 它由中台侧统一维护（改一处六处生效），眼下寄居在 Go 的模板目录下
 * （`sdk/go/cmd/hub-plugin/templates/_shared/onboarding.md`）——因为 Go 的
 * `//go:embed` 不能向上取父目录，放到 `sdk/shared/` 就得为 Go 侧另加一套生成与
 * 校验脚本。中台（Rust）读任何路径都不受限制，所以位置迁就了 Go 一侧。
 *
 * ⚠️ 这是一处**对仓库布局的依赖**，而生成工程可能被拷到仓库之外。找不到时**报错**
 * 而不是给一段空章节：空章节会在生成的 AGENTS.md 里留下一个没有内容的「四道关」，
 * 那种缺陷要靠人逐字读产物才会发现（中台侧有一条测试专门守着这件事）。
 */
export async function resolveOnboarding(): Promise<string> {
  const candidates = onboardingCandidates()
  for (const candidate of candidates) {
    if (existsSync(candidate)) return await fs.readFile(candidate, 'utf8')
  }
  throw new Error(
    '找不到接入指南共通层（`_shared/onboarding.md`）。\n' +
      '它随 SDK 源码下发；若你是从打包产物里跑的，请显式指定：\n' +
      '  --onboarding <path/to/onboarding.md>   或   export HUB_ONBOARDING_FILE=<path>\n' +
      '找过这些位置：\n' +
      candidates.map((c) => `  - ${c}`).join('\n'),
  )
}

function onboardingCandidates(): string[] {
  const fromEnv = process.env.HUB_ONBOARDING_FILE
  return [
    ...(fromEnv ? [fromEnv] : []),
    // 现状：寄居在 Go 的模板目录下
    path.join(PACKAGE_ROOT, '..', 'go', 'cmd', 'hub-plugin', 'templates', '_shared', 'onboarding.md'),
    // 将来若挪到各语言共用的位置，这两条就不用改了
    path.join(PACKAGE_ROOT, '..', 'shared', 'onboarding.md'),
    path.join(PACKAGE_ROOT, 'templates', '_shared', 'onboarding.md'),
  ]
}

export { RenderError }
