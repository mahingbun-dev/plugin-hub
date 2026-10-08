import assert from 'node:assert/strict'
import { test } from 'node:test'

import { configFromEnv, emptyDescriptor, invalid, issue, payloadJSON, valid, withPayloadJSON } from '../src/index.ts'
import type { Plugin } from '../src/index.ts'
import { Logger } from '../src/log.ts'
import { MockHub } from '../src/mockhub.ts'
import { run } from '../src/run.ts'

/** 一个最小插件：接受 JSON 载荷里的 text，回显它。 */
function echoPlugin(): Plugin {
  return {
    manifest: () => ({
      name: 'test-plugin',
      version: '0.1.0',
      description: '测试用',
      owner: '测试',
      consumes: [{ fqName: 'google.protobuf.Struct', description: '直接调用的 JSON 载荷' }],
    }),
    descriptor: () => emptyDescriptor(),
    validate: (_ctx, env) => {
      const [payload, ok] = payloadJSON(env)
      if (!ok) return invalid(issue('payload', '需要 JSON 对象载荷'))
      if (typeof payload?.['text'] !== 'string') return invalid(issue('payload.text', '缺少必填字段 text'))
      return valid()
    },
    handle: (_ctx, env) => withPayloadJSON(env, { echo: 'ok' }),
  }
}

/**
 * 起一个插件，返回「停掉它」的句柄。
 *
 * 监听端口一律用 `127.0.0.1:0` 让系统分配——测试并行跑时固定端口必然撞车，
 * 而撞出来的失败信息（EADDRINUSE）与被测行为毫无关系。
 */
async function startPlugin(hub: MockHub, overrides: Record<string, unknown> = {}) {
  const controller = new AbortController()
  const done = run(
    echoPlugin(),
    {
      hubAddr: hub.addr,
      advertiseAddr: 'http://127.0.0.1:1',
      listenAddr: '127.0.0.1:0',
      instanceId: 'test-1',
      // 把重试间隔压到毫秒级：注册失败要重试的那条路径在测试里不该等 5 秒
      retryIntervalMs: 50,
      // 日志静音：插件自己的注册 / 心跳日志会淹掉断言失败的信息
      logger: Logger.silent(),
      ...overrides,
    },
    { signal: controller.signal },
  )

  return {
    stop: async () => {
      controller.abort()
      await done
    },
    done,
  }
}

test('注册成功后按中台指定的周期发心跳', async () => {
  const hub = await MockHub.start({ heartbeatIntervalSeconds: 1 })
  const plugin = await startPlugin(hub)

  try {
    await hub.waitForRegistration(1)
    assert.equal(hub.lastRegistration?.pluginName, 'test-plugin')
    assert.equal(hub.lastRegistration?.instanceId, 'test-1')
    // 只发本插件自己的 proto，不发自有的——空 descriptor 是合法的
    assert.equal(hub.lastRegistration?.descriptorSet.length, 0)

    await hub.waitForHeartbeats(2)
    assert.ok(hub.heartbeats >= 2, '应当按中台指定的周期持续发心跳')
  } finally {
    await plugin.stop()
    await hub.close()
  }
})

test('心跳回执要求重注册时重新走一遍注册流程（被摘除后自愈）', async () => {
  const hub = await MockHub.start({ heartbeatIntervalSeconds: 1 })
  const plugin = await startPlugin(hub)

  try {
    await hub.waitForRegistration(1)
    assert.equal(hub.instancesSnapshot().length, 1)

    // 模拟中台的心跳超时巡检把实例摘了：插件并不知道，照常发心跳
    hub.forgetInstance('test-1')

    // 下一拍心跳会拿到 reregisterRequired，插件据此重走注册流程
    await hub.waitForRegistration(2, 5_000)

    // 自愈的判据是**实例真的回到册上**，不是"又发了一次注册请求"——
    // 注册被拒时也会发请求，而那时实例不会回来
    await hub.waitUntilInstanceBack('test-1', 5_000)
    assert.equal(hub.instancesSnapshot().length, 1)
  } finally {
    await plugin.stop()
    await hub.close()
  }
})

test('注册被拒时一直重试，直到中台接受', async () => {
  let attempts = 0
  const hub = await MockHub.start({
    heartbeatIntervalSeconds: 1,
    rejectRegister: () => {
      attempts += 1
      // 前两次拒，之后放行：验证的是「拒绝不是终点」，而不是拒绝的措辞
      return attempts <= 2
        ? [{ code: 'REJECT_CODE_UNREACHABLE', message: '中台拨不通你自报的地址', detail: '检查 HUB_ADVERTISE_ADDR' }]
        : []
    },
  })
  const plugin = await startPlugin(hub)

  try {
    await hub.waitForRegistration(3, 5_000)
    assert.equal(hub.rejections.length, 2, '前两次的拒绝原因应当被原样送回插件')
    assert.equal(hub.rejections[0]?.code, 'REJECT_CODE_UNREACHABLE')
  } finally {
    await plugin.stop()
    await hub.close()
  }
})

