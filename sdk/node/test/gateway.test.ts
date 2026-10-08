import assert from 'node:assert/strict'
import { test } from 'node:test'

import * as grpc from '@grpc/grpc-js'

import {
  CALL_CHAIN_META,
  GatewayClient,
  InvokeFailed,
  InvokeRejected,
  MAX_INVOKE_DEPTH,
  newPluginGatewayClient,
} from '../src/gateway.ts'
import type { PluginGatewayClient } from '../src/gateway.ts'
import {
  STRUCT_FQ_NAME,
  cloneEnvelope,
  emptyDescriptor,
  emptyEnvelope,
  invalid,
  issue,
  newEnvelope,
  normalizeManifest,
  payloadJSON,
  structContract,
  valid,
  withPayloadJSON,
} from '../src/index.ts'
import type { Envelope, InvokeRequest, InvokeResponse, Plugin } from '../src/index.ts'
import { Logger } from '../src/log.ts'
import { MockHub, freeAddr } from '../src/mockhub.ts'
import { STATE_TOKEN_METADATA, hubServices } from '../src/proto.ts'
import { run } from '../src/run.ts'

// ---------------------------------------------------------------- 客户端侧
//
// InvokePlugin 的信封装配（链复制、trace 复用、deadline 夹紧）是客户端自己的逻辑，
// 要对「发出去的请求」逐字段断言——mock 中台在链路另一端，看到的是中台整备之后的
// 信封，验不出客户端这边的行为。

interface RecordedInvoke {
  req: InvokeRequest
  token: string
  /** 本次 gRPC 调用的 deadline 选项（毫秒绝对时刻）。未设置时 undefined。 */
  deadline: unknown
}

/** 记录最近 Invoke 请求的假客户端，按预设应答。 */
function recordingGateway(
  resp: InvokeResponse,
  err: grpc.ServiceError | null = null,
): { client: PluginGatewayClient; invokes: RecordedInvoke[] } {
  const invokes: RecordedInvoke[] = []
  const fake = {
    Invoke: (
      req: InvokeRequest,
      md: grpc.Metadata,
      options: grpc.CallOptions,
      cb: (e: grpc.ServiceError | null, r: InvokeResponse) => void,
    ) => {
      invokes.push({
        req,
        token: String(md.get(STATE_TOKEN_METADATA)[0] ?? ''),
        deadline: options.deadline,
      })
      cb(err, resp)
      return {} as grpc.ClientUnaryCall
    },
  }
  return { client: fake as unknown as PluginGatewayClient, invokes }
}

/** 应答 HANDLED 的假客户端：装配用例只关心请求，给一个最小的合法 HANDLED。 */
function handledFake(): { client: PluginGatewayClient; invokes: RecordedInvoke[] } {
  return recordingGateway({
    outcome: 'HANDLED',
    envelope: { ...emptyEnvelope(), messageId: '01J0HANDLED' },
    issues: [],
    reason: '',
    elapsedMs: 0,
  })
}

test('invokePlugin 复制当前信封的 trace 上下文', async () => {
  const { client, invokes } = handledFake()
  const gateway = new GatewayClient(client)
  gateway.setToken('tok-1')

  const current = emptyEnvelope()
  current.messageId = '01J0OLD'
  current.traceId = '01J0TRACE'
  current.runId = 'run-7'
  current.nodeId = 'node-3'
  // 整体预算很远，应被本次预算夹紧
  current.deadlineMs = Date.now() + 3_600_000
  current.meta[CALL_CHAIN_META] = 'p1,p2'

  const before = Date.now()
  const out = await gateway.invokePlugin('target', {
    timeoutMs: 5_000,
    currentEnvelope: current,
    payloadJSON: { text: '你好' },
  })

  // HANDLED → 返回下游的信封
  assert.equal(out.messageId, '01J0HANDLED')

  assert.equal(invokes.length, 1)
  const { req, token, deadline } = invokes[0]!
  // 凭证必须走 x-hub-state-token metadata，与 StateClient 同款
  assert.equal(token, 'tok-1')
  assert.equal(req.plugin, 'target')
  assert.equal(req.version, '', '不指定版本时应传空（= 最新版本）')
  assert.equal(req.timeoutMs, 5000)

  const sent = req.envelope!
  assert.equal(sent.traceId, '01J0TRACE')
  assert.equal(sent.runId, 'run-7')
  assert.equal(sent.nodeId, 'node-3')
  // 链是「原样复制」：多一个自己或少一段都算错——追加 caller 是中台的事
  assert.equal(sent.meta[CALL_CHAIN_META], 'p1,p2')
  assert.equal(Object.keys(sent.meta).length, 1, 'meta 里不该有链以外的键')
  assert.ok(sent.messageId, 'message_id 应换新')
  assert.notEqual(sent.messageId, '01J0OLD')
  assert.equal(sent.type, 'PAYLOAD_TYPE_REQUEST')

  // deadline 取 min(整体预算, now+5s)：整体预算在一小时后，所以应落在 5s 附近
  assert.ok(
    sent.deadlineMs > before + 4_000 && sent.deadlineMs < before + 6_000,
    `deadline 应夹到本次预算附近，实际剩余 ${sent.deadlineMs - before}`,
  )
  // 信封预算同时是 gRPC 调用的预算：到点客户端先放弃
  assert.equal(deadline, sent.deadlineMs)

  const [payload, ok] = payloadJSON(sent)
  assert.ok(ok, 'JSON 载荷应按 Struct 打包')
  assert.equal(payload?.['text'], '你好')
})

