//! 外置状态（`HubState`）的**跨 crate 共用**部分：键名规则与服务端上限。
//!
//! **为什么单独放这里，而不是留在 `hub-grpc`**：本地 mock 中台也要用**同一份**规则。
//! 判据同源是 mock 的立身之本（见 `crates/hub-mock/src/lib.rs` 开头那段）。真中台
//! 与 mock 对同一个键给出不同判定，是最坏的一类不一致——开发者在本地验过的东西，
//! 到线上被拒，而两边都没有报错。
//!
//! 而 mock **不能**依赖 `hub-grpc`——那会把 tonic 服务端连同 redis、sqlx 一起拖进一个
//! 本该「一个进程、无外部依赖」的替身里。`hub-core` 是零依赖的，放这里两边都够得着。
//!
//! 规则本身的事实来源是 `sdk/go/hubkit/testdata/hub-rules.json`，各语言的实现都由
//! 契约测试钉住（`crates/hub-grpc/tests/state_rules.rs` 与 `sdk/go/hubkit/rules_test.go`
//! 读的是同一份文件）。

/// 状态凭证的 metadata 键。
///
/// 插件注册成功后从回执里拿到 `state_token`，之后**每个**状态请求都要带在 gRPC
/// metadata 的这个键上。中台按它反查插件名——身份来自凭证，不是插件自报的字段
/// （插件面本就不鉴权，自报的插件名不构成身份）。
pub const STATE_TOKEN_METADATA: &str = "x-hub-state-token";

/// 键前缀。中台按凭证反查出插件名后**强制**拼上，插件自报的 `namespace` 只能作为
/// 子空间。取这个值是让它与总线的键空间（`hub:flows`）天然隔离。
pub const KEY_PREFIX: &str = "hub:state";

/// 值上限：HubState 不是对象存储，别让插件把它当桶用。
///
/// 客户端也该拿它做前置拦截——让开发者在本地就看到一句能懂的错，而不是等服务端回
/// 一个 `INVALID_ARGUMENT`。
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;

/// 单次扫描的硬上限，防止一次拉走整个命名空间。
pub const MAX_SCAN_LIMIT: u32 = 1000;

/// 命名空间与键的字符白名单。
///
/// **放行 `*` 是漏洞不是功能**：前缀靠字符串拼接，通配符会让 `KvScan` 变成跨
/// 命名空间的模式匹配，前缀隔离被一个星号绕过。冒号同理——它破坏前缀的结构。
///
/// 长度用 `s.len()`（字节）而非字符数：白名单只放 ASCII，所以对合法输入两者相等；
/// 非法输入无论先撞哪一条，结果都是 `false`。这个论证写在契约文件的 `_byte_vs_char`
/// 那段里，各语言的实现因此不必在字节与字符之间纠结。
pub fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 白名单只放字母数字与三个符号() {
        for ok in ["s", "A1", "a_b", "a.b", "a-b", "0", "___"] {
            assert!(valid_segment(ok), "{ok:?} 应合法");
        }
        // 这几个不是「不好看」，是**会破坏前缀结构**：放行 `*` 就能跨命名空间扫，
        // 放行 `:` 就能伪造前缀的层级
        for bad in ["", "*", "a:b", "a b", "a/b", "中文", "a@b", "a\nb"] {
            assert!(!valid_segment(bad), "{bad:?} 应非法");
        }
    }

    #[test]
    fn 长度上限是两百字节() {
        assert!(valid_segment(&"a".repeat(200)));
        assert!(!valid_segment(&"a".repeat(201)));
    }

    #[test]
    fn 上限常量就是契约里写的那些() {
        // 钉住数值：客户端与 mock 都从这几个常量取，改它们等于改契约
        assert_eq!(MAX_VALUE_BYTES, 1024 * 1024);
        assert_eq!(MAX_SCAN_LIMIT, 1000);
        assert_eq!(KEY_PREFIX, "hub:state");
        assert_eq!(STATE_TOKEN_METADATA, "x-hub-state-token");
    }
}
