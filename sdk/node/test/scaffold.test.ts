import assert from 'node:assert/strict'
import fs from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { test } from 'node:test'

import {
  SDK_COPY_EXCLUDED,
  TEMPLATE_DIR,
  TEMPLATE_SUFFIX,
  listTemplateFiles,
  scaffold,
  scaffoldValues,
  resolveOnboarding,
} from '../src/scaffold.ts'
import { KNOWN_KEYS, RenderError, placeholderKeysIn, render } from '../src/template.ts'

function values(): Parameters<typeof render>[1] {
  return {
    name: 'order-reader',
    package: '@example/order-reader',
    sdk_module: '@example/hubkit',
    sdk_path: './sdk',
    hub_addr: 'http://127.0.0.1:8093',
    onboarding: '（共通层正文）',
  }
}

test('已知占位符被替换，且同一个出现多次都替换', () => {
  const got = render('@@name@@-@@name@@-@@package@@', values())
  assert.equal(got, 'order-reader-order-reader-@example/order-reader')
})

test('没有占位符时原样返回', () => {
  const raw = 'export const x = 1\n'
  assert.equal(render(raw, values()), raw)
})

test('未定义的占位符报错而不是留空', () => {
  // 这一条是整套约定的核心：留空会生成一个要等到注册时才炸的工程
  assert.throws(() => render('name: @@plugin_name@@', values()), (err: Error) => {
    assert.ok(err instanceof RenderError)
    assert.match(err.message, /plugin_name/)
    return true
  })
})

test('没有收尾的占位符报错', () => {
  assert.throws(() => render('name: @@name', values()), RenderError)
})

test('模板里用到的占位符都在认得的清单里', async () => {
  // 模板里出现了清单外的占位符，要等用户点「生成」才会发现；这里提前扫一遍
  for (const relative of await listTemplateFiles()) {
    const raw = await fs.readFile(path.join(TEMPLATE_DIR, relative), 'utf8')
    for (const key of placeholderKeysIn(raw)) {
      assert.ok(
        (KNOWN_KEYS as readonly string[]).includes(key),
        `${relative} 里用了未定义的占位符 @@${key}@@`,
      )
    }
  }
})

test('生成的工程干净且可解析', async () => {
  const outDir = await fs.mkdtemp(path.join(os.tmpdir(), 'hubkit-scaffold-'))
  try {
    // 共通层正文显式给：这一条验的是渲染与拷贝，不该顺带依赖中台仓库的目录布局
    // （「默认去找」那条路径由下面的用例单独覆盖）
    const written = await scaffold({ name: 'order-reader', outDir, onboarding: '（共通层正文）' })

    // 文件名里的 .tmpl 后缀会去掉，需要前导点的文件会还原
    assert.deepEqual(written, [
      // `.dockerignore` 该出现在开发者手里：Docker 只认它、不认 .gitignore，而本工程的
      // Dockerfile 是 `COPY . .`——本地 `npm install` 出来的 node_modules 会被盖进镜像
      // （按 macOS/arm64 装的依赖进 linux/amd64 容器，原生模块直接不可用）。
      '.dockerignore',
      '.gitignore',
      'AGENTS.md',
      'Dockerfile',
      'README.md',
      'package.json',
      // M6 的互调/发现休眠演示：与 main/plugin 同在 src/ 下，tsc 的 include 一并检查到
      'src/gateway-example.ts',
      'src/main.ts',
      'src/plugin.ts',
      'test/plugin.test.ts',
      'tsconfig.json',
    ])

    for (const file of written) {
      const text = await fs.readFile(path.join(outDir, file), 'utf8')
      assert.ok(!text.includes('@@'), `${file} 里残留了未渲染的占位符`)
    }

    const pkg = JSON.parse(await fs.readFile(path.join(outDir, 'package.json'), 'utf8')) as {
      name: string
      dependencies: Record<string, string>
    }
    assert.equal(pkg.name, 'order-reader')
    // 生成的工程按包内相对路径引用 SDK：内网没有私有 npm 源，带上源码才装得上
    assert.equal(pkg.dependencies['@example/hubkit'], 'file:./sdk')

    // AGENTS.md 里的共通层挂载点要被真的展开，而不是留一个空章节
    const agents = await fs.readFile(path.join(outDir, 'AGENTS.md'), 'utf8')
    assert.match(agents, /（共通层正文）/)

    // SDK 源码按排除清单拷进 sdk/
    const sdkPackage = JSON.parse(await fs.readFile(path.join(outDir, 'sdk', 'package.json'), 'utf8')) as {
      name: string
    }
    assert.equal(sdkPackage.name, '@example/hubkit')
    for (const excluded of SDK_COPY_EXCLUDED) {
      await assert.rejects(
        () => fs.stat(path.join(outDir, 'sdk', excluded)),
        `${excluded} 不该被拷进生成工程`,
      )
    }
  } finally {
    await fs.rm(outDir, { recursive: true, force: true })
  }
})

test('插件名不合法时拒绝生成', async () => {
  const outDir = await fs.mkdtemp(path.join(os.tmpdir(), 'hubkit-scaffold-bad-'))
  try {
    // 名字不合法的话中台注册时必拒，而那时工程已经生成好、依赖也装完了
    await assert.rejects(() => scaffold({ name: '中文名', outDir }), /不合法/)
    await assert.rejects(() => scaffold({ name: '-lead', outDir }), /不合法/)
  } finally {
    await fs.rm(outDir, { recursive: true, force: true })
  }
})

test('取值表覆盖全部占位符', () => {
  const built = scaffoldValues({ name: 'x', outDir: '/tmp/x' }, '正文')
  assert.deepEqual(Object.keys(built).sort(), [...KNOWN_KEYS].sort())
  assert.equal(built.sdk_path, './sdk')
  assert.equal(built.sdk_module, '@example/hubkit')
})

test('模板文件都带 .tmpl 后缀', async () => {
  // build.rs 是按后缀去后缀名的，漏一个就会在产物里留下 .tmpl
  for (const relative of await listTemplateFiles()) {
    assert.ok(relative.endsWith(TEMPLATE_SUFFIX), `${relative} 没有 .tmpl 后缀`)
  }
})

test('共通层素材在本仓库里找得到', async () => {
  // 找不到时 CLI 会报错而不是给一个空章节——中台侧有一条测试专门守着这件事
  const onboarding = await resolveOnboarding()
  assert.ok(onboarding.trim().length > 0)
  assert.match(onboarding, /四道关/)
})