test('invokePlugin 整体预算更早时以其为准', async () => {
  const { client, invokes } = handledFake()
  const gateway = new GatewayClient(client)

  const soon = Date.now() + 2_000
  await gateway.invokePlugin('target', {
    timeoutMs: 30_000,
    currentEnvelope: { ...emptyEnvelope(), traceId: 't', deadlineMs: soon },
  })

  // 本次预算不允许把整体 deadline 往后顶
  const sent = invokes[0]!.req.envelope!
  assert.ok(
    sent.deadlineMs <= soon + 1_000,
    `deadline 不该被顶到整体预算之后：整体 +${soon - Date.now()}ms，实际剩余 ${sent.deadlineMs - Date.now()}ms`,
  )
})

test('invokePlugin 无当前信封时新开 trace', async () => {
  const { client, invokes } = handledFake()
  const gateway = new GatewayClient(client)

  await gateway.invokePlugin('target', { payloadJSON: { n: 1 } })

  const sent = invokes[0]!.req.envelope!
  assert.equal(sent.traceId.length, 26, `新 trace_id 应是 26 字符的 ULID，实际 ${sent.traceId}`)
  assert.equal(sent.meta[CALL_CHAIN_META], undefined, '顶层直调不该带调用链')
  assert.equal(sent.runId, '')
  assert.equal(sent.nodeId, '')
  // 没有预算也没给超时：deadline 留空，由中台给默认预算兜底
  assert.equal(sent.deadlineMs, 0)
  assert.equal(invokes[0]!.deadline, undefined)
})

test('invokePlugin 结果映射：REJECTED/ERROR 映射为类型化异常', async () => {
  const rejected = new GatewayClient(
    recordingGateway({
      outcome: 'REJECTED',
      envelope: null,
      issues: [{ path: 'payload.sku', message: '缺少必填字段', severity: 'SEVERITY_ERROR' }],
      reason: '目标插件校验未通过，见 issues',
      elapsedMs: 0,
    }).client,
  )
  await assert.rejects(
    () => rejected.invokePlugin('target'),
    (err: unknown) => {
      assert.ok(err instanceof InvokeRejected, `应映射为 InvokeRejected，实际 ${String(err)}`)
      assert.equal((err as InvokeRejected).issues[0]?.path, 'payload.sku', 'issues 应随异常携带')
      assert.ok((err as InvokeRejected).reason, 'reason 应随异常携带')
      return true
    },
  )

  const failed = new GatewayClient(
    recordingGateway({
      outcome: 'ERROR',
      envelope: null,
      issues: [],
      reason: '未声明对 target 的调用授权',
      elapsedMs: 0,
    }).client,
  )
  await assert.rejects(
    () => failed.invokePlugin('target'),
    (err: unknown) => {
      assert.ok(err instanceof InvokeFailed, `应映射为 InvokeFailed，实际 ${String(err)}`)
      assert.equal((err as InvokeFailed).reason, '未声明对 target 的调用授权')
      return true
    },
  )

  const unknown = new GatewayClient(
    recordingGateway({
      outcome: 'INVOKE_OUTCOME_UNSPECIFIED',
      envelope: null,
      issues: [],
      reason: '',
      elapsedMs: 0,
    }).client,
  )
  await assert.rejects(() => unknown.invokePlugin('target'), /未知的结果分类/)
})

