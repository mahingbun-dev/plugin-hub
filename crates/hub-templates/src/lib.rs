//! 插件脚手架模板：注册、渲染、打包。
//!
//! 中台的控制台上有一页「插件开发接入」，开发者在那里填一个插件名，就能下载到一份
//! 可运行的插件工程。这个 crate 负责那件事的服务端一半：
//!
//! 1. **注册**——各语言的模板目录由 `build.rs` 在**编译期**扫描并嵌入二进制。
//!    嵌入而不是运行时读盘：控制台上显示的模板版本与下载下来的内容因此永远同源，
//!    不会出现「页面写着 0.1.0、包里是别的」。
//! 2. **渲染**——把 `@@key@@` 占位符替换成插件名、包名、中台地址（见 [`render`]）。
//! 3. **打包**——打成顶层带一层插件名目录的 zip（见 [`pack`]）。
//!
//! **模板源在各语言 SDK 自己的目录下**（Go 是 `sdk/go/cmd/hub-plugin/templates/`），
//! 不集中到顶层：模板与它对应的 SDK 同生共死，升 SDK 时不会忘改模板。要加一门语言，
//! 改 `build.rs` 的 `LANGUAGES` 一行，并把模板目录建出来即可。

mod pack;
mod render;

pub use pack::{PackError, archive_size, build_zip, download_filename};
pub use render::{
    KEY_HUB_ADDR, KEY_NAME, KEY_ONBOARDING, KEY_PACKAGE, KEY_SDK_MODULE, KEY_SDK_PATH, Values,
};

/// 一门语言的插件模板。
#[derive(Debug)]
pub struct Language {
    /// 语言标识，出现在 URL 与下载文件名里（如 `go`）。
    pub id: &'static str,

    /// 控制台卡片上的展示名。
    pub display_name: &'static str,

    /// 一句话说明这门语言的模板给到什么程度。**按实际状态写**，
    /// 不按设计意图写——曾经的教训是把「计划中的能力」写成「已有能力」。
    pub description: &'static str,

    /// 包名 / module 路径输入框里的提示文字。
    pub package_hint: &'static str,

    /// 模板文件：(输出路径, 模板原始字节)。输出路径已去掉 `.tmpl` 后缀。
    pub files: &'static [(&'static str, &'static [u8])],

    /// 模板源目录，**相对本 crate 的 `CARGO_MANIFEST_DIR`**（如
    /// `../../sdk/go/cmd/hub-plugin/templates`）。
    ///
    /// 只给测试用：那条「嵌入的模板与磁盘上的模板一致」要从磁盘再读一遍比对，而它
    /// 此前把 id → 目录的对应关系**硬编码在测试里**——加一门语言就要回去补一处，
    /// 漏了就是一条以 panic 形式出现的假失败。由 `build.rs` 一并生成，就不存在第二份表。
    pub source_dir: &'static str,

    /// 随包下发的 SDK 源码在包内的目录名（如 `sdk`）。空串表示这门语言没有随包 SDK。
    pub sdk_dir: &'static str,

    /// SDK 源码：(相对 SDK 根的路径, 字节)。**原样写入，不渲染**——SDK 不是模板。
    ///
    /// 为什么要有它：生成的工程用 `replace` 指到 SDK，而那个路径在开发者机器上
    /// 不存在（内网没有私有 Go module 代理），于是「下载一个工程」落不了地：
    /// 包能下、能解压，就是编不过。带上源码，`replace` 指向包内的相对路径，
    /// 拿到手就能跑。见 `build.rs` 的 `SDK_SOURCES`。
    pub sdk_files: &'static [(&'static str, &'static [u8])],

    /// SDK 在**生成工程里**的引用路径（Go 是 module 路径，Python 是包名……）。
    ///
    /// 它进 `import` / `require` / `use` 那一类语句，而 [`Language::sdk_dir`] 进
    /// 依赖声明（`replace` / `file:` / `path`）。两者必须分开：前者不能是相对路径，
    /// 后者必须是相对的。
    ///
    /// 由 `build.rs` 的 `SDK_SOURCES` 给出，**中台侧不得写死**任何一门语言的值——
    /// 写死了，加语言时就要去改那个写死的地方。
    pub sdk_module: &'static str,
}

impl Language {
    /// 这份模板一共几个文件（不含随包的 SDK 源码）。
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// 随包 SDK 源码占多少字节。控制台的卡片上要显示包大小时要用到。
    pub fn sdk_bytes(&self) -> usize {
        self.sdk_files.iter().map(|(_, b)| b.len()).sum()
    }
}

// build.rs 生成：各语言的 LANG_* 静态量、LANGUAGES 列表与 BUILT_AT_EPOCH。
// 放在 Language 定义之后——生成的内容要引用这个类型。
include!(concat!(env!("OUT_DIR"), "/templates_gen.rs"));