test('优雅退出时向中台注销，并带上注册响应里下发的那份凭证', async () => {
  const hub = await MockHub.start({ heartbeatIntervalSeconds: 1 })
  const plugin = await startPlugin(hub)

  try {
    await hub.waitForRegistration(1)
    await plugin.stop()

    assert.deepEqual(hub.unregisters, ['test-1'])
    // 凭证是注销的属主证明，中台凭它认出「你是这一行的主人」——不带或带错都删不掉行
    assert.deepEqual(hub.unregisterTokens, [hub.stateToken])
    assert.deepEqual(hub.refusedUnregisters, [])
    assert.equal(hub.instancesSnapshot().length, 0, '注销之后不该还留在册上')
  } finally {
    // 断言失败时也必须收摊：mock 的 gRPC 服务不停，node --test 会一直挂着不退出
    // （报出来的是 "Promise resolution is still pending"，与被测行为毫无关系）
    await hub.close()
  }
})

test('注册从未成功时不发注销', async () => {
  // 注册被拒的插件手里没有凭证，而"发一次不带身份的注销"正是删掉**属主**那一行的
  // 路径：instanceId 由插件自报、可以撞。所以这里不发请求，而不是发一个注定被拒的。
  const hub = await MockHub.start({
    heartbeatIntervalSeconds: 1,
    rejectRegister: () => [
      { code: 'REJECT_CODE_UNREACHABLE', message: '中台拨不通你自报的地址', detail: '检查 HUB_ADVERTISE_ADDR' },
    ],
  })
  const plugin = await startPlugin(hub)

  try {
    // 先确认它**试过**注册：没有这一条，下面的断言会因为"压根没跑起来"而假绿
    await hub.waitForRegistration(1)
    assert.equal(hub.rejections.length >= 1, true, '应当至少被拒过一次')

    await plugin.stop()

    assert.deepEqual(hub.unregisters, [], '没拿到过凭证就不该发注销')
    assert.equal(hub.instancesSnapshot().length, 0, '本来就没注册上，注册表里不该有它')
  } finally {
    await hub.close()
  }
})

test('中台比插件晚起来也能接上', async () => {
  // 插件先启动是常态：注册必须一直重试，而不是起来失败就退。
  // 先起一个中台拿到真实端口再关掉，插件连的就是那个「存在过但现在没人听」的地址
  const boot = await MockHub.start({ heartbeatIntervalSeconds: 1 })
  const target = boot.target
  await boot.close()

  const controller = new AbortController()
  const done = run(
    echoPlugin(),
    {
      hubAddr: `http://${target}`,
      advertiseAddr: 'http://127.0.0.1:1',
      listenAddr: '127.0.0.1:0',
      instanceId: 'late-1',
      retryIntervalMs: 50,
      logger: Logger.silent(),
    },
    { signal: controller.signal },
  )

  let hub: MockHub | undefined
  try {
    // 等它至少失败一次，再把中台起回来（同端口）——否则证明不了是靠重试接上的
    await new Promise((resolve) => setTimeout(resolve, 300))
    hub = await MockHub.startOn(target, { heartbeatIntervalSeconds: 1 })
    await hub.waitForRegistration(1, 5_000)
  } finally {
    controller.abort()
    await done
    await hub?.close()
  }
})

test('配置缺项时拒绝启动', async () => {
  await assert.rejects(
    () => run(echoPlugin(), configFromEnv({} as NodeJS.ProcessEnv)),
    /缺少必填配置 .*HUB_ADDR/,
  )
})

test('manifest 缺版本号时拒绝启动', async () => {
  const bad: Plugin = { ...echoPlugin(), manifest: () => ({ name: 'x', version: '' }) }
  await assert.rejects(
    () => run(bad, { hubAddr: 'http://127.0.0.1:1', advertiseAddr: 'http://127.0.0.1:1' }),
    /缺少版本号/,
  )
})

test('声明了自有类型却没给 descriptor 时拒绝启动', async () => {
  const bad: Plugin = {
    ...echoPlugin(),
    manifest: () => ({
      name: 'x',
      version: '0.1.0',
      consumes: [{ fqName: 'wms.v1.OrderCreated', description: '自有类型' }],
    }),
    descriptor: () => new Uint8Array(0),
  }
  await assert.rejects(
    () => run(bad, { hubAddr: 'http://127.0.0.1:1', advertiseAddr: 'http://127.0.0.1:1' }),
    /声明了自有类型/,
  )
})
