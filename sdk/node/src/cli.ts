#!/usr/bin/env node
// 脚手架 CLI。
//
//   hub-plugin new <name> [--dir <目录>] [--package <包名>] [--hub-addr <地址>]
//   hub-plugin conform <插件地址>
//
// `new` 渲染模板生成一个可运行的插件工程；`conform` 跑运行时契约自检
// （L2 那一关），直连插件、不需要中台在线。
//
// 两个子命令都在包内（`sdk/src/cli.ts`），所以生成出来的工程不必另外装东西：
//
//   node sdk/src/cli.ts conform http://127.0.0.1:9000

import { realpathSync } from 'node:fs'
import process from 'node:process'
import { pathToFileURL } from 'node:url'

// `runtime` **刻意不在这里静态 import**（它来自 `./conformance.ts`，而后者 import
// `@grpc/grpc-js`）。写成顶层静态 import 的话，`new` 这个纯脚手架子命令会被 `conform`
// 的运行时依赖连坐：模块加载期就要解析 `@grpc/grpc-js`，而它躺在 `node_modules` 里、
// 本包刻意不提交（见 sdk/node/.gitignore），于是**只要有 node、没有依赖，`new` 就起不来**。
//
// 代价不是理论上的：CI 的模板冒烟（scripts/smoke-template.sh）用
// `node sdk/node/src/cli.ts new` 渲染 node 那门模板，构建镜像里没有 npm 依赖源，
// 旧写法让那一门在渲染阶段就 ERR_MODULE_NOT_FOUND，整条 job 变红。
//
// 拆掉之后 `new` 的依赖闭包只剩 node 内建：scaffold.ts 只 import `node:*`，
// 它再往下用到的 rules.ts 与 template.ts **一个 import 都没有**。第三方包只出现在
// conformance / state / proto / mockhub / run 这几个运行时文件里，`conform` 才需要。
import { resolveOnboarding, scaffold, SDK_COPY_EXCLUDED } from './scaffold.ts'

const USAGE = `anc-hub 插件脚手架

用法：
  hub-plugin new <插件名> [选项]      生成一个可运行的插件工程
  hub-plugin conform <插件地址>       跑运行时契约自检（L2，不需要中台在线）

new 的选项：
  --dir <目录>          生成到哪个目录，缺省 ./<插件名>
  --package <包名>      npm 包名，缺省与插件名相同
  --hub-addr <地址>     写进 README / AGENTS.md 的中台插件面地址
                        缺省 http://127.0.0.1:8093
  --onboarding <文件>   接入指南共通层（一般不必给，会自动去找）
  --no-sdk              不把 SDK 源码拷进 <目录>/sdk

示例：
  hub-plugin new order-reader --dir /tmp/order-reader
  hub-plugin conform http://127.0.0.1:9000
`

async function main(argv: string[]): Promise<number> {
  const [command, ...rest] = argv

  switch (command) {
    case 'new':
      return await newCommand(rest)
    case 'conform':
      return await conformCommand(rest)
    case undefined:
    case '-h':
    case '--help':
    case 'help':
      process.stdout.write(USAGE)
      return 0
    default:
      process.stderr.write(`不认识的子命令 ${JSON.stringify(command)}\n\n${USAGE}`)
      return 2
  }
}

async function newCommand(args: string[]): Promise<number> {
  const flags = parseFlags(args)
  const name = flags.positional[0]
  if (!name) {
    process.stderr.write(`缺少插件名。\n\n${USAGE}`)
    return 2
  }

  const outDir = flags.options['dir'] ?? `./${name}`
  const onboardingFile = flags.options['onboarding']

  try {
    const written = await scaffold({
      name,
      outDir,
      packageName: flags.options['package'],
      hubAddr: flags.options['hub-addr'],
      onboarding: onboardingFile ? await readOnboardingFile(onboardingFile) : undefined,
      copySdk: !flags.switches.has('no-sdk'),
    })

    process.stdout.write(`已生成 ${name} → ${outDir}\n`)
    for (const file of written) process.stdout.write(`  ${file}\n`)
    if (!flags.switches.has('no-sdk')) {
      process.stdout.write(
        `  sdk/（SDK 源码，排除 ${SDK_COPY_EXCLUDED.join(' / ')}）\n`,
      )
    }
    process.stdout.write(`\n下一步：\n  cd ${outDir}\n  npm install\n  npm test\n`)
    return 0
  } catch (err) {
    process.stderr.write(`${err instanceof Error ? err.message : String(err)}\n`)
    return 1
  }
}

async function conformCommand(args: string[]): Promise<number> {
  const address = args.find((a) => !a.startsWith('-'))
  if (!address) {
    process.stderr.write(`缺少插件地址。\n\n${USAGE}`)
    return 2
  }

  // 到这里才加载运行时依赖——理由见文件头那段。写法与下面的 readOnboardingFile 一致。
  const { runtime } = await import('./conformance.ts')

  const report = await runtime(address)
  process.stdout.write(report.toString())
  return report.passed() ? 0 : 1
}

async function readOnboardingFile(file: string): Promise<string> {
  const fs = await import('node:fs/promises')
  return await fs.readFile(file, 'utf8')
}

/** 极简参数解析：`--key value`、`--switch`、以及位置参数。 */
function parseFlags(args: string[]): {
  positional: string[]
  options: Record<string, string>
  switches: Set<string>
} {
  const positional: string[] = []
  const options: Record<string, string> = {}
  const switches = new Set<string>()

  for (let i = 0; i < args.length; i++) {
    const arg = args[i]!
    if (!arg.startsWith('--')) {
      positional.push(arg)
      continue
    }
    const key = arg.slice(2)
    const next = args[i + 1]
    if (next === undefined || next.startsWith('--')) {
      switches.add(key)
      continue
    }
    options[key] = next
    i++
  }

  return { positional, options, switches }
}

/**
 * 本文件是不是被**直接执行**的入口（被 import 时不跑 main）。
 *
 * 比较前先 `realpathSync` 一下，这不是多余的：生成工程里的 SDK 是 `file:./sdk` 链接进
 * `node_modules/@example/hubkit` 的，走 `npx hub-plugin` 时 `process.argv[1]` 是**链接路径**
 * 而 `import.meta.url` 是**真实路径**，直接比会不相等——表现是「命令跑了、什么都没输出、
 * 退出码 0」，比报错难查得多。
 */
function isEntryPoint(): boolean {
  const invoked = process.argv[1]
  if (!invoked) return false
  try {
    return import.meta.url === pathToFileURL(realpathSync(invoked)).href
  } catch {
    return false
  }
}

if (isEntryPoint()) {
  process.exitCode = await main(process.argv.slice(2))
}

export { main, resolveOnboarding }
