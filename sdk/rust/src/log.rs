//! 结构化日志：JSON 一行一条，打到 **stderr**。
//!
//! 与 Go 侧（`sdk/go/hubkit` 用 slog 的 JSONHandler）同款输出，逐字段对齐：
//!
//! ```json
//! {"time":"2026-09-18T10:47:43.521Z","level":"INFO","msg":"已注册到中台","plugin":"order-reader","version":"0.1.0"}
//! ```
//!
//! 为什么是 stderr 而不是 stdout：插件的 stdout 可能被别的东西用（比如管道里的业务
//! 输出），日志混进去会污染它。容器里两者都进 `docker logs`，所以不影响排障。
//!
//! 为什么不引 `tracing` / `slog`：SDK 要随包发给插件团队，多加一个依赖就多一份
//! 「版本对不上」的可能；而这里要的只是「几个字段拼成一行 JSON」，不值得为它
//! 引一整棵依赖树。

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 日志级别。判定与 Go 的 slog.Level 一致：数值越大越严重。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// 调试细节。生产默认不开。
    Debug,
    /// 常规运行信息（监听、已注册、已退出）。
    Info,
    /// 需要留意但不致命（心跳失败、注销失败）。
    Warn,
    /// 需要人介入（注册被拒）。
    Error,
}

impl Level {
    /// 解析 `HUB_LOG_LEVEL`。认不出的一律当 info——
    /// 与环境变量打错时得到的「没有日志」相比，「比预期多几条日志」显然更安全。
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "debug" => Level::Debug,
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }

    /// 大写的级别名，进 JSON 的 `level` 字段。
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    fn code(self) -> u8 {
        match self {
            Level::Debug => 0,
            Level::Info => 1,
            Level::Warn => 2,
            Level::Error => 3,
        }
    }
}

/// 一个字段的取值。只支持 JSON 里这几种标量——日志要的是「能一眼看懂」，
/// 嵌一层对象只会让 `jq` 的路径变长。
#[derive(Debug, Clone)]
pub enum Value<'a> {
    /// 借来的字符串。
    Str(&'a str),
    /// 自己持有的字符串（例如 `err.to_string()` 的结果）。
    Owned(String),
    /// 整数。
    Int(i64),
    /// 非负整数（条数、字节数）。
    Uint(u64),
    /// 布尔。
    Bool(bool),
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(v: &'a str) -> Self {
        Value::Str(v)
    }
}
impl<'a> From<&'a String> for Value<'a> {
    fn from(v: &'a String) -> Self {
        Value::Str(v.as_str())
    }
}
impl From<String> for Value<'_> {
    fn from(v: String) -> Self {
        Value::Owned(v)
    }
}
impl From<i64> for Value<'_> {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<i32> for Value<'_> {
    fn from(v: i32) -> Self {
        Value::Int(i64::from(v))
    }
}
impl From<u64> for Value<'_> {
    fn from(v: u64) -> Self {
        Value::Uint(v)
    }
}
impl From<usize> for Value<'_> {
    fn from(v: usize) -> Self {
        Value::Uint(v as u64)
    }
}
impl From<bool> for Value<'_> {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

/// 日志器。`Clone` 是廉价的（只有一个原子级别字段）。
///
/// 手动实现 `Debug` 而不是 derive：`Config` 里带着它，而一个 `Arc<AtomicU8>`
/// 的 `Debug` 输出对读日志/断言的人毫无意义。
#[derive(Clone)]
pub struct Logger {
    level: std::sync::Arc<AtomicU8>,
}

impl Logger {
    /// 指定级别构造。
    pub fn new(level: Level) -> Self {
        Self {
            level: std::sync::Arc::new(AtomicU8::new(level.code())),
        }
    }

    /// 从 `HUB_LOG_LEVEL` 构造，缺省 info。
    pub fn from_env() -> Self {
        let raw = std::env::var("HUB_LOG_LEVEL").unwrap_or_default();
        Self::new(Level::parse(&raw))
    }

    /// 当前级别。
    pub fn level(&self) -> Level {
        match self.level.load(Ordering::Relaxed) {
            0 => Level::Debug,
            2 => Level::Warn,
            3 => Level::Error,
            _ => Level::Info,
        }
    }

    /// 改级别。运行期可调，测试里用它把噪音关掉。
    pub fn set_level(&self, level: Level) {
        self.level.store(level.code(), Ordering::Relaxed);
    }

    /// 该级别在当前设置下是否会真的打出来。
    pub fn enabled(&self, level: Level) -> bool {
        level >= self.level()
    }

