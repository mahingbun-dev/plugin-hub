import assert from 'node:assert/strict'
import { test } from 'node:test'

import { Logger, formatTime, levelFromEnv } from '../src/log.ts'

/** 把日志收进一个数组，供断言用。 */
function collector(): { lines: string[]; logger: Logger } {
  const lines: string[] = []
  return { lines, logger: new Logger('DEBUG', (line) => lines.push(line)) }
}

function parse(line: string): Record<string, unknown> {
  return JSON.parse(line) as Record<string, unknown>
}

test('一行一个 JSON，字段与 Go 侧的 slog 同形', () => {
  const { lines, logger } = collector()
  logger.info('已注册到中台', { plugin: 'order-reader', version: '0.1.0' })

  assert.equal(lines.length, 1)
  assert.ok(lines[0]?.endsWith('\n'), '每条日志自成一行')

  const entry = parse(lines[0]!)
  assert.deepEqual(Object.keys(entry), ['time', 'level', 'msg', 'plugin', 'version'])
  assert.equal(entry['level'], 'INFO')
  assert.equal(entry['msg'], '已注册到中台')
  assert.equal(entry['plugin'], 'order-reader')
  // 中台的接入指南引用的就是这个形状，键序也照它来
  assert.match(String(entry['time']), /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}[+-]\d{2}:\d{2}$/)
})

test('级别过滤', () => {
  const lines: string[] = []
  const logger = new Logger('WARN', (line) => lines.push(line))

  logger.debug('看不见')
  logger.info('看不见')
  logger.warn('看得见')
  logger.error('看得见')

  assert.equal(lines.length, 2)
  assert.equal(parse(lines[0]!)['level'], 'WARN')
})

test('缺字段的字段集是稳定的', () => {
  const { lines, logger } = collector()
  // 中台没给 detail 时，字段不能整个消失——脚本要能无脑取
  logger.error('中台拒绝了注册', { code: 'UNREACHABLE', message: '拨不通', detail: undefined })

  const entry = parse(lines[0]!)
  assert.equal(entry['detail'], '')
  assert.deepEqual(Object.keys(entry), ['time', 'level', 'msg', 'code', 'message', 'detail'])
})

test('Error 落成 message', () => {
  const { lines, logger } = collector()
  logger.warn('心跳失败', { err: new Error('connect ECONNREFUSED') })

  // JSON.stringify(error) 给的是 {}，那样这一行就白打了
  assert.equal(parse(lines[0]!)['err'], 'connect ECONNREFUSED')
})

test('HUB_LOG_LEVEL 认不出来时按 INFO', () => {
  assert.equal(levelFromEnv('debug'), 'DEBUG')
  assert.equal(levelFromEnv(' WARN '), 'WARN')
  assert.equal(levelFromEnv('error'), 'ERROR')
  assert.equal(levelFromEnv('trace'), 'INFO')
  assert.equal(levelFromEnv(undefined), 'INFO')
  // 拼错一个级别不该让插件起不来
  assert.equal(levelFromEnv(''), 'INFO')
})

test('时间戳带本地时区偏移', () => {
  const formatted = formatTime(new Date(2026, 8, 18, 10, 47, 43, 521))
  // 具体偏移随机器时区变，这里只锁形状
  assert.match(formatted, /^2026-09-18T10:47:43\.521[+-]\d{2}:\d{2}$/)
})
