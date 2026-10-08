//! 模板渲染：把 `@@key@@` 占位符替换成实际取值。
//!
//! 同一份规则在 Go 侧（`sdk/go/cmd/hub-plugin/render.go`）也有一份实现，两边的
//! 行为必须一致——CLI 生成的工程与网页下载的工程是同一批文件，渲染结果不一致
//! 就是两条产品线。**改动这里请同步改那边**，并把占位符集合一起改。
//!
//! 用 `@@key@@` 而不是 Go 的 `{{.Key}}`：`{{`/`}}` 在 Node 模板与 C# 插值字符串里
//! 都可能撞，而 `@@` 与任何语言的语法都不冲突。

use std::fmt;

/// 占位符定界符。两侧相同，故只有一个常量有意义——分开写是为了对照 Go 侧的实现。
pub const PLACEHOLDER: &str = "@@";

/// 认得的占位符。模板里写了别的会报错，而不是留空。
pub const KEY_NAME: &str = "name";
pub const KEY_PACKAGE: &str = "package";
pub const KEY_SDK_MODULE: &str = "sdk_module";
/// 包内 SDK 源码的目录，`go.mod` 的 `replace` 指向它（如 `./sdk`）。
///
/// 与 [`KEY_SDK_MODULE`] 分开是必须的：module 路径要写进 `import` 语句里，
/// 那里**不能是相对路径**；而 `replace` 的目标必须是相对的，否则在开发者机器上
/// 就又是一个不存在的绝对路径。
pub const KEY_SDK_PATH: &str = "sdk_path";
pub const KEY_HUB_ADDR: &str = "hub_addr";
pub const KEY_ONBOARDING: &str = "onboarding";

/// 全部认得的关键字，用于生成给人看的报错。
pub const KNOWN_KEYS: [&str; 6] = [
    KEY_NAME,
    KEY_PACKAGE,
    KEY_SDK_MODULE,
    KEY_SDK_PATH,
    KEY_HUB_ADDR,
    KEY_ONBOARDING,
];

/// 渲染用的取值。
///
/// 每个字段都必须有值——包括 `hub_addr`：中台没配对外地址时，调用方要显式传一个
/// **可见的占位文字**（如「（中台未配置对外地址，请填写）」）进来，而不是让渲染留空。
/// 留空会生成一个 `HUB_ADDR=` 的工程，那种错误要等插件连不上中台时才暴露。
#[derive(Debug, Clone, Default)]
pub struct Values {
    pub name: String,
    pub package: String,
    pub sdk_module: String,
    pub sdk_path: String,
    pub hub_addr: String,
    pub onboarding: String,
}

impl Values {
    fn get(&self, key: &str) -> Option<&str> {
        let v = match key {
            KEY_NAME => &self.name,
            KEY_PACKAGE => &self.package,
            KEY_SDK_MODULE => &self.sdk_module,
            KEY_SDK_PATH => &self.sdk_path,
            KEY_HUB_ADDR => &self.hub_addr,
            KEY_ONBOARDING => &self.onboarding,
            _ => return None,
        };
        Some(v.as_str())
    }
}

/// 渲染失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// 模板里用了没定义的占位符。
    UnknownKey { key: String },
    /// 有一个 `@@` 没有配对的收尾。
    Unterminated { rest: String },
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey { key } => write!(
                f,
                "模板里有未定义的占位符 {PLACEHOLDER}{key}{PLACEHOLDER}（认得的只有 {}）",
                KNOWN_KEYS
                    .iter()
                    .map(|k| format!("{PLACEHOLDER}{k}{PLACEHOLDER}"))
                    .collect::<Vec<_>>()
                    .join(" / ")
            ),
            Self::Unterminated { rest } => write!(
                f,
                "模板里有一个没有收尾的 {PLACEHOLDER}：{:?}",
                head(rest, 40)
            ),
        }
    }
}

impl std::error::Error for RenderError {}

/// 把模板里的 `@@key@@` 替换成 `values` 里对应的值。
///
/// **未定义的占位符直接报错，不留空**：留空会生成一个 manifest 里插件名为空、
/// 或 import 路径残缺的工程，那种工程要等到编译或注册时才炸——而那时已经离
/// 「渲染」很远了，人不会想到回头怀疑模板。
pub fn render(raw: &str, values: &Values) -> Result<String, RenderError> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;

    loop {
        let Some(start) = rest.find(PLACEHOLDER) else {
            out.push_str(rest);
            return Ok(out);
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + PLACEHOLDER.len()..];

        let Some(end) = rest.find(PLACEHOLDER) else {
            return Err(RenderError::Unterminated {
                rest: rest.to_string(),
            });
        };
        let key = &rest[..end];
        rest = &rest[end + PLACEHOLDER.len()..];

        let Some(value) = values.get(key) else {
            return Err(RenderError::UnknownKey {
                key: key.to_string(),
            });
        };
        out.push_str(value);
    }
}

/// 截断一段文本用于报错，避免把整个模板正文甩进错误信息里。
fn head(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        Values {
            name: "order-reader".into(),
            package: "example.com/order-reader".into(),
            sdk_module: "example.com/sdk".into(),
            sdk_path: "./sdk".into(),
            hub_addr: "http://127.0.0.1:8093".into(),
            onboarding: "（共通层正文）".into(),
        }
    }

    #[test]
    fn 已知占位符被替换() {
        let got = render("module @@package@@\n// @@name@@", &values()).unwrap();
        assert_eq!(got, "module example.com/order-reader\n// order-reader");
    }

    #[test]
    fn 同一个占位符出现多次都被替换() {
        // README 里 @@name@@ 出现十几次，只换第一处是最容易写出的那种 bug
        let got = render("@@name@@-@@name@@-@@name@@", &values()).unwrap();
        assert_eq!(got, "order-reader-order-reader-order-reader");
    }

    #[test]
    fn 没有占位符时原样返回() {
        let raw = "package main\n\nfunc main() {}\n";
        assert_eq!(render(raw, &values()).unwrap(), raw);
    }

    #[test]
    fn 未定义的占位符报错而不是留空() {
        // 这一条是整套约定的核心：留空会生成一个要等到注册时才炸的工程
        let err = render("name: @@plugin_name@@", &values()).unwrap_err();
        assert_eq!(
            err,
            RenderError::UnknownKey {
                key: "plugin_name".into()
            }
        );
        assert!(err.to_string().contains("plugin_name"));
    }

    #[test]
    fn 没有收尾的占位符报错() {
        let err = render("name: @@name", &values()).unwrap_err();
        assert!(matches!(err, RenderError::Unterminated { .. }));
    }

    #[test]
    fn 空占位符也不算未定义以外的特例() {
        // @@@@ 里的 key 是空串，一样走「未定义」这条——不额外开特例，
        // 否则就是一个没人测过的分支
        let err = render("@@@@", &values()).unwrap_err();
        assert_eq!(err, RenderError::UnknownKey { key: String::new() });
    }

    #[test]
    fn 中台地址未配置时用的是调用方给的可见占位文字() {
        // 渲染不负责判断「有没有配」——它只负责「必须有值」。
        // 这条测试锁住的是：占位文字会原样出现在产物里，而不是被当成空串丢掉。
        let mut v = values();
        v.hub_addr = "（中台未配置对外地址，请改成中台插件面地址）".into();
        let got = render("HUB_ADDR=@@hub_addr@@", &v).unwrap();
        assert_eq!(got, "HUB_ADDR=（中台未配置对外地址，请改成中台插件面地址）");
    }
}
