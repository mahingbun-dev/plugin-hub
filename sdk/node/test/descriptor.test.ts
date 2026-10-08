import assert from 'node:assert/strict'
import { test } from 'node:test'

import { DescriptorError, descriptorMessages, emptyDescriptor } from '../src/descriptor.ts'

// 手写一段 FileDescriptorSet 的线格式，而不是靠 protobufjs 生成：
// 被测的就是「不依赖任何 descriptor.proto 也能把消息名读出来」这件事，
// 用同一个 protobuf 实现去造输入、再去验它，证明不了这一点。

function concat(...parts: Uint8Array[]): Uint8Array {
  const total = parts.reduce((n, p) => n + p.length, 0)
  const out = new Uint8Array(total)
  let at = 0
  for (const part of parts) {
    out.set(part, at)
    at += part.length
  }
  return out
}

function varint(value: number): Uint8Array {
  const bytes: number[] = []
  let rest = value
  do {
    const byte = rest & 0x7f
    rest = Math.floor(rest / 128)
    bytes.push(rest > 0 ? byte | 0x80 : byte)
  } while (rest > 0)
  return new Uint8Array(bytes)
}

function tag(field: number, wire: number): Uint8Array {
  return varint(field * 8 + wire)
}

function lenDelimited(field: number, payload: Uint8Array): Uint8Array {
  return concat(tag(field, 2), varint(payload.length), payload)
}

function text(field: number, value: string): Uint8Array {
  return lenDelimited(field, new TextEncoder().encode(value))
}

/** DescriptorProto：name=1、nested_type=3、options=7 */
function message(name: string, nested: Uint8Array[] = [], mapEntry = false): Uint8Array {
  // MessageOptions.map_entry = 7（bool，varint）
  const options = concat(tag(7, 0), varint(1))

  return concat(
    text(1, name),
    ...nested.map((n) => lenDelimited(3, n)),
    ...(mapEntry ? [lenDelimited(7, options)] : []),
  )
}

/** FileDescriptorProto：package=2、message_type=4 */
function file(pkg: string, messages: Uint8Array[]): Uint8Array {
  return concat(text(2, pkg), ...messages.map((m) => lenDelimited(4, m)))
}

/** FileDescriptorSet：file=1 */
function descriptorSet(files: Uint8Array[]): Uint8Array {
  return concat(...files.map((f) => lenDelimited(1, f)))
}

const SAMPLE = descriptorSet([
  file('wms.v1', [
    message('OrderCreated', [
      message('Item'),
      // map 字段会生成合成的 XxxEntry 消息，属实现细节，不算契约类型
      message('SkuEntry', [], true),
    ]),
    message('OrderShipped'),
  ]),
  file('tms.v1', [message('Shipment')]),
])

test('读出消息全限定名，嵌套带前缀', () => {
  assert.deepEqual([...descriptorMessages(SAMPLE)].sort(), [
    'tms.v1.Shipment',
    'wms.v1.OrderCreated',
    'wms.v1.OrderCreated.Item',
    'wms.v1.OrderShipped',
  ])
})

test('map 生成的合成消息不算契约类型', () => {
  const found = descriptorMessages(SAMPLE)
  assert.equal(found.has('wms.v1.OrderCreated.SkuEntry'), false)
})

test('package 与消息的先后顺序不影响结果', () => {
  // 编码方没有义务把 package 排在消息前面，两趟扫描各自独立
  const outOfOrder = descriptorSet([concat(lenDelimited(4, message('Order')), text(2, 'wms.v1'))])
  assert.deepEqual([...descriptorMessages(outOfOrder)], ['wms.v1.Order'])
})

test('空 descriptor 是合法的', () => {
  assert.deepEqual([...descriptorMessages(emptyDescriptor())], [])
  assert.equal(emptyDescriptor().length, 0)
})

test('截断的 descriptor 报错而不是给出半份结果', () => {
  const truncated = SAMPLE.slice(0, SAMPLE.length - 3)
  assert.throws(() => descriptorMessages(truncated), DescriptorError)
})
