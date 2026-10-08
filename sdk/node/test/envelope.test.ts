import assert from 'node:assert/strict'
import { test } from 'node:test'

import {
  STRUCT_FQ_NAME,
  STRUCT_TYPE_URL,
  budgetOf,
  emptyEnvelope,
  expired,
  invalid,
  isWellKnownFQName,
  issue,
  newEnvelope,
  newULID,
  payloadJSON,
  structContract,
  valid,
  withPayloadJSON,
} from '../src/index.ts'
import type { Envelope, JsonObject } from '../src/index.ts'

function envWith(payload: JsonObject): Envelope {
  return withPayloadJSON(emptyEnvelope(), payload)
}

test('JSON 载荷原样往返', () => {
  // 覆盖 Struct 的每一个分支：null / number / string / bool / struct / list
  const original: JsonObject = {
    text: '你好',
    length: 6,
    flag: true,
    nothing: null,
    list: [1, 'a', false, null, { deep: 2 }],
    nested: { a: { b: 'c' } },
    empty: '',
    zero: 0,
    negative: -1.5,
  }

  const [out, ok] = payloadJSON(envWith(original))
  assert.ok(ok)
  assert.deepEqual(out, original)
})

test('载荷不是 Struct 时如实说不认识', () => {
  const env = emptyEnvelope()
  assert.deepEqual(payloadJSON(env), [undefined, false])

  // 别的插件在 flow 里传的业务类型：type_url 不是 Struct，插件应改按自己的类型解码
  const other: Envelope = { ...env, payload: { typeUrl: 'type.googleapis.com/wms.v1.Order', value: new Uint8Array([1]) } }
  assert.deepEqual(payloadJSON(other), [undefined, false])
})

test('装载荷不改原信封', () => {
  const env = envWith({ a: 1 })
  const out = withPayloadJSON(env, { b: 2 })

  assert.notEqual(out, env)
  assert.deepEqual(payloadJSON(env)[0], { a: 1 }, '原信封不该被改动——链路里可能有别的持有者')
  assert.deepEqual(payloadJSON(out)[0], { b: 2 })
})

test('类型常量与 manifest 声明', () => {
  assert.equal(STRUCT_TYPE_URL, 'type.googleapis.com/google.protobuf.Struct')
  assert.equal(STRUCT_FQ_NAME, 'google.protobuf.Struct')
  assert.deepEqual(structContract(), {
    fqName: 'google.protobuf.Struct',
    description: '直接调用的 JSON 载荷',
  })

  // 中台对 well-known 类型豁免「声明必须出现在自己的 descriptor 里」
  assert.equal(isWellKnownFQName('google.protobuf.Struct'), true)
  assert.equal(isWellKnownFQName('google.protobuf.Timestamp'), true)
  assert.equal(isWellKnownFQName('wms.v1.OrderCreated'), false)
})

test('deadline 与剩余预算', () => {
  const env = emptyEnvelope()
  assert.deepEqual(budgetOf(env), [0, false], '没带 deadline 时 ok=false，而不是"剩 0"')
  assert.equal(expired(env), false)

  const future: Envelope = { ...env, deadlineMs: Date.now() + 30_000 }
  const [budget, ok] = budgetOf(future)
  assert.ok(ok)
  assert.ok(budget > 29_000 && budget <= 30_000, `预算应接近 30s，实际 ${budget}`)
  assert.equal(expired(future), false)

  // 逐跳递减的意义：过期了要能看出来，且剩余预算被夹到 0 而不是负数
  const past: Envelope = { ...env, deadlineMs: Date.now() - 1 }
  assert.deepEqual(budgetOf(past), [0, true])
  assert.equal(expired(past), true)
})

test('校验响应的构造', () => {
  assert.deepEqual(valid(), { valid: true, issues: [] })

  const bad = invalid(issue('payload.text', '缺少必填字段 text'))
  assert.equal(bad.valid, false)
  assert.equal(bad.issues[0]?.severity, 'SEVERITY_ERROR')
  assert.equal(bad.issues[0]?.path, 'payload.text')
})

test('newULID 形状与唯一性', () => {
  // 把 ULID 的对外承诺钉住：26 字符、Crockford 字母表、时间前缀单调、连续生成不重复。
  //
  // 单调性只对**前 10 个字符**（48 位毫秒时间戳）作保：同一毫秒内生成多个 id 时，
  // 高位相同、低位是各自的随机数，整体字符串序不保证递增——这是 ULID 的定义，
  // 不是实现的毛病，中台侧的 ulid crate 同样如此。
  const ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'
  const seen = new Set<string>()
  let prev = ''
  for (let i = 0; i < 1000; i++) {
    const id = newULID()
    assert.equal(id.length, 26, `ULID 应为 26 字符，实际 ${id.length}`)
    for (const ch of id) {
      assert.ok(ALPHABET.includes(ch), `字符 ${ch} 不在 Crockford 字母表里: ${id}`)
    }
    if (prev) assert.ok(id.slice(0, 10) >= prev.slice(0, 10), `时间前缀应单调递增: ${id} < ${prev}`)
    assert.ok(!seen.has(id), `连续生成出现重复 id: ${id}`)
    seen.add(id)
    prev = id
  }
})

test('newEnvelope 给全新 id', () => {
  const env = newEnvelope()
  assert.ok(env.messageId, 'message_id 应生成')
  assert.ok(env.traceId, 'trace_id 应生成')
  assert.notEqual(env.messageId, env.traceId, 'message_id 与 trace_id 不该是同一个值')
})
