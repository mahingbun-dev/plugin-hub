import assert from 'node:assert/strict'
import { test } from 'node:test'

import { emptyDescriptor, invalid, issue, payloadJSON, valid, withPayloadJSON } from '../src/index.ts'
import type { Envelope, Plugin } from '../src/index.ts'
import { Logger } from '../src/log.ts'
import { PluginClient, local, runtime } from '../src/conformance.ts'
import { MockHub, freeAddr } from '../src/mockhub.ts'
import { run } from '../src/run.ts'

function goodPlugin(): Plugin {
  return {
    manifest: () => ({
      name: 'order-reader',
      version: '1.0.0',
      consumes: [{ fqName: 'google.protobuf.Struct', description: 'JSON' }],
      tools: [{ name: 'echo', description: '回显' }],
    }),
    descriptor: () => emptyDescriptor(),
    validate: (_ctx, env: Envelope) => {
      const [payload, ok] = payloadJSON(env)
      if (!ok) return invalid(issue('payload', '需要 JSON 对象载荷'))
      if (typeof payload?.['text'] !== 'string') return invalid(issue('payload.text', '缺少'))
      return valid()
    },
    handle: (_ctx, env) => withPayloadJSON(env, { ok: true }),
  }
}

test('自洽的插件六项全过', () => {
  const report = local(goodPlugin())
  assert.ok(report.passed(), report.toString())
  assert.deepEqual(
    report.checks.map((c) => c.name),
    ['manifest 存在', '插件名合法', '版本号存在', 'descriptor 可用', '声明的类型都在 descriptor 中', '工具声明合法'],
  )
  assert.equal(report.subject, 'order-reader@1.0.0')
})

test('manifest 缺失直接判失败', () => {
  const report = local({ ...goodPlugin(), manifest: () => undefined as never })
  assert.equal(report.passed(), false)
  assert.equal(report.subject, '（未命名插件）')
  assert.deepEqual(report.failures()[0]?.name, 'manifest 存在')
})

test('插件名不合法', () => {
  const report = local({ ...goodPlugin(), manifest: () => ({ name: '中文名', version: '1.0.0' }) })
  assert.equal(report.passed(), false)
  assert.deepEqual(report.failures()[0]?.name, '插件名合法')
})

test('声明了自有类型却没提供 descriptor', () => {
  const report = local({
    ...goodPlugin(),
    manifest: () => ({
      name: 'order-reader',
      version: '1.0.0',
      consumes: [{ fqName: 'wms.v1.OrderCreated', description: '自有类型' }],
    }),
  })
  assert.equal(report.passed(), false)
  // 中台也会拒，但那是网络往返之后的事
  assert.match(report.toString(), /声明的类型都在 descriptor 中/)
  assert.match(report.toString(), /wms\.v1\.OrderCreated/)
})

test('一条契约都没声明', () => {
  const report = local({
    ...goodPlugin(),
    manifest: () => ({ name: 'order-reader', version: '1.0.0' }),
  })
  assert.equal(report.passed(), false)
  assert.match(report.toString(), /声明了契约/)
})

test('工具名非法与重名', () => {
  const illegal = local({
    ...goodPlugin(),
    manifest: () => ({
      name: 'order-reader',
      version: '1.0.0',
      consumes: [{ fqName: 'google.protobuf.Struct', description: 'JSON' }],
      tools: [{ name: 'echo.now', description: '带点' }],
    }),
  })
  assert.match(illegal.toString(), /工具名 "echo\.now" 含非法字符/)

  const duplicated = local({
    ...goodPlugin(),
    manifest: () => ({
      name: 'order-reader',
      version: '1.0.0',
      consumes: [{ fqName: 'google.protobuf.Struct', description: 'JSON' }],
      tools: [
        { name: 'echo', description: 'a' },
        { name: 'echo', description: 'b' },
      ],
    }),
  })
  assert.match(duplicated.toString(), /重复声明/)
})

test('不声明工具是合法的', () => {
  const report = local({
    ...goodPlugin(),
    manifest: () => ({
      name: 'order-reader',
      version: '1.0.0',
      consumes: [{ fqName: 'google.protobuf.Struct', description: 'JSON' }],
    }),
  })
  assert.ok(report.passed(), report.toString())
  assert.match(report.toString(), /未声明工具（仅参与 flow）/)
})

test('运行时自检：对着跑起来的插件五项全过', async () => {
  const addr = await freeAddr()
  const hub = await MockHub.start({ heartbeatIntervalSeconds: 5 })
  const controller = new AbortController()
  const done = run(
    goodPlugin(),
    {
      hubAddr: hub.addr,
      advertiseAddr: `http://${addr}`,
      listenAddr: addr,
      instanceId: 'conformance-1',
      logger: Logger.silent(),
    },
    { signal: controller.signal },
  )

  try {
    await hub.waitForRegistration(1)

    const report = await runtime(`http://${addr}`)
    assert.ok(report.passed(), report.toString())
    assert.deepEqual(report.checks.map((c) => c.name), [
      'Health 可应答',
      'Describe 可应答',
      '校验器对空信封不崩',
      '校验器可处理 JSON 载荷',
      '插件体返回信封',
    ])
    // 探针载荷本来就不满足业务规则，拒绝是合法结果
    assert.match(report.toString(), /校验器可处理 JSON 载荷 —— 拒绝（1 条问题）/)
  } finally {
    controller.abort()
    await done
    await hub.close()
  }
})

test('运行时自检：端口上没人听时报在 Health 那一行', async () => {
  // 惰性连接：只登记地址不发包，所以"没人听"要等第一次真正的 RPC 才浮出来
  const addr = await freeAddr()
  const report = await runtime(`http://${addr}`)

  assert.equal(report.passed(), false)
  assert.equal(report.failures()[0]?.name, 'Health 可应答')
})

test('PluginClient 直连插件', async () => {
  const addr = await freeAddr()
  const hub = await MockHub.start({ heartbeatIntervalSeconds: 5 })
  const controller = new AbortController()
  const done = run(
    goodPlugin(),
    {
      hubAddr: hub.addr,
      advertiseAddr: `http://${addr}`,
      listenAddr: addr,
      instanceId: 'client-1',
      logger: Logger.silent(),
    },
    { signal: controller.signal },
  )

  const client = PluginClient.dial(`http://${addr}`)
  try {
    await hub.waitForRegistration(1)

    assert.equal((await client.health()).healthy, true)
    assert.equal((await client.describe()).name, 'order-reader')

    const out = await client.handle(withPayloadJSON({ ...emptyEnvelopeShape() }, { text: 'hi' }))
    assert.deepEqual(payloadJSON(out)[0], { ok: true })
  } finally {
    client.close()
    controller.abort()
    await done
    await hub.close()
  }
})

/** 空信封的形状（conformance 的客户端只要求能编解码）。 */
function emptyEnvelopeShape(): Envelope {
  return {
    messageId: '',
    traceId: '',
    spanId: '',
    flowId: '',
    runId: '',
    nodeId: '',
    tenant: '',
    subject: null,
    deadlineMs: 0,
    type: 'PAYLOAD_TYPE_UNSPECIFIED',
    payload: null,
    payloadRef: null,
    meta: {},
  }
}
