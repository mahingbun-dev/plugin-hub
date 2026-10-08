// 本文件放**跨语言的**规则判定。
//
// 事实来源是 `sdk/go/hubkit/testdata/hub-rules.json`——Rust 侧
// （`crates/hub-grpc/tests/state_rules.rs`）与 Go 侧（`sdk/go/hubkit/rules_test.go`）
// 读的也是同一份文件。本文件里的两个函数必须符合那份 cases，由
// `test/rules.test.ts` 逐条比对。
//
// 判定合一，**动作不合并**：中台与 mock 拿它去拒绝请求，插件拿它去跳过缓存
// （fail-open，不拒账号）。同一个函数在两侧的正确动作不同，不要试图统一。

/**
 * 判断字符串能否作为 HubState 的 namespace / key / scan prefix。
 *
 * 规则：非空、最长 200 字节、只允许 `[A-Za-z0-9_.-]`。
 *
 * 放行 `*` 是漏洞不是功能：前缀靠字符串拼接，通配符会让 KvScan 变成跨命名空间的
 * 模式匹配；冒号同理——它破坏前缀的结构。
 *
 * 长度按 UTF-8 字节算而不是 `String.length`（那是 UTF-16 码元个数）：中台那边
 * 是 `len(...)` 的字节数，而白名单全是 ASCII，唯一的差异只在非法输入上——
 * 非法输入无论先撞长度上限还是先撞字符集，结果都是 false，所以两种算法不可观测。
 * 这里仍按字节算，是为了与规则文件的措辞一一对上。
 */
export function validStateSegment(s: string): boolean {
  const bytes = Buffer.byteLength(s, 'utf8')
  if (s.length === 0 || bytes > 200) return false

  for (const ch of s) {
    const ok =
      (ch >= 'a' && ch <= 'z') ||
      (ch >= 'A' && ch <= 'Z') ||
      (ch >= '0' && ch <= '9') ||
      ch === '_' ||
      ch === '.' ||
      ch === '-'
    if (!ok) return false
  }
  return true
}

/**
 * 判断 manifest 里的插件名是否合法。
 *
 * 规则：非空、最长 64 字节、首字符是字母或数字、其余位置允许 `[A-Za-z0-9_-]`。
 * 与中台的 `crates/hub-registry/src/validate.rs` 的 `is_valid_plugin_name` 等价。
 */
export function validPluginName(name: string): boolean {
  const bytes = Buffer.byteLength(name, 'utf8')
  if (name.length === 0 || bytes > 64) return false

  // 用 [...name] 按码点遍历：按 UTF-16 码元遍历时，代理对会被拆成两半，
  // 每一半都不在白名单里，结论碰巧也是 false——但那是巧合，不是判定。
  const chars = [...name]
  for (let i = 0; i < chars.length; i++) {
    const ch = chars[i]!
    const alnum =
      (ch >= 'a' && ch <= 'z') || (ch >= 'A' && ch <= 'Z') || (ch >= '0' && ch <= '9')
    if (alnum) continue
    if (i > 0 && (ch === '-' || ch === '_')) continue
    return false
  }
  return true
}