test('invokePlugin 基础设施故障原样透传，401 叫醒 onDenied', async () => {
  const err401 = Object.assign(new Error('状态凭证无效或已失效'), {
    code: grpc.status.UNAUTHENTICATED,
    details: '状态凭证无效或已失效',
    metadata: new grpc.Metadata(),
  }) as grpc.ServiceError
  let denied = 0
  const gateway = new GatewayClient(recordingGateway({} as InvokeResponse, err401).client, () => {
    denied += 1
  })

  // 未鉴权就该是 UNAUTHENTICATED：调用方才知道该等凭证轮换而不是改数据
  await assert.rejects(
    () => gateway.invokePlugin('target'),
    (err: grpc.ServiceError) => err.code === grpc.status.UNAUTHENTICATED,
  )
  assert.equal(denied, 1, '401 应触发 onDenied 去换新凭证')
})

test('invokePlugin 参数防线', async () => {
  const gateway = new GatewayClient(handledFake().client)

  await assert.rejects(() => gateway.invokePlugin('  '), /缺少目标插件名/)
  await assert.rejects(
    () =>
      gateway.invokePlugin('t', {
        payloadJSON: {},
        payload: { typeUrl: STRUCT_FQ_NAME, value: new Uint8Array(0) },
      }),
    /二选一/,
  )
  await assert.rejects(() => gateway.invokePlugin('t', { timeoutMs: -1 }), /不能为负/)
})

// ---------------------------------------------------------------- 链路侧
//
// 与 mock 中台之间走真实 gRPC：验发现三件套、互调整备与业务结果在两端的行为。

function gatewayFor(hub: MockHub, onDenied: () => void = () => {}): GatewayClient {
  const gateway = new GatewayClient(
    newPluginGatewayClient(hub.target, grpc.credentials.createInsecure()),
    onDenied,
    // 测试里不设上限：真出问题时有测试运行器的超时兜着
    0,
  )
  gateway.setToken(hub.stateToken)
  return gateway
}

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
      client[name]!(req, new grpc.Metadata(), {}, (err, res) => (err ? reject(err) : resolve(res)))
    })
  return { Register: call('Register'), Unregister: call('Unregister') }
}

/** 让一个插件进到 mock 的注册表里（不起实例， advertise 地址是拨不通的占位）。 */
async function registerPlugin(
  hub: MockHub,
  name: string,
  version: string,
  manifest: Parameters<typeof normalizeManifest>[0] | null,
  instanceId: string,
): Promise<string> {
  const resp = await registryOf(hub).Register({
    pluginName: name,
    version,
    instanceId,
    advertiseAddr: 'http://127.0.0.1:1',
    manifest: manifest ? normalizeManifest(manifest) : null,
    descriptorSet: new Uint8Array(0),
  })
  return resp.stateToken
}

/**
 * 起一个真实插件（含 gRPC 服务与注册循环），监听与 advertise 用同一个**真实**端口。
 *
 * 互调与 flow 执行不同：mock 中台会真的拨 advertise 地址去调 Validate / Handle，
 * 所以这里的地址不能再像 run.test.ts 那样用占位符。
 */
async function startPlugin(hub: MockHub, plugin: Plugin): Promise<{ stop: () => Promise<void> }> {
  const addr = await freeAddr()
  const controller = new AbortController()
  const done = run(
    plugin,
    {
      hubAddr: hub.addr,
      advertiseAddr: `http://${addr}`,
      listenAddr: addr,
      retryIntervalMs: 50,
      logger: Logger.silent(),
    },
    { signal: controller.signal },
  )
  return {
    stop: async () => {
      controller.abort()
      await done
    },
  }
}

/** 等一个值就绪，或超时。 */
async function waitFor<T>(ready: () => T | undefined, what: string, timeoutMs = 5_000): Promise<T> {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    const value = ready()
    if (value !== undefined) return value
    if (Date.now() > deadline) throw new Error(`等待 ${what} 超时`)
    await new Promise((resolve) => setTimeout(resolve, 10))
  }
}

