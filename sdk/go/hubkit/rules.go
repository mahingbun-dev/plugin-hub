package hubkit

// 本文件放**跨语言的**规则判定。
//
// 事实来源是 testdata/hub-rules.json——Rust 侧（crates/hub-grpc/tests/state_rules.rs）
// 读同一份文件并断言它那边的实现。任一侧改了实现而没同步契约文件，**它自己那侧**的
// 契约测试就会变红；改了契约文件而某一侧没跟上，则是那一侧变红。如此挡住
// 「插件以为写进去了、中台拒绝」这类静默漂移。
//
// 判定合一，**动作不合并**：中台与 mockhub 拿它去拒绝请求，插件拿它去跳过缓存
// （fail-open，不拒账号）。同一个函数在两侧的正确动作不同，不要试图统一。

// ValidStateSegment 判断字符串能否作为 HubState 的 namespace / key / scan prefix。
//
// 规则：非空、最长 200 字节、只允许 [A-Za-z0-9_.-]。
//
// 放行 `*` 是漏洞不是功能：前缀靠字符串拼接，通配符会让 KvScan 变成跨命名空间的
// 模式匹配；冒号同理——它破坏前缀的结构。
//
// 按字节遍历是安全的：白名单全是 ASCII，非 ASCII 字符的首字节必然 >= 0x80，
// 会被 default 分支拒掉。
func ValidStateSegment(s string) bool {
	if s == "" || len(s) > 200 {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c >= 'a' && c <= 'z', c >= 'A' && c <= 'Z', c >= '0' && c <= '9':
		case c == '_', c == '.', c == '-':
		default:
			return false
		}
	}
	return true
}

// ValidPluginName 判断 manifest 里的插件名是否合法。
//
// 规则：非空、最长 64 字节、首字符是字母或数字、其余位置允许 [A-Za-z0-9_-]。
// 与中台的 crates/hub-registry/src/validate.rs 的 is_valid_plugin_name 等价。
func ValidPluginName(name string) bool {
	if name == "" || len(name) > 64 {
		return false
	}
	// range 的索引 i 是字节偏移，首个 rune 的偏移恒为 0，所以 i > 0 即「非首字符」
	for i, r := range name {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9':
		case i > 0 && (r == '-' || r == '_'):
		default:
			return false
		}
	}
	return true
}
