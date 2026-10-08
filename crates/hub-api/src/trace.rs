//! W3C Trace Context（`traceparent`）的解析与生成。
//!
//! 中台自己生成的 trace 用 ULID；但当调用方（上游系统、agent）已经带了一条 trace 时，
//! 应当**沿用**它——排障时最怕的就是「同一个请求在两个系统里是两个 id」，
//! 那样两边的日志永远串不起来。

/// 一条 trace 上下文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    /// 32 位十六进制
    pub trace_id: String,

    /// 16 位十六进制，本次调用的父 span
    pub parent_span_id: String,
}

/// HTTP 头名。
pub const TRACEPARENT_HEADER: &str = "traceparent";

/// 生成一个 W3C 合规的 trace id：32 位十六进制。
///
/// **不能直接用 ULID 的字符串形式**——它是 Crockford base32，含超出 `f` 的字母，
/// 放到 `traceparent` 里是非法值。这里取 ULID 的 128 位原始值转成十六进制，
/// 既保留随机性与时间有序性，又满足 W3C 的格式要求。
pub fn new_trace_id() -> String {
    format!("{:032x}", ulid::Ulid::generate().0)
}

/// 生成本次调用的 span id：16 位十六进制。
pub fn new_span_id() -> String {
    format!(
        "{:016x}",
        (ulid::Ulid::generate().0 & u64::MAX as u128) as u64
    )
}

/// 解析 `traceparent` 头。
///
/// 格式：`00-<32 位十六进制 trace-id>-<16 位十六进制 span-id>-<2 位十六进制标志>`
///
/// **不合法时返回 `None` 而不是报错**：一个坏 header 不该让请求失败，
/// 退化成「自己生成一条新 trace」就够了。
pub fn parse_traceparent(value: &str) -> Option<TraceContext> {
    let value = value.trim();
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() < 4 {
        return None;
    }

    let (version, trace_id, parent_span_id) = (parts[0], parts[1], parts[2]);

    if version.len() != 2 || !is_hex(version) {
        return None;
    }
    // 全 0 的 id 是无效值（W3C 明确规定），见到就当作没带
    if trace_id.len() != 32 || !is_hex(trace_id) || trace_id.chars().all(|c| c == '0') {
        return None;
    }
    if parent_span_id.len() != 16
        || !is_hex(parent_span_id)
        || parent_span_id.chars().all(|c| c == '0')
    {
        return None;
    }
    if parts[3].len() != 2 || !is_hex(parts[3]) {
        return None;
    }

    Some(TraceContext {
        trace_id: trace_id.to_ascii_lowercase(),
        parent_span_id: parent_span_id.to_ascii_lowercase(),
    })
}

/// 生成 `traceparent` 头的值。
pub fn format_traceparent(trace_id: &str, span_id: &str) -> String {
    format!("00-{trace_id}-{span_id}-01")
}

fn is_hex(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn 解析合法的_traceparent() {
        let ctx = parse_traceparent(VALID).expect("应能解析");
        assert_eq!(ctx.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(ctx.parent_span_id, "00f067aa0ba902b7");
    }

    #[test]
    fn 前后空白被容忍() {
        assert!(parse_traceparent(&format!("  {VALID}  ")).is_some());
    }

    #[test]
    fn 大写十六进制被归一化为小写() {
        let ctx = parse_traceparent("00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01")
            .expect("应能解析");
        assert_eq!(ctx.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[test]
    fn 缺字段被拒() {
        assert!(parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736").is_none());
        assert!(parse_traceparent("").is_none());
        assert!(parse_traceparent("garbage").is_none());
    }

    #[test]
    fn 长度不对被拒() {
        // trace-id 少一位
        assert!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01").is_none()
        );
        // span-id 多一位
        assert!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7a-01").is_none()
        );
        // 版本号长度不对
        assert!(
            parse_traceparent("0-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").is_none()
        );
    }

    #[test]
    fn 非十六进制被拒() {
        assert!(
            parse_traceparent("00-zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-00f067aa0ba902b7-01").is_none()
        );
    }

    #[test]
    fn 全零的_id_被拒() {
        // W3C 明确规定全 0 无效；见到就当没带，自己生成一条
        assert!(
            parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none()
        );
        assert!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01").is_none()
        );
    }

    #[test]
    fn 生成的值能被自己解析回来() {
        let trace_id = new_trace_id();
        let span_id = new_span_id();

        let header = format_traceparent(&trace_id, &span_id);
        let ctx = parse_traceparent(&header).expect("自己生成的应能解析");
        assert_eq!(ctx.trace_id, trace_id);
        assert_eq!(ctx.parent_span_id, span_id);
    }

    #[test]
    fn 生成的_id_长度与字符集符合_w3c() {
        assert_eq!(new_trace_id().len(), 32);
        assert!(is_hex(&new_trace_id()));
        assert_eq!(new_span_id().len(), 16);
        assert!(is_hex(&new_span_id()));

        // 连着生成不该重复
        assert_ne!(new_trace_id(), new_trace_id());
        assert_ne!(new_span_id(), new_span_id());
    }
}
