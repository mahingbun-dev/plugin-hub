import assert from 'node:assert/strict'
import { test } from 'node:test'

import * as grpc from '@grpc/grpc-js'

import { PublishRejected } from '../src/state.ts'
import { STATE_TOKEN_METADATA } from '../src/proto.ts'
import { MockHub } from '../src/mockhub.ts'
import { StateClient, newHubStateClient } from '../src/state.ts'
import { emptyEnvelope } from '../src/index.ts'
import type { HubStateClient, PublishRequest, PublishResponse } from '../src/index.ts'

function clientFor(hub: MockHub, onDenied: () => void = () => {}): StateClient {
  return new StateClient(
    newHubStateClient(hub.target, grpc.credentials.createInsecure()),
    onDenied,
    // 测试里不设上限：真出问题时有测试运行器的超时兜着
    0,
  )
}

test('凭证随注册下发，键值读写走同一套规则', async () => {
  const hub = await MockHub.start()
  const state = clientFor(hub)
  state.setToken(hub.stateToken)

  try {
    assert.deepEqual(await state.get('orders', 'SO-1'), [undefined, false], '键不存在时 found=false')

    await state.put('orders', 'SO-1', Buffer.from('{"id":"SO-1"}'))
    const [value, found] = await state.get('orders', 'SO-1')
    assert.equal(found, true)
    assert.equal(Buffer.from(value!).toString(), '{"id":"SO-1"}')

    // 空字节与"键不存在"是两回事
    await state.put('orders', 'empty', new Uint8Array(0))
    const [empty, emptyFound] = await state.get('orders', 'empty')
    assert.equal(emptyFound, true)
    assert.equal(empty!.length, 0)

    assert.equal(await state.delete('orders', 'SO-1'), true)
    // 删不存在的键返回 false，不是错误：调用方的意图（这键没了）已达成
    assert.equal(await state.delete('orders', 'SO-1'), false)

    await state.put('orders', 'k1', Buffer.from('1'))
    await state.put('orders', 'k2', Buffer.from('2'))
    await state.put('other', 'k3', Buffer.from('3'))
    const entries = await state.scan('orders', '', 10)
    assert.deepEqual(entries.map((e) => e.key).sort(), ['empty', 'k1', 'k2'])
  } finally {
    await hub.close()
  }
})

test('TTL 的粒度是秒', async () => {
  const hub = await MockHub.start()
  const state = clientFor(hub)
  state.setToken(hub.stateToken)

  try {
    await state.put('t', 'k', Buffer.from('v'), 500)
    // 不足 1 秒的 ttl 会被截断成 0 —— 也就是**永不过期**，与 Go 侧行为一致
    assert.equal((await state.get('t', 'k'))[1], true)

    await state.put('e', 'k', Buffer.from('v'), 1000)
    assert.equal((await state.get('e', 'k'))[1], true)
    await new Promise((resolve) => setTimeout(resolve, 1200))
    assert.equal((await state.get('e', 'k'))[1], false, '过期的键不该读得到')
  } finally {
    await hub.close()
  }
})

test('没有凭证时被拒，并且只在 401 时叫醒注册循环', async () => {
  const hub = await MockHub.start()
  let denied = 0
  const state = clientFor(hub, () => {
    denied += 1
  })

  try {
    // 没设凭证
    await assert.rejects(
      () => state.get('orders', 'SO-1'),
      (err: grpc.ServiceError) => err.code === grpc.status.UNAUTHENTICATED,
    )
    assert.equal(denied, 1, '401 应当叫醒注册循环去换新凭证')

    state.setToken('wrong-token')
    await assert.rejects(() => state.get('orders', 'SO-1'), /状态凭证无效或已失效/)
    assert.equal(denied, 2)

    // 参数非法不是凭证问题：重注册解决不了，反而会让注册循环空转
    state.setToken(hub.stateToken)
    await assert.rejects(
      () => state.get('bad namespace', 'k'),
      (err: grpc.ServiceError) => err.code === grpc.status.INVALID_ARGUMENT,
    )
    assert.equal(denied, 2, '非 401 的错误不该触发重注册')

    // scan 的 limit 是硬要求：0 会被中台拒
    await assert.rejects(
      () => state.scan('orders', '', 0),
      (err: grpc.ServiceError) => err.code === grpc.status.INVALID_ARGUMENT,
    )
    assert.equal(denied, 2)
  } finally {
    await hub.close()
  }
})