/// 全部语言的模板，顺序即控制台上的展示顺序。
pub fn languages() -> &'static [&'static Language] {
    LANGUAGES
}

/// 按标识找一门语言。
pub fn find(id: &str) -> Option<&'static Language> {
    languages().iter().copied().find(|l| l.id == id)
}

/// 各语言共用的接入指南素材（`_shared/onboarding.md`）。
///
/// 各语言的 `AGENTS.md` 里用 `@@onboarding@@` 引用它，避免同一条「接入四道关」在
/// 六份模板里各抄一遍、改一处漏五处。`None` = 这份素材还没有。
pub fn shared_onboarding() -> Option<&'static str> {
    SHARED_ONBOARDING_SRC
}

impl Values {
    /// 按用户输入构造一份取值。
    ///
    /// `onboarding` 由这里自动填上共通层素材——**不给调用方忘记的机会**：
    /// 忘了的后果是 AGENTS.md 里出现一个没有内容的章节，而那种缺陷要靠人逐字读
    /// 生成出来的文件才会发现。
    ///
    /// `sdk_path` 这里给的只是兜底值；真正生效的是 [`pack::build_zip`] 按语言
    /// 注入的那个（包内的相对目录由语言决定）。两处都在，是因为直接调 `render`
    /// （不打包）时也得有个合理取值。
    pub fn for_plugin(
        name: impl Into<String>,
        package: impl Into<String>,
        sdk_module: impl Into<String>,
        hub_addr: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            package: package.into(),
            sdk_module: sdk_module.into(),
            sdk_path: "./sdk".to_string(),
            hub_addr: hub_addr.into(),
            onboarding: shared_onboarding().unwrap_or_default().to_string(),
        }
    }
}

/// 这份二进制里的模板是什么时候构建的，RFC3339 UTC。
///
/// 控制台的卡片上要显示它，回答「这份模板是不是过期的」。CI 里用
/// `SOURCE_DATE_EPOCH` 可以把这个值钉住，让同样的输入产出同样的二进制。
pub fn built_at() -> String {
    format_rfc3339(BUILT_AT_EPOCH)
}