test('发现三件套走 mock 中台', async () => {
  const hub = await MockHub.start()
  try {
    const bareToken = await registerPlugin(hub, 'bare-test', '0.2.0', null, 'b-1')
    await registerPlugin(
      hub,
      'declared-test',
      '1.0.0',
      {
        name: 'declared-test',
        version: '1.0.0',
        produces: [structContract()],
        consumes: [structContract()],
        invokes: ['echo-test'],
        tools: [{ name: 'do-thing', description: '做一件事' }],
      },
      'd-1',
    )

    const gateway = gatewayFor(hub)

    // ListPlugins：注册过的插件都能被看到，且注册过即在线（mock 不做健康探测）
    const plugins = await gateway.listPlugins()
    assert.deepEqual(
      plugins.plugins.map((p) => p.name).sort(),
      ['bare-test', 'declared-test'],
    )
    assert.ok(plugins.plugins.every((p) => p.online && p.instanceCount === 1))

    // DescribeMessage：按消息反查生产者与消费者
    const desc = await gateway.describeMessage(STRUCT_FQ_NAME)
    assert.deepEqual(desc.producers, [{ plugin: 'declared-test', version: '1.0.0' }])
    assert.deepEqual(desc.consumers, [{ plugin: 'declared-test', version: '1.0.0' }])

    // GetContract：契约、invokes 声明与工具一起返回
    const contract = await gateway.getContract('declared-test')
    assert.equal(contract.version, '1.0.0')
    assert.deepEqual(contract.invokes, ['echo-test'])
    assert.equal(contract.produces[0]?.fqName, STRUCT_FQ_NAME)
    assert.equal(contract.tools[0]?.name, 'do-thing')

    // 查无此插件 / 版本不对是 gRPC NotFound（参数错了，不是业务结果）
    await assert.rejects(
      () => gateway.getContract('ghost'),
      (err: grpc.ServiceError) => err.code === grpc.status.NOT_FOUND,
    )
    await assert.rejects(
      () => gateway.getContract('declared-test', '9.9.9'),
      (err: grpc.ServiceError) => err.code === grpc.status.NOT_FOUND,
    )

    // 实例全被摘掉后插件仍在册但离线：缺省清单不含它，includeOffline 才可见
    await registryOf(hub).Unregister({ instanceId: 'b-1', reason: '测试', stateToken: bareToken })
    const online = await gateway.listPlugins()
    assert.deepEqual(online.plugins.map((p) => p.name), ['declared-test'])
    const all = await gateway.listPlugins(true)
    const bare = all.plugins.find((p) => p.name === 'bare-test')
    assert.equal(bare?.online, false, '实例被摘后应报离线')
    assert.equal(bare?.instanceCount, 0)
  } finally {
    await hub.close()
  }
})

test('invokePlugin 端到端：信封整备后经真实 gRPC 调到被调方', async () => {
  const hub = await MockHub.start()
  const received: Envelope[] = []
  const callee = await startPlugin(hub, {
    manifest: () => ({ name: 'echo-test', version: '1.0.0', consumes: [structContract()] }),
    descriptor: () => emptyDescriptor(),
    validate: (_ctx, env) => {
      const [payload, ok] = payloadJSON(env)
      return ok && typeof payload?.['text'] === 'string'
        ? valid()
        : invalid(issue('payload.text', '缺少必填字段 text'))
    },
    handle: (_ctx, env) => {
      // 记下收到的信封供断言：「中台整备」发生在 mock 一侧，调用方自己的视角看不见
      received.push(cloneEnvelope(env))
      const [payload] = payloadJSON(env)
      return withPayloadJSON(env, { echo: payload?.['text'] })
    },
  })

  try {
    await hub.waitForRegistration(1)
    const gateway = gatewayFor(hub)

    // 模拟「调用方正在处理一条从上游来的消息」：trace/链都是上游带的
    const current = emptyEnvelope()
    current.messageId = '01J0CURRENT'
    current.traceId = '01J0TRACE01'
    current.runId = 'run-7'
    current.nodeId = 'node-3'
    current.deadlineMs = Date.now() + 10_000
    current.meta[CALL_CHAIN_META] = 'p1,p2'

    const out = await gateway.invokePlugin('echo-test', {
      timeoutMs: 5_000,
      currentEnvelope: current,
      payloadJSON: { text: '你好' },
    })
    const [payload, ok] = payloadJSON(out)
    assert.ok(ok, '应拿到被调方的回显载荷')
    assert.equal(payload?.['echo'], '你好')

    // 被调方视角：信封是中台整备之后的
    assert.equal(received.length, 1)
    const env = received[0]!
    assert.equal(env.traceId, '01J0TRACE01', 'trace 应从上游贯通')
    assert.equal(env.runId, 'run-7')
    assert.equal(env.nodeId, 'node-3')
    // mock 的身份反查固定返回 mock-plugin，所以链上追加的是它，而不是调用方的注册名
    assert.equal(env.meta[CALL_CHAIN_META], 'p1,p2,mock-plugin')
    assert.equal(env.subject?.kind, 'SUBJECT_KIND_PLUGIN', 'subject 应被中台覆盖为 caller')
    assert.equal(env.subject?.id, 'mock-plugin')
    assert.equal(env.type, 'PAYLOAD_TYPE_REQUEST')
    assert.notEqual(env.messageId, '01J0CURRENT', 'message_id 应是新信封的新值')
    // deadline 应被夹到 now+5s（整体预算 10s 更晚）
    const left = env.deadlineMs - Date.now()
    assert.ok(left > 3_000 && left < 6_000, `deadline 应夹到本次预算附近，剩余 ${left}`)
  } finally {
    await callee.stop()
    await hub.close()
  }
})