test('凭证被吊销（中台一律回 401）时每次调用都会叫醒注册循环', async () => {
  const hub = await MockHub.start({ denyStateToken: true })
  let denied = 0
  const state = clientFor(hub, () => {
    denied += 1
  })
  // 凭证本身照常下发，模拟"中台已吊销但旧凭证还在插件手里"
  state.setToken(hub.stateToken)

  try {
    await assert.rejects(() => state.get('orders', 'k'), (err: grpc.ServiceError) => {
      assert.equal(err.code, grpc.status.UNAUTHENTICATED)
      return true
    })
    assert.equal(denied, 1)
  } finally {
    await hub.close()
  }
})

test('凭证的 metadata 键与中台一致', () => {
  // 写错的话中台判成"无凭证"，而插件侧只看到一个 401，很难查
  assert.equal(STATE_TOKEN_METADATA, 'x-hub-state-token')
})

/**
 * 只为 Publish 封装服务的假客户端：记录最近一次请求与凭证，按预设应答。
 *
 * 用它而不是 mockhub：Publish 的封装逻辑（请求字段、凭证 metadata、accepted=false
 * 的 reason 暴露）是客户端自己的事，mock 中台的 Publish 与真中台一样是
 * Unimplemented（防环与限流是独立课题），在它上面验不出封装行为。
 */
function recordingStateClient(resp: PublishResponse, err: grpc.ServiceError | null = null): {
  client: HubStateClient
  lastPublish: () => { req: PublishRequest; token: string } | undefined
} {
  let last: { req: PublishRequest; token: string } | undefined
  const fake = {
    Publish: (req: PublishRequest, md: grpc.Metadata, _opts: grpc.CallOptions, cb: (e: grpc.ServiceError | null, r: PublishResponse) => void) => {
      last = { req, token: String(md.get(STATE_TOKEN_METADATA)[0] ?? '') }
      cb(err, resp)
      return {} as grpc.ClientUnaryCall
    },
  }
  return {
    client: fake as unknown as HubStateClient,
    lastPublish: () => last,
  }
}

test('publish 受理时返回 run_id，凭证走 metadata', async () => {
  const fake = recordingStateClient({ accepted: true, runId: 'run-9', reason: '' })
  const state = new StateClient(fake.client, () => {}, 0)
  state.setToken('tok-1')

  const env = emptyEnvelope()
  env.messageId = 'm-1'
  const runId = await state.publish('order.flow', env)
  assert.equal(runId, 'run-9')

  const sent = fake.lastPublish()
  assert.equal(sent?.req.target, 'order.flow')
  assert.equal(sent?.req.envelope?.messageId, 'm-1')
  // 凭证必须走 x-hub-state-token metadata：写错的话中台判成「无凭证」
  assert.equal(sent?.token, 'tok-1')
})

test('publish 未受理（accepted=false）抛 PublishRejected 且 reason 随异常携带', async () => {
  // 未受理是**业务结果**而不是网络故障：防环、超限、目标 flow 不存在都属于此类，
  // reason 要原样到达调用方——改逻辑或换目标，而不是退避重试
  const fake = recordingStateClient({
    accepted: false,
    runId: '',
    reason: '检测到环：order.flow 已在本次触发链上（order.flow）',
  })
  const state = new StateClient(fake.client, () => {}, 0)

  await assert.rejects(
    () => state.publish('order.flow', emptyEnvelope()),
    (err: unknown) => {
      assert.ok(err instanceof PublishRejected, `应映射为 PublishRejected，实际 ${String(err)}`)
      assert.ok((err as PublishRejected).reason.includes('检测到环'))
      return true
    },
  )
})

test('mock 中台的 Publish 与真中台一致：一律 Unimplemented，且不看凭证', async () => {
  const hub = await MockHub.start()
  try {
    const client = newHubStateClient(hub.target, grpc.credentials.createInsecure())
    const call = (md: grpc.Metadata): Promise<grpc.ServiceError> =>
      new Promise((resolve, reject) => {
        client.Publish({ target: 'flow', envelope: null }, md, {}, (err) =>
          err ? resolve(err) : reject(new Error('应报错')),
        )
      })

    // mock 若先查凭证，插件就会照着「不带凭证的 Publish 会先撞 401」去写，
    // 上真环境才发现不是——所以带不带凭证都必须是同一个 Unimplemented
    assert.equal((await call(metadataWith(hub.stateToken))).code, grpc.status.UNIMPLEMENTED)
    assert.equal((await call(new grpc.Metadata())).code, grpc.status.UNIMPLEMENTED)
  } finally {
    await hub.close()
  }
})

function metadataWith(token: string): grpc.Metadata {
  const md = new grpc.Metadata()
  md.set(STATE_TOKEN_METADATA, token)
  return md
}