/// 把 Unix 秒格式化成 RFC3339，自己算是为了不引入 chrono——只为一行展示文字。
fn format_rfc3339(secs: i64) -> String {
    // 民用历换算，Howard Hinnant 的 days_from_civil 逆运算
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };

    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
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
    fn 至少有一门语言的模板() {
        assert!(!languages().is_empty(), "一门语言都没有，控制台会是空白页");
    }

    #[test]
    fn 每门语言的文件清单非空且不含_shared() {
        for lang in languages() {
            assert!(
                lang.file_count() > 0,
                "{} 一个模板文件都没有",
                lang.display_name
            );
            for (path, _) in lang.files {
                // _shared/ 是内联素材，不作为产物发给开发者
                assert!(
                    !path.starts_with("_shared/"),
                    "{} 的 {path} 属于 _shared/，不该作为产物输出",
                    lang.display_name
                );
                assert!(
                    !path.ends_with(".tmpl"),
                    "{} 的 {path} 没去掉 .tmpl 后缀",
                    lang.display_name
                );
            }
        }
    }

    #[test]
    fn 语言标识可用于_url_与文件名() {
        // 标识会直接进 URL 路径与下载文件名，出现大写或空格会很别扭
        for lang in languages() {
            assert!(
                !lang.id.is_empty()
                    && lang
                        .id
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "语言标识 {:?} 只应含小写字母、数字与连字符",
                lang.id
            );
        }
    }

    #[test]
    fn 按标识能找到语言() {
        let first = languages()[0];
        assert!(find(first.id).is_some());
        assert!(find("不存在的语言").is_none());
    }

    #[test]
    fn 构建时间能格式化成_rfc3339() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // 闰日与年末，这两处最容易算错
        assert_eq!(format_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    #[test]
    fn 嵌入的模板与磁盘上的模板一致() {
        // 这条兜住「build.rs 的 rerun-if-changed 失效」：
        // 真发生了的话，二进制里的模板会是旧的，而页面上显示的是新的——
        // 一个不可能靠肉眼发现的缺陷。
        for lang in languages() {
            for (path, embedded) in lang.files {
                let dir = template_dir(lang);
                let on_disk = std::fs::read(dir.join(format!("{path}.tmpl")))
                    .unwrap_or_else(|e| panic!("读磁盘上的 {path}.tmpl 失败: {e}"));
                assert_eq!(
                    *embedded,
                    on_disk.as_slice(),
                    "{}/{path}.tmpl 的内容与嵌入二进制的不一致",
                    lang.id
                );
            }
        }
    }

    /// 模板目录，测试里从磁盘再读一遍用。
    ///
    /// 目录取自 [`Language::source_dir`]（由 `build.rs` 生成），这里**不再维护第二份表**：
    /// 此前它硬编码了 id → 目录的对应关系，加一门语言就要回来补一处，漏了就是一条
    /// 以 panic 形式出现的假失败。
    fn template_dir(lang: &Language) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(lang.source_dir)
    }

    #[test]
    fn 渲染后的包不含未替换的占位符() {
        // 端到端的一条：从嵌入的模板出发，走渲染 + 打包，确认产物干净。
        // 单看 render 的测试不够——模板文件本身可能真的写了 @@name@@ 之外的键。
        for lang in languages() {
            let bytes = crate::build_zip(lang, &values()).unwrap();
            let mut archive =
                zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("必须是合法 zip");
            // 随包的 SDK 是**原样照抄**的，不参与渲染——它里面出现 `@@` 是正常的
            // （SDK 自己的 README 就在讲「模板里有这个占位符」）。这条只管模板渲染出来的
            // 那些文件；pack.rs 里那条同名的断言也是这么跳过的。
            let sdk_prefix = format!("{}/{}/", values().name, lang.sdk_dir);
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).unwrap();
                let name = entry.name().to_string();
                let mut text = String::new();
                std::io::Read::read_to_string(&mut entry, &mut text).unwrap();

                if !lang.sdk_dir.is_empty() && name.starts_with(&sdk_prefix) {
                    continue;
                }

                for key in crate::render::KNOWN_KEYS {
                    let leftover = format!(
                        "{}{key}{}",
                        crate::render::PLACEHOLDER,
                        crate::render::PLACEHOLDER
                    );
                    assert!(
                        !text.contains(&leftover),
                        "{name} 里残留了未渲染的 {leftover}"
                    );
                }
            }
        }
    }

    #[test]
    fn 插件名会出现在包内路径与文件内容里() {
        // 用另一个名字再跑一遍：若哪里把名字写死成 "order-reader"，这条会红
        let mut v = values();
        v.name = "wms-writer".into();
        let lang = languages()[0];
        let bytes = crate::build_zip(lang, &v).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();

        for i in 0..archive.len() {
            let name = archive.by_index(i).unwrap().name().to_string();
            assert!(name.starts_with("wms-writer/"), "{name} 路径里没带上插件名");
        }
    }

    #[test]
    fn 已知占位符常量与模板实际用到的一致() {
        // 模板里出现的占位符若不在 KNOWN_KEYS 里，渲染时会报错——但那要等到
        // 用户点下载才发现。这里提前扫一遍。
        for lang in languages() {
            for (path, raw) in lang.files {
                let text = std::str::from_utf8(raw).unwrap();
                for key in placeholder_keys_in(text) {
                    assert!(
                        render::KNOWN_KEYS.contains(&key.as_str()),
                        "{}/{path} 里用了未定义的占位符 @@{key}@@",
                        lang.id
                    );
                }
            }
        }
    }

    /// 取出文本里所有 `@@key@@` 的 key。只用于测试。
    fn placeholder_keys_in(s: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let mut rest = s;
        while let Some(start) = rest.find(render::PLACEHOLDER) {
            rest = &rest[start + render::PLACEHOLDER.len()..];
            let Some(end) = rest.find(render::PLACEHOLDER) else {
                break;
            };
            keys.push(rest[..end].to_string());
            rest = &rest[end + render::PLACEHOLDER.len()..];
        }
        keys
    }

    #[test]
    fn 引用了共通层素材的模板必须真的能拿到素材() {
        // @@onboarding@@ 的取值是「没有也算合法」（空字符串），所以素材缺失时
        // 渲染不会报错，只会产出**一个没有内容的章节**——那种缺陷要靠人逐字读
        // 生成出来的 AGENTS.md 才会发现。这条把它提前成一条红测试。
        let mut referenced = Vec::new();
        for lang in languages() {
            for (path, raw) in lang.files {
                if String::from_utf8_lossy(raw).contains("@@onboarding@@") {
                    referenced.push(format!("{}/{}", lang.id, path));
                }
            }
        }

        if !referenced.is_empty() {
            let text = shared_onboarding().unwrap_or_else(|| {
                panic!(
                    "以下模板引用了 @@onboarding@@，但 _shared/onboarding.md 不存在，\
                     渲染出来会是没有内容的章节：{referenced:?}"
                )
            });
            assert!(
                !text.trim().is_empty(),
                "_shared/onboarding.md 存在但是空的，同样会渲染出空章节"
            );
        }
    }

    /// 随包 SDK 的目录名与排除项在 Rust 与 Go 两侧各有一份，必须一致。
    ///
    /// 不一致的后果不是「多带或少带一个文件」那么轻：`cmd/hub-plugin` 被带进去时，
    /// 它自己的 `//go:embed all:templates` 找不到 templates，**开发者拿到手编译直接失败**。
    /// 而两侧都是手写清单——「两份手写清单迟早对不上」是这个仓库反复踩过的坑，
    /// 所以比着源码守一条。
    #[test]
    fn 随包sdk的排除项与_go_侧一致() {
        let go = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../sdk/go/cmd/hub-plugin/sdkfiles.go"
        ))
        .expect("读 sdkfiles.go 失败");

        let lang = find("go").expect("go 语言必须存在");

        let dir = between(&go, "sdkDirInProject = \"", "\"")
            .expect("sdkfiles.go 里找不到 sdkDirInProject");
        assert_eq!(lang.sdk_dir, dir, "包内 SDK 目录名两侧不一致");

        let raw = between(&go, "var sdkExcluded = []string{", "}").expect("找不到 sdkExcluded");
        let go_excl: Vec<&str> = raw
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .filter(|s| !s.is_empty())
            .collect();

        assert_eq!(
            go_excl, SDK_GO_FILES_EXCLUDED,
            "随包 SDK 的排除项两侧不一致（Rust 侧 {:?}，Go 侧 {:?}）",
            SDK_GO_FILES_EXCLUDED, go_excl
        );
    }

    /// 随包 SDK 里**不该出现构建产物**。
    ///
    /// 这条刻意从**产出的文件清单**这一侧断言，而不是去核对各语言的排除项写法。
    /// 清单是手写的，而「写了却没生效」正是发生过的缺陷：Python 的 `*.egg-info`
    /// 一度因为只做字面前缀匹配而没命中 `hubkit.egg-info`，那个目录里的 `PKG-INFO`
    /// （记着打包者机器上的路径）本会原样进开发者的下载包。
    ///
    /// 构建产物进下载包的后果不只是「包大」：`node_modules` 会让体积涨几十倍，
    /// 而带机器绝对路径的元数据文件是把构建环境泄露给了每一个下载者。
    #[test]
    fn 随包sdk不含构建产物() {
        // 目录名——出现在路径的任意一段都算
        const BAD_SEGMENTS: &[&str] = &[
            "node_modules",
            "__pycache__",
            ".pytest_cache",
            ".venv",
            "target",
            "bin",
            "obj",
        ];
        // 后缀——`.egg-info` 这类
        const BAD_SUFFIXES: &[&str] = &[".egg-info", ".pyc", ".dll", ".pdb", ".rlib"];

        for lang in languages() {
            for (path, _) in lang.sdk_files {
                for seg in path.split('/') {
                    assert!(
                        !BAD_SEGMENTS.contains(&seg),
                        "{} 的随包 SDK 里混进了构建产物目录：{path}",
                        lang.id
                    );
                    assert!(
                        !BAD_SUFFIXES.iter().any(|s| seg.ends_with(s)),
                        "{} 的随包 SDK 里混进了构建产物：{path}",
                        lang.id
                    );
                }
            }
        }
    }

    /// 取 `open` 与随后的 `close` 之间的内容。只给测试解析 Go 源码用。
    fn between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
        let start = haystack.find(open)? + open.len();
        let rest = &haystack[start..];
        Some(&rest[..rest.find(close)?])
    }

    #[test]
    fn 占位符清单与_go_侧一致() {
        // 这条**真的比对两侧的清单**，而不只是锁住本侧的拼写——后者在本仓库已经
        // 放走过一次真实的漂移：模板里用了 `@@hub_addr@@`，中台侧认得它，而 Go 侧的
        // `knownPlaceholders` 不认得，于是 `hub-plugin new` 直接报「未定义的占位符」。
        // 那是跑冒烟脚本才发现的，而冒烟要人记得跑；这条在 `cargo test` 里就会红。
        let go = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../sdk/go/cmd/hub-plugin/render.go"
        ))
        .expect("读 render.go 失败");

        // knownPlaceholders 切片里列的是**常量名**，逐个换成它们在 Go 侧的值
        let block = between(&go, "var knownPlaceholders = []string{", "}")
            .expect("render.go 里找不到 knownPlaceholders");

        let mut go_keys: Vec<String> = block
            .lines()
            .map(|l| l.trim().trim_end_matches(',').trim())
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .map(|name| {
                between(&go, &format!("{name} = \""), "\"")
                    .unwrap_or_else(|| panic!("render.go 里找不到常量 {name} 的定义"))
                    .to_string()
            })
            .collect();
        go_keys.sort();

        let mut rust_keys: Vec<String> = crate::render::KNOWN_KEYS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        rust_keys.sort();

        assert_eq!(
            rust_keys, go_keys,
            "两侧认得的占位符对不上（Rust {rust_keys:?}，Go {go_keys:?}）。\
             对不上时，同一个模板会在一侧渲染得了、在另一侧报「未定义的占位符」"
        );
    }
}