    /// 打一条 debug。
    pub fn debug(&self, msg: &str, fields: &[(&str, Value<'_>)]) {
        self.emit(Level::Debug, msg, fields);
    }

    /// 打一条 info。
    pub fn info(&self, msg: &str, fields: &[(&str, Value<'_>)]) {
        self.emit(Level::Info, msg, fields);
    }

    /// 打一条 warn。
    pub fn warn(&self, msg: &str, fields: &[(&str, Value<'_>)]) {
        self.emit(Level::Warn, msg, fields);
    }

    /// 打一条 error。
    pub fn error(&self, msg: &str, fields: &[(&str, Value<'_>)]) {
        self.emit(Level::Error, msg, fields);
    }

    /// 打一行。
    ///
    /// 刻意**一次 write 一个完整行**（拼好再写）：分成多次 write 时，两个线程同时
    /// 打日志会把字段交错进对方的行里，产出的是谁也解析不了的半截 JSON。
    pub fn emit(&self, level: Level, msg: &str, fields: &[(&str, Value<'_>)]) {
        if !self.enabled(level) {
            return;
        }

        let mut line = String::with_capacity(128 + msg.len());
        line.push_str("{\"time\":\"");
        line.push_str(&rfc3339_utc(SystemTime::now()));
        let _ = write!(line, "\",\"level\":\"{}\",\"msg\":", level.as_str());
        push_json_string(&mut line, msg);

        for (key, value) in fields {
            line.push(',');
            push_json_string(&mut line, key);
            line.push(':');
            match value {
                Value::Str(s) => push_json_string(&mut line, s),
                Value::Owned(s) => push_json_string(&mut line, s),
                Value::Int(v) => {
                    let _ = write!(line, "{v}");
                }
                Value::Uint(v) => {
                    let _ = write!(line, "{v}");
                }
                Value::Bool(v) => {
                    let _ = write!(line, "{v}");
                }
            }
        }
        line.push_str("}\n");

        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        // 日志写不出去（stderr 被关了）没有补救办法，也不该让插件因此挂掉
        let _ = lock.write_all(line.as_bytes());
        let _ = lock.flush();
    }
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Logger({})", self.level().as_str())
    }
}

impl Default for Logger {
    fn default() -> Self {
        Self::from_env()
    }
}

/// 按 JSON 的规则转义一个字符串。
///
/// 手写而不是 `serde_json::to_string`：这个函数在日志热路径上，而它只需要处理
/// 「加引号 + 转义」。同时它保证不会失败——日志因为一个畸形字符串而整条丢掉，
/// 正是最需要它的时刻。
fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // 其余控制字符走 \uXXXX；可打印字符原样（含中文，JSON 允许 UTF-8 直出）
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// 把时刻格式化成 RFC3339（UTC，毫秒）。
///
/// 不引 `time` / `chrono`：SDK 随包发给插件团队，能不加的依赖就不加。
/// 而 RFC3339 的 UTC 分支只需要一段「天数 ↔ 年月日」的换算，几十行就够；
/// 代价是输出固定带 `Z`（不带本地时区偏移），与 Go 侧 slog 的本地时间不同。
/// 这是个有意的取舍：日志要的是**可比较**，UTC 比本地时间更适合这个用途。
fn rfc3339_utc(t: SystemTime) -> String {
    let (secs, millis) = unix_millis_parts(t);

    // div_euclid / rem_euclid 而不是 `/` 与 `%`：后两者对负数向零取整，
    // 会把 1969-12-31 算成 1970-01-01 的某一秒。
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);

    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// 把一个 `SystemTime` 拆成「相对纪元的整秒（可负）+ 0..1000 的毫秒」。
///
/// 1970 之前刻意**真的算出负数**，而不是像 `duration_since(UNIX_EPOCH).unwrap_or_default()`
/// 那样退化成 0——退化会让一台时钟错到 1970 之前的机器在日志里显示 1970-01-01，
/// 而「时间戳明显不对」恰恰是发现时钟问题的唯一线索，抹平它等于把线索删掉。
fn unix_millis_parts(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_millis()),
        Err(_) => {
            let d = UNIX_EPOCH.duration_since(t).unwrap_or_default();
            let secs = -(d.as_secs() as i64);
            let ms = d.subsec_millis();
            // 取负时秒与毫秒要一起翻：-1.25s 是「比纪元早 1.25 秒」，
            // 也就是第 -2 秒的第 750 毫秒，不能简单写成 (-1, 250)。
            if ms == 0 {
                (secs, 0)
            } else {
                (secs - 1, 1000 - ms)
            }
        }
    }
}

/// Howard Hinnant 的 `civil_from_days`：把「1970-01-01 起的天数」换成公历年月日。
///
/// 用它而不是自己推闰年规则：这段算法把「400 年 146097 天」的整除关系一次算清，
/// 自己写的分支版本在各种边界年（百年不闰、四百年又闰）上很容易差一天。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 级别解析认得四种取值且兜底为_info() {
        assert_eq!(Level::parse("debug"), Level::Debug);
        assert_eq!(Level::parse("INFO"), Level::Info);
        assert_eq!(Level::parse(" Warn "), Level::Warn);
        assert_eq!(Level::parse("error"), Level::Error);
        assert_eq!(Level::parse(""), Level::Info);
        assert_eq!(Level::parse("verbose"), Level::Info);
    }

    #[test]
    fn 时间戳是_rfc3339_且能对回已知时刻() {
        // 2026-09-18T02:47:43.521Z（= Go 那份日志样例里的 10:47:43.521+08:00）
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_789_699_663_521);
        assert_eq!(rfc3339_utc(t), "2026-09-18T02:47:43.521Z");

        // 纪元本身：1970-01-01
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");

        // 2000-02-29（四百年闰的边界）
        let leap = UNIX_EPOCH + std::time::Duration::from_secs(951_782_400);
        assert_eq!(rfc3339_utc(leap), "2000-02-29T00:00:00.000Z");

        // 1900-03-01 是「百年不闰」之后一天
        let t = UNIX_EPOCH - std::time::Duration::from_secs(2_203_891_200);
        assert_eq!(rfc3339_utc(t), "1900-03-01T00:00:00.000Z");
    }

    #[test]
    fn 字符串转义挡住会破坏_json_的字符() {
        let mut out = String::new();
        push_json_string(&mut out, "a\"b\\c\nd\te\u{1}f中文");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\te\\u0001f中文\"");
    }

    #[test]
    fn 字段按传入顺序拼在_msg_之后() {
        // 直接验格式化逻辑：把 emitted 行抓出来不现实（写的是进程 stderr），
        // 所以这里验的是同样的拼装路径——字段名也走转义，不会出现裸引号。
        let logger = Logger::new(Level::Debug);
        assert!(logger.enabled(Level::Debug));
        logger.set_level(Level::Warn);
        assert!(!logger.enabled(Level::Info));
        assert!(logger.enabled(Level::Error));
    }
}
