import assert from 'node:assert/strict'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import path from 'node:path'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'

import { PROTO_ROOT } from '../src/proto.ts'

/**
 * 随包下发的 `.proto` 必须与中台仓库里的契约原文逐字一致。
 *
 * 这条是「契约不可漂移」在 Node 侧的那一半：SDK 把 `.proto` 复制了一份进来（为的是
 * 开发者不必装 protoc），复制出来的第二份就会漂移——改了中台那份而忘了这份，插件
 * 会在注册时被 `BREAKING_CHANGE` 拒，而那时离「复制」已经很远了。
 *
 * ⚠️ 打包下发的 SDK 里没有中台仓库，跳过而不是假装通过（跳过会被报出来）。
 */
// sdk/node/proto → sdk/node → sdk → 仓库根
const CENTRAL_PROTO_DIR = path.resolve(PROTO_ROOT, '..', '..', '..', 'crates', 'hub-proto', 'proto', 'hub', 'v1')

test('.proto 与 crates/hub-proto 的契约原文逐字一致', (t) => {
  if (!existsSync(CENTRAL_PROTO_DIR)) {
    t.skip(`找不到中台仓库的契约目录 ${CENTRAL_PROTO_DIR}（打包下发的 SDK 里没有）`)
    return
  }

  const local = readdirSync(path.join(PROTO_ROOT, 'hub', 'v1')).sort()
  const central = readdirSync(CENTRAL_PROTO_DIR).filter((f) => f.endsWith('.proto')).sort()

  // 本 SDK 不带 bus.proto：插件面用不到消息总线
  const expected = central.filter((f) => f !== 'bus.proto')
  assert.deepEqual(local, expected, 'SDK 里的 .proto 清单与契约目录对不上')

  for (const file of local) {
    const localBytes = readFileSync(path.join(PROTO_ROOT, 'hub', 'v1', file))
    const centralBytes = readFileSync(path.join(CENTRAL_PROTO_DIR, file))
    assert.ok(
      localBytes.equals(centralBytes),
      `${file} 与中台的契约原文不一致——改了中台那份就要把这份一起更新（中台的 crates/hub-proto）`,
    )
  }
})

test('well-known 类型随包下发', () => {
  // proto-loader 找不到 import 时会直接抛错，而内网机器上未必装着 protoc 的 include 目录
  for (const file of ['any.proto', 'struct.proto']) {
    const full = path.join(PROTO_ROOT, 'google', 'protobuf', path.basename(file))
    assert.ok(existsSync(full), `${full} 不存在`)
    assert.ok(readFileSync(full, 'utf8').includes('package google.protobuf'))
  }
})
