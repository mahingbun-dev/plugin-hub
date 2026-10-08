//! **跨语言**的规则判定。
//!
//! 事实来源是 `sdk/go/hubkit/testdata/hub-rules.json`——中台侧的 Rust
//! （`crates/hub-grpc/tests/state_rules.rs`）与 Go 侧（`sdk/go/hubkit/rules_test.go`）
//! 读的是同一份文件，本 SDK 的 `tests/rules.rs` 也读它。任何一侧改了实现而没同步
//! 契约文件，**那一侧**的契约测试就会变红，于是挡住「插件以为写进去了、中台拒绝」
//! 这类静默漂移。
//!
//! 判定合一，**动作不合并**：中台与 mock 拿它去拒绝请求，插件拿它去跳过缓存
//! （fail-open，不拒账号）。同一个函数在两侧的正确动作不同，不要试图统一。

/// 判断字符串能否作为 HubState 的 namespace / key / scan prefix。
///
/// 规则：非空、最长 200 字节、只允许 `[A-Za-z0-9_.-]`。
///
/// 放行 `*` 是漏洞不是功能：前缀靠字符串拼接，通配符会让 `KvScan` 变成跨命名空间的
/// 模式匹配；冒号同理——它破坏前缀的结构。
///
/// 按字节遍历是安全的：白名单全是 ASCII，非 ASCII 字符的首字节必然 >= 0x80，
/// 会被 `default` 分支拒掉。
pub fn valid_state_segment(s: &str) -> bool {
    if s.is_empty() || s.len() > 200 {
        return false;
    }
    s.bytes().all(|c| {
        matches!(c,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'-')
    })
}

/// 判断 manifest 里的插件名是否合法。
///
/// 规则：非空、最长 64 字节、首字符是字母或数字、其余位置允许 `[A-Za-z0-9_-]`。
/// 与中台的 `crates/hub-registry/src/validate.rs` 的 `is_valid_plugin_name` 等价。
pub fn valid_plugin_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    name.char_indices().all(|(i, r)| match r {
        'a'..='z' | 'A'..='Z' | '0'..='9' => true,
        // i 是字节偏移，首个字符的偏移恒为 0，所以 i > 0 即「非首字符」
        '-' | '_' => i > 0,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 状态段拒绝通配符与冒号() {
        assert!(valid_state_segment("a.b-c_1"));
        assert!(!valid_state_segment("*"));
        assert!(!valid_state_segment("a:b"));
        assert!(!valid_state_segment(""));
        // 200 字节整好合法，201 就超了
        assert!(valid_state_segment(&"a".repeat(200)));
        assert!(!valid_state_segment(&"a".repeat(201)));
    }

    #[test]
    fn 插件名首字符不允许连字符与下划线() {
        assert!(valid_plugin_name("good-plugin"));
        assert!(valid_plugin_name("a_b-1"));
        assert!(!valid_plugin_name("-lead"));
        assert!(!valid_plugin_name("_lead"));
        assert!(!valid_plugin_name("中文名"));
        assert!(valid_plugin_name(&"a".repeat(64)));
        assert!(!valid_plugin_name(&"a".repeat(65)));
    }

    #[test]
    fn 中文按字节长度判定且一律非法() {
        // 非 ASCII 字符一律非法（白名单只有 ASCII），所以「字节数 vs 字符数」
        // 的差别不可观测——与 Go 侧一致。
        assert!(!valid_state_segment("中文"));
        assert!(!valid_plugin_name("插件"));
    }
}
