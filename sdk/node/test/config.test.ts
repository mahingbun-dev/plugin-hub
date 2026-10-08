import assert from 'node:assert/strict'
import { test } from 'node:test'

import {
  DEFAULT_LISTEN_ADDR,
  HEARTBEAT_FALLBACK_INTERVAL_MS,
  REGISTER_RETRY_INTERVAL_MS,
  configFromEnv,
  defaultInstanceId,
  validateConfig,
  withDefaults,
} from '../src/config.ts'

test('缺必填项时把缺哪些一次说全', () => {
  assert.throws(() => validateConfig({ hubAddr: '', advertiseAddr: '' }), (err: Error) => {
    assert.match(err.message, /HUB_ADDR（中台插件面地址）/)
    assert.match(err.message, /HUB_ADVERTISE_ADDR（中台可达的本插件地址）/)
    return true
  })

  // 只缺一个时也只说那一个
  assert.throws(
    () => validateConfig({ hubAddr: 'http://127.0.0.1:8093', advertiseAddr: '  ' }),
    /HUB_ADVERTISE_ADDR/,
  )
})

test('缺省值', () => {
  const cfg = withDefaults({ hubAddr: 'http://127.0.0.1:8093', advertiseAddr: 'http://127.0.0.1:9000' })

  assert.equal(cfg.listenAddr, DEFAULT_LISTEN_ADDR)
  assert.equal(cfg.listenAddr, ':9000')
  assert.equal(cfg.retryIntervalMs, REGISTER_RETRY_INTERVAL_MS)
  assert.equal(HEARTBEAT_FALLBACK_INTERVAL_MS, 10_000)
  assert.match(cfg.instanceId, /-\d+$/, '缺省实例标识应以 PID 结尾')
  assert.equal(cfg.instanceId, defaultInstanceId())
  assert.ok(cfg.logger)
})

test('环境变量：空串等同没设，纯空白要留住', () => {
  const cfg = configFromEnv({
    HUB_ADDR: 'http://127.0.0.1:8093',
    HUB_ADVERTISE_ADDR: 'http://127.0.0.1:9000',
    HUB_INSTANCE_ID: '',
  } as NodeJS.ProcessEnv)

  // 空串等同没设：SDK 自动生成「主机名-PID」，注册得成
  const resolved = withDefaults(cfg)
  assert.equal(resolved.instanceId, defaultInstanceId())

  // 纯空白**不能**当成没设：它会带着一个空白 id 去注册，被中台判 INSTANCE_CONFLICT，
  // 而踩坑的正是复制粘贴带上的空格——把它悄悄修好反而掩盖了真正的问题
  const blank = withDefaults(
    configFromEnv({
      HUB_ADDR: 'http://127.0.0.1:8093',
      HUB_ADVERTISE_ADDR: 'http://127.0.0.1:9000',
      HUB_INSTANCE_ID: ' ',
    } as NodeJS.ProcessEnv),
  )
  assert.equal(blank.instanceId, ' ')
})

test('监听地址为空的写法由骨架归一化', async () => {
  const { normalizeListenAddr, effectiveListenAddr, hubTarget } = await import('../src/run.ts')

  // `:9000` 是 Go 的写法，grpc-js 的地址解析要求有主机名
  assert.equal(normalizeListenAddr(':9000'), '0.0.0.0:9000')
  assert.equal(normalizeListenAddr('127.0.0.1:9000'), '127.0.0.1:9000')
  assert.equal(effectiveListenAddr('0.0.0.0:0', 19301), '0.0.0.0:19301')
  assert.equal(effectiveListenAddr(':0', 19301), '0.0.0.0:19301')
})

test('中台地址归一成 grpc target', async () => {
  const { hubTarget, hubCredentials } = await import('../src/run.ts')

  assert.equal(hubTarget('http://127.0.0.1:8093'), '127.0.0.1:8093')
  assert.equal(hubTarget('https://hub.example.com:8094'), 'hub.example.com:8094')
  // 路径要丢掉：插件面是独立端口，带上 HTTP 面的 /hub-api 是把两个面配混了
  assert.equal(hubTarget('http://127.0.0.1:8093/hub-api'), '127.0.0.1:8093')

  // https 才走 TLS
  assert.ok(hubCredentials('https://x:1'))
  assert.ok(hubCredentials('http://x:1'))
})