test('invokePlugin 目标校验拒绝时拿到带 issues 的 InvokeRejected', async () => {
  const hub = await MockHub.start()
  const callee = await startPlugin(hub, {
    manifest: () => ({ name: 'strict-test', version: '1.0.0' }),
    descriptor: () => emptyDescriptor(),
    validate: () => invalid(issue('payload.text', '缺少必填字段 text')),
    handle: (_ctx, env) => env,
  })

  try {
    await hub.waitForRegistration(1)
    await assert.rejects(
      () => gatewayFor(hub).invokePlugin('strict-test', { payloadJSON: {} }),
      (err: unknown) => {
        assert.ok(err instanceof InvokeRejected, `应映射为 InvokeRejected，实际 ${String(err)}`)
        assert.equal((err as InvokeRejected).issues[0]?.path, 'payload.text')
        assert.ok((err as InvokeRejected).reason.includes('校验'))
        return true
      },
    )
  } finally {
    await callee.stop()
    await hub.close()
  }
})

test('invokePlugin 成环被拒：caller 已在链上时被调方根本不会被碰到', async () => {
  const hub = await MockHub.start()
  const received: Envelope[] = []
  const callee = await startPlugin(hub, echoInto(received))

  try {
    await hub.waitForRegistration(1)

    // 链上已经有 mock-plugin（mock 认定的 caller）：再调一次就是 A→B→A 的环
    const current = emptyEnvelope()
    current.traceId = '01J0TRACE02'
    current.meta[CALL_CHAIN_META] = 'p1,mock-plugin'

    await assert.rejects(
      () => gatewayFor(hub).invokePlugin('echo-test', { currentEnvelope: current }),
      (err: unknown) => {
        assert.ok(err instanceof InvokeFailed, `应映射为 InvokeFailed，实际 ${String(err)}`)
        assert.ok((err as InvokeFailed).reason.includes('互调环'))
        return true
      },
    )
    assert.equal(received.length, 0, '成环被拒时被调方不该被调到')
  } finally {
    await callee.stop()
    await hub.close()
  }
})

test('invokePlugin 链深超限时以业务结果拒绝', async () => {
  const hub = await MockHub.start()
  try {
    // 预填 8 段链（不含 caller）：加上 caller 就越过了 MAX_INVOKE_DEPTH
    const current = emptyEnvelope()
    current.traceId = '01J0TRACE03'
    current.meta[CALL_CHAIN_META] = Array.from({ length: MAX_INVOKE_DEPTH }, (_, i) => `p${i}`).join(
      ',',
    )

    await assert.rejects(
      () => gatewayFor(hub).invokePlugin('echo-test', { currentEnvelope: current }),
      (err: unknown) => {
        assert.ok(err instanceof InvokeFailed, `应映射为 InvokeFailed，实际 ${String(err)}`)
        assert.ok((err as InvokeFailed).reason.includes('上限'))
        return true
      },
    )
  } finally {
    await hub.close()
  }
})

