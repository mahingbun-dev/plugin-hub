import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import path from 'node:path'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'

import { validPluginName, validStateSegment } from '../src/rules.ts'

/**
 * 跨语言规则的一致性用例。
 *
 * 事实来源是 `sdk/go/hubkit/testdata/hub-rules.json`——Rust 侧
 * （`crates/hub-grpc/tests/state_rules.rs`）与 Go 侧（`sdk/go/hubkit/rules_test.go`）
 * 读的也是同一份文件。**本文件不自己写用例**：手写的期望值迟早会与那份规格漂移，
 * 而漂移的后果是「插件以为写进去了、中台拒绝」这类最难查的静默失败。
 */
const RULES_FILE = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  'go',
  'hubkit',
  'testdata',
  'hub-rules.json',
)

interface RulesFile {
  stateTokenMetadata: string
  cases: { rule: string; input: string; valid: boolean }[]
}

test('规则判定与跨语言契约文件逐条一致', (t) => {
  if (!existsSync(RULES_FILE)) {
    // 打包下发的 SDK 里只有本语言那份，找不到 Go 的 testdata 是正常的。
    // 但**跳过而不是假装通过**：跳过会被测试运行器报出来（skipped），
    // 而假装通过会让这个文件变成一条永远为真的断言
    t.skip(`找不到跨语言契约文件 ${RULES_FILE}（打包下发的 SDK 里可能没有）`)
    return
  }

  const rules = JSON.parse(readFileSync(RULES_FILE, 'utf8')) as RulesFile
  assert.ok(rules.cases.length > 0, '契约文件里一条用例都没有')

  const implement: Record<string, (input: string) => boolean> = {
    stateSegment: validStateSegment,
    pluginName: validPluginName,
  }

  for (const item of rules.cases) {
    const fn = implement[item.rule]
    assert.ok(fn, `契约文件里出现了本 SDK 不认识的规则名 ${item.rule}`)
    assert.equal(
      fn(item.input),
      item.valid,
      `${item.rule}(${JSON.stringify(item.input)}) 应为 ${item.valid}`,
    )
  }

  // 状态凭证的 metadata 键也必须三侧一致：写错的话中台判成"无凭证"，
  // 而插件侧只看到一个 401
  assert.equal(rules.stateTokenMetadata, 'x-hub-state-token')
})

test('状态段的边界与字符集', () => {
  assert.equal(validStateSegment(''), false)
  assert.equal(validStateSegment('s'), true)
  assert.equal(validStateSegment('a'.repeat(200)), true)
  assert.equal(validStateSegment('a'.repeat(201)), false)
  // 放行 `*` 是漏洞不是功能：前缀靠字符串拼接，通配符会让 KvScan 变成跨命名空间的模式匹配
  assert.equal(validStateSegment('a*'), false)
  assert.equal(validStateSegment('a:b'), false)
})

test('插件名不允许以 - 或 _ 开头', () => {
  assert.equal(validPluginName('-lead'), false)
  assert.equal(validPluginName('_lead'), false)
  assert.equal(validPluginName('a-'), true)
  assert.equal(validPluginName('a'.repeat(64)), true)
  assert.equal(validPluginName('a'.repeat(65)), false)
})
