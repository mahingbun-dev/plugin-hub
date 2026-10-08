import assert from 'node:assert/strict'
import { test } from 'node:test'

import * as grpc from '@grpc/grpc-js'

import { MockHub } from '../src/mockhub.ts'
import { hubServices } from '../src/proto.ts'

/**
 * mock 中台的注销校验。
 *
 * 这一层单独测，是因为它是**判据本身**：`run.test.ts` 里那些「注销之后行没了」的
 * 断言，只有在 mock 真的会拒绝错误凭证时才证明得了「插件带对了凭证」——mock 一旦
 * 比真中台宽松（谁来了都删），骨架退化成「不带凭证也照删」那些用例照样全绿。
 */

/** `hub.v1.PluginRegistry` 的客户端，把回调式调用收成 Promise。 */
function registryOf(hub: MockHub): {
  Register: (req: unknown) => Promise<{ stateToken: string }>
  Unregister: (req: unknown) => Promise<unknown>
} {
  const Ctor = hubServices().hub.v1.PluginRegistry as unknown as new (
    address: string,
    credentials: grpc.ChannelCredentials,
  ) => Record<
    string,
    (
      req: unknown,
      md: grpc.Metadata,
      opts: grpc.CallOptions,
      cb: (err: grpc.ServiceError | null, res: never) => void,
    ) => void
  >
  const client = new Ctor(hub.target, grpc.credentials.createInsecure())

  const call = (name: 'Register' | 'Unregister') => (req: unknown): Promise<never> =>
    new Promise<never>((resolve, reject) => {
      // 回调签名里的响应在测试侧只有 `Register` 用得上，统一在这里收成 Promise
      client[name]!(req, new grpc.Metadata(), {}, (err, res) => (err ? reject(err) : resolve(res)))
    })

  return { Register: call('Register'), Unregister: call('Unregister') }
}

/** 让一个实例进到 mock 的在册列表里，返回**注册响应里下发的**那份凭证。 */
async function registerInstance(hub: MockHub, instanceId: string): Promise<string> {
  const resp = await registryOf(hub).Register({
    pluginName: 'test-plugin',
    version: '0.1.0',
    instanceId,
    advertiseAddr: 'http://127.0.0.1:1',
    manifest: null,
    descriptorSet: new Uint8Array(0),
  })
  return resp.stateToken
}

function unregister(
  hub: MockHub,
  req: { instanceId: string; reason: string; stateToken: string },
): Promise<unknown> {
  return registryOf(hub).Unregister(req)
}

test('凭证为空或不符时不删任何行，带对的凭证才删', async () => {
  const hub = await MockHub.start()
  const token = await registerInstance(hub, 'owner-1')
  assert.equal(hub.instancesSnapshot().length, 1, '注册成功后应当在册')

  try {
    // 空凭证：旧版插件的路径，也正是「注册被拒的一方退出时删掉属主那一行」的路径
    await unregister(hub, { instanceId: 'owner-1', reason: '插件优雅退出', stateToken: '' })
    assert.equal(hub.instancesSnapshot().length, 1, '空凭证不该删掉任何行')

    // 不符：撞了 instanceId 的另一个插件手里是它自己那份凭证，或者凭证已被轮换
    await unregister(hub, { instanceId: 'owner-1', reason: '插件优雅退出', stateToken: 'not-mine' })
    assert.equal(hub.instancesSnapshot().length, 1, '凭证不符不该删掉任何行')

    // 对上了才删
    await unregister(hub, { instanceId: 'owner-1', reason: '插件优雅退出', stateToken: token })
    assert.equal(hub.instancesSnapshot().length, 0, '凭证正确时应当摘除实例行')

    // 三次请求都收到了（含被拒的）：被拒不是"没收到"，是"收到了但没删"
    assert.deepEqual(hub.unregisters, ['owner-1', 'owner-1', 'owner-1'])
    assert.deepEqual(hub.unregisterTokens, ['', 'not-mine', token])
    assert.deepEqual(hub.refusedUnregisters, ['owner-1', 'owner-1'])
  } finally {
    await hub.close()
  }
})

test('注销一个不在册的实例行：拒绝，且不牵连别的行', async () => {
  // 「另一行」才是重点：撞了 id 的插件注销时，真正会被误删的是**属主**那一行。
  // 这条用例把它摆在旁边，确保拒绝的那一次没有顺手删到它。
  const hub = await MockHub.start()
  const token = await registerInstance(hub, 'owner-1')

  try {
    await unregister(hub, { instanceId: 'ghost', reason: '插件优雅退出', stateToken: token })
    assert.deepEqual(hub.refusedUnregisters, ['ghost'])
    assert.equal(hub.instancesSnapshot().length, 1, '没这一行时不该删到别的行')
    assert.equal(hub.instancesSnapshot()[0]?.instanceId, 'owner-1')
  } finally {
    await hub.close()
  }
})