test('invokePlugin 未注册目标是业务结果而不是 gRPC 错误', async () => {
  const hub = await MockHub.start()
  try {
    await assert.rejects(
      () => gatewayFor(hub).invokePlugin('ghost'),
      (err: unknown) => {
        assert.ok(err instanceof InvokeFailed, `应映射为 InvokeFailed，实际 ${String(err)}`)
        assert.ok((err as InvokeFailed).reason.includes('未注册'))
        return true
      },
    )
  } finally {
    await hub.close()
  }
})

test('配额打满后以业务结果拒绝（每插件每分钟 60 次）', async () => {
  const hub = await MockHub.start()
  try {
    const gateway = gatewayFor(hub)
    // 打未注册的目标：配额记账在解析目标之前，不用真拨插件就能打满，快
    const env = newEnvelope()
    let quotaReason = ''
    for (let i = 1; i <= 60; i++) {
      const resp = await gateway.invoke({ plugin: 'ghost', version: '', envelope: env, timeoutMs: 0 })
      assert.equal(resp.outcome, 'ERROR')
      assert.ok(resp.reason.includes('未注册'), `第 ${i} 次应还是「未注册」，实际 ${resp.reason}`)
    }
    const resp = await gateway.invoke({ plugin: 'ghost', version: '', envelope: env, timeoutMs: 0 })
    quotaReason = resp.reason
    assert.ok(quotaReason.includes('上限'), `超配额应是限额文案，实际 ${quotaReason}`)
  } finally {
    await hub.close()
  }
})

test('凭证缺失时网关面回 401 并叫醒 onDenied', async () => {
  const hub = await MockHub.start()
  let denied = 0
  // 刻意不带 token：mock 与真中台一样，四个 RPC 每个方法第一行都鉴权
  const gateway = new GatewayClient(
    newPluginGatewayClient(hub.target, grpc.credentials.createInsecure()),
    () => {
      denied += 1
    },
    0,
  )

  try {
    await assert.rejects(
      () => gateway.listPlugins(),
      (err: grpc.ServiceError) => err.code === grpc.status.UNAUTHENTICATED,
    )
    assert.equal(denied, 1, '发现面的 401 也应叫醒注册循环')
    await assert.rejects(
      () => gateway.invoke({ plugin: 'x', version: '', envelope: newEnvelope(), timeoutMs: 0 }),
      (err: grpc.ServiceError) => err.code === grpc.status.UNAUTHENTICATED,
    )
    assert.equal(denied, 2, '互调面的 401 同样')
  } finally {
    await hub.close()
  }
})

test('run() 在注册成功后把 GatewayClient 注入实现了 GatewayAware 的插件', async () => {
  const hub = await MockHub.start()
  let injected: GatewayClient | undefined
  const callee = await startPlugin(hub, echoInto([]))
  const caller = await startPlugin(hub, {
    manifest: () => ({ name: 'invoker-test', version: '1.0.0' }),
    descriptor: () => emptyDescriptor(),
    validate: () => valid(),
    handle: (_ctx, env) => env,
    // GatewayAware：由骨架在注册成功后注入
    setGateway: (g: GatewayClient) => {
      injected = g
    },
  } as Plugin)

  try {
    await hub.waitForRegistration(2)
    // 注入的客户端必须带着注册换来的凭证：拿它发一次发现调用，401 就说明没接上
    const gateway = await waitFor(() => injected, 'GatewayClient 注入')
    const plugins = await gateway.listPlugins()
    assert.ok(Array.isArray(plugins.plugins), '注入的客户端应已带凭证，可直接调用')
  } finally {
    await caller.stop()
    await callee.stop()
    await hub.close()
  }
})

// ---------------------------------------------------------------- 测试插件

/** 把收到的信封记进 `received` 的 echo 插件（供「被调方视角」的断言）。 */
function echoInto(received: Envelope[]): Plugin {
  return {
    manifest: () => ({ name: 'echo-test', version: '1.0.0', consumes: [structContract()] }),
    descriptor: () => emptyDescriptor(),
    validate: (_ctx, env) => {
      const [payload, ok] = payloadJSON(env)
      return ok && typeof payload?.['text'] === 'string'
        ? valid()
        : invalid(issue('payload.text', '缺少必填字段 text'))
    },
    handle: (_ctx, env) => {
      received.push(cloneEnvelope(env))
      const [payload] = payloadJSON(env)
      return withPayloadJSON(env, { echo: payload?.['text'] })
    },
  }
}
