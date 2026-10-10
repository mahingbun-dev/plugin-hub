//! 插件脚手架模板：列语言、按语言下载一份可运行的插件工程。
//!
//! 控制台的「插件开发接入」页用这两个接口。**按设计不鉴权**——它们是「怎么开发一个
//! 插件」的开发资料，不是运行中的数据；管理面鉴权管的是后者。见
//! [`crate::authz::required_scope`] 里那一条的说明（要收紧的话在那里改一行）。
//!
//! 模板内容全部由 [`hub_templates`] 在编译期嵌入中台二进制，这里只负责把它渲染成
//! 某个具体插件的工程并打成一个 zip。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use hub_registry::validate::is_valid_plugin_name;
use hub_templates::{Language, Values};
use serde::{Deserialize, Serialize};

use crate::ApiState;
use crate::error::ApiError;

/// 中台没配对外地址时，写进工程里的占位文字。
///
/// **刻意写得像一句话而不是留空**：留空会生成一个 `HUB_ADDR=` 的工程，那种错误要等
/// 插件连不上中台时才暴露，而报错只有一句 connection refused，看不出是这里少了东西。
///
/// 五门语言的 `AGENTS.md` 都引用 `@@hub_addr@@`（`README.md` 里四门有、Go 那份没有
/// ——它刻意同时讲本地与 UAT 两个地址，机械替换会丢掉它讲清楚的东西），所以这段文字
/// 会真的出现在开发者拿到的工程里。它必须能当**取值**读：那些地方写的是
/// 「本次随包下发的就是 `@@hub_addr@@`」，把占位文字代进去仍然读得通。
const HUB_ADDR_PLACEHOLDER: &str = "<中台插件面地址——向中台维护方索取，或见部署文档>";

/// 列表项：控制台上每种语言一张卡片。
#[derive(Debug, Serialize)]
pub struct TemplateInfo {
    /// 语言标识，下载时用在 URL 里。
    pub lang: &'static str,

    /// 卡片标题。
    pub display_name: &'static str,

    /// 一句话说明这份模板给到什么程度。
    pub description: &'static str,

    /// 包名 / module 路径输入框的提示文字。
    pub package_hint: &'static str,

    /// 这份模板一共几个文件。
    pub file_count: usize,

    /// 下载下来大约多少字节。
    ///
    /// **是近似值，不是承诺**：包大小随插件名长度浮动（名字出现在包内路径与文件正文
    /// 里二十来处），而列表是在用户填名字**之前**取的。这里给的是「拿一个常见长度的
    /// 名字量出来的参考值」，页面上应当渲染成「约 32 KB」而不是精确数字。
    ///
    /// 之所以还是按真实打一遍来量、而不是把各文件长度加起来：后者算的是**内容**之和，
    /// 与下载到的 zip 差着每个条目的头部与中央目录，会把一个能用的估算变成一个错的数字。
    pub size_bytes: usize,

    /// M6 升级包下载下来大约多少字节；**0 = 这门语言的升级素材还没就绪**。
    ///
    /// 近似值的口径与 [`TemplateInfo::size_bytes`] 相同。素材在编译期嵌入
    /// （`templates/upgrade/`），五门并行产出期允许部分语言缺席——前端拿 0
    /// 判断「隐藏/禁用下载升级包按钮」，比一个 500 或空 zip 体面。
    pub upgrade_size_bytes: u64,

    /// 中台自身的版本。模板随中台发版，这个值回答「这份模板适配哪版中台」。
    pub hub_version: &'static str,

    /// 这份二进制里的模板是什么时候构建的（RFC3339 UTC）。
    pub built_at: String,

    /// 中台是否配了插件面对外地址（`HUB_PLUGIN_PUBLIC_ADDR`）。
    ///
    /// 五门模板都引用 `@@hub_addr@@`，所以为 false 时工程里出现的是
    /// [`HUB_ADDR_PLACEHOLDER`] 那段文字，页面据此提示开发者「本次下载的工程里中台
    /// 地址是占位文字，需自行替换」；为 true 时工程里就是本中台的地址，不必提示。
    ///
    /// 不猜一个地址是刻意的：地址填错是插件接不进来的头号原因，而报错只有一句
    /// connection refused。
    pub plugin_addr_configured: bool,
}

/// 全部语言的模板。
pub async fn list(State(state): State<ApiState>) -> Json<Vec<TemplateInfo>> {
    let addr_configured = state.plugin_public_addr.is_some();

    let items = hub_templates::languages()
        .iter()
        .map(|lang| TemplateInfo {
            lang: lang.id,
            display_name: lang.display_name,
            description: lang.description,
            package_hint: lang.package_hint,
            file_count: lang.file_count(),
            // 用一个常见长度的样例名按真实流程打一遍再量。这个数字是近似的——
            // 包大小随插件名长度浮动，而列表是在用户填名字之前取的，所以页面上
            // 应当渲染成「约 32 KB」。详见 TemplateInfo::size_bytes 的说明。
            size_bytes: hub_templates::archive_size(lang, &sample_values(lang)).unwrap_or_default(),
            // 素材未就绪的语言给 0（页面上据此藏掉「下载升级包」），而不是让
            // 列表接口整体报错——五门并行产出期里，缺一门不该连另外几门也下载不了
            upgrade_size_bytes: match hub_templates::upgrade_files(lang.id) {
                Ok(_) => hub_templates::upgrade_archive_size(lang, &sample_values(lang))
                    .unwrap_or_default() as u64,
                Err(_) => 0,
            },
            hub_version: env!("CARGO_PKG_VERSION"),
            built_at: hub_templates::built_at(),
            plugin_addr_configured: addr_configured,
        })
        .collect();

    Json(items)
}

/// 下载参数。
#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    /// 插件名。同时决定包内目录名、manifest 里的 `name` 与 MCP 工具前缀。
    pub name: String,

    /// 包名 / module 路径。留空时用插件名（与 `hub-plugin new` 的缺省一致）。
    ///
    /// **csharp 下载升级包时建议显式传**：csharp 素材里的 `@@package@@` 是 C#
    /// 命名空间，而插件名常带 `-`（如 `order-reader`）——缺省代进去会渲染出
    /// `namespace order-reader;` 这种非法代码，要到开发者 `dotnet build` 时才
    /// 以一屏 CS0116 暴露，报错里没有一句指向「包名没传」。插件名本身是合法
    /// 命名空间（如 `order_reader`）时缺省无碍。
    #[serde(default)]
    pub package: Option<String>,
}

/// 按语言下载一份插件工程。
pub async fn download(
    State(state): State<ApiState>,
    Path(lang_id): Path<String>,
    Query(q): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    let lang = hub_templates::find(&lang_id).ok_or_else(|| {
        // 把有哪些语言列出来：写错一个字母时，这条消息就是唯一能自助的地方
        let known: Vec<&str> = hub_templates::languages().iter().map(|l| l.id).collect();
        ApiError::not_found(format!(
            "没有 {lang_id} 这门语言的模板（现有：{}）",
            known.join(" / ")
        ))
    })?;

    let name = q.name.trim();
    // 用**中台注册期那条规则**校验，而不是另写一套：这里的判据与
    // hub-registry 注册时用的必须是同一个，否则会出现「页面放行、注册被拒」
    // 或者反过来的割裂。注意它比 hub-plugin new 宽松（允许大写与下划线）。
    if !is_valid_plugin_name(name) {
        return Err(ApiError::bad_request(
            "插件名非法：只允许字母数字与 -_，最长 64 字符，且以字母数字开头",
        ));
    }

    // 缺省用插件名当包名，与 `hub-plugin new` 的缺省一致——两边生成的工程
    // 因此长得一样，开发者换用另一条路径时不会感到意外。
    let package = q
        .package
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(name);

    let values = values_for(lang, name, package, state.plugin_public_addr.as_deref());

    let bytes = hub_templates::build_zip(lang, &values).map_err(|e| {
        // 渲染失败的成因在中台这一侧（模板里有未定义的占位符），不是调用方的问题
        ApiError::internal(format!("打包 {} 模板失败：{e}", lang.display_name))
    })?;

    let filename = hub_templates::download_filename(lang, name);
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// 按语言与用户输入构造渲染取值。
///
/// `sdk_module` 取自语言本身（由 `hub-templates` 的 `build.rs` 给出），**不在这里
/// 写死**：Go 的 module 路径与 Python 的包名是不同的东西，写死成某一门语言的值，
/// 加语言时就要回来改这里。
///
/// `package` 的缺省（插件名）对 go / python / node / rust 的升级素材无碍——它们的
/// 素材不用 `@@package@@`；**csharp 例外**，见 [`DownloadQuery::package`] 的说明：
/// 素材里的 `@@package@@` 是 C# 命名空间，插件名带 `-` 时缺省渲染出非法代码，
/// 前端在 csharp 门应引导用户显式填命名空间（如 `OrderReader`）。
fn values_for(lang: &Language, name: &str, package: &str, hub_addr: Option<&str>) -> Values {
    let addr = hub_addr
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .unwrap_or(HUB_ADDR_PLACEHOLDER);

    Values::for_plugin(name, package, lang.sdk_module, addr)
}

/// 按语言下载 M6 升级补丁包（发给**存量**插件工程）。
///
/// 与 [`download`] 同一套判据（语言未知 404、插件名非法 400）与同一套 values 装配
/// ——两条通道给同一个开发者，判据分叉就会出现「页面能下、CLI 报错」的割裂。
///
/// `?package=` 对 **csharp 建议总是显式传**（合法 C# 命名空间，如 `OrderReader`）：
/// 缺省用插件名渲染 csharp 素材的 `@@package@@`（命名空间），插件名带 `-` 时产物
/// 编不过，详见 [`DownloadQuery::package`]。CLI 那条通道（`hub-scaffold upgrade`）
/// 会从存量工程的 csproj 自动探测，HTTP 这条探测不到——对面只是一个下载页。
pub async fn upgrade_package(
    State(state): State<ApiState>,
    Path(lang_id): Path<String>,
    Query(q): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    let name = q.name.trim();
    let package = q
        .package
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(name);

    let (bytes, filename) =
        upgrade_package_bytes(&lang_id, name, package, state.plugin_public_addr.as_deref())?;

    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// [`upgrade_package`] 的核心：校验、渲染、打包、给下载文件名。
///
/// 拆出来是因为 handler 要挂 State（构造一个真的 ApiState 得连数据库），而这条
/// 逻辑值得直接测三态：成功 / 语言未知 404 / 名字非法 400——素材未就绪的 404
/// 也在这一层分流。
fn upgrade_package_bytes(
    lang_id: &str,
    name: &str,
    package: &str,
    hub_addr: Option<&str>,
) -> Result<(Vec<u8>, String), ApiError> {
    let lang = hub_templates::find(lang_id).ok_or_else(|| {
        // 把有哪些语言列出来：写错一个字母时，这条消息就是唯一能自助的地方
        let known: Vec<&str> = hub_templates::languages().iter().map(|l| l.id).collect();
        ApiError::not_found(format!(
            "没有 {lang_id} 这门语言的模板（现有：{}）",
            known.join(" / ")
        ))
    })?;

    // 与 download 同一条规则（中台注册期那条），理由见 download 里的注释
    if !is_valid_plugin_name(name) {
        return Err(ApiError::bad_request(
            "插件名非法：只允许字母数字与 -_，最长 64 字符，且以字母数字开头",
        ));
    }

    let values = values_for(lang, name, package, hub_addr);

    let bytes = hub_templates::build_upgrade_zip(lang, &values).map_err(|e| match e {
        // 素材未就绪是「这门还没有升级包」而不是中台故障：404 让前端提示
        // 「请升级中台后再试」，错误信息点名缺的是哪几门
        hub_templates::PackError::Upgrade(why) => ApiError::not_found(why.to_string()),
        // 渲染失败的成因在中台这一侧（模板里有未定义的占位符），不是调用方的问题
        other => ApiError::internal(format!("打包 {} 升级包失败：{other}", lang.display_name)),
    })?;

    let filename = hub_templates::upgrade_download_filename(lang, name);
    Ok((bytes, filename))
}

/// 列表里量包大小用的样例取值。
fn sample_values(lang: &Language) -> Values {
    values_for(
        lang,
        "sample-plugin",
        lang.package_hint,
        Some("https://example.invalid:8094"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试里要的那门语言。取 `go` 是因为它是当前唯一有 CLI 可比对的一门。
    fn go() -> &'static Language {
        hub_templates::find("go").expect("go 语言必须存在")
    }

    #[test]
    fn 缺省包名用插件名与_cli_一致() {
        // hub-plugin new 的缺省也是 --module 留空时取插件名
        let values = values_for(go(), "order-reader", "order-reader", None);
        assert_eq!(values.package, "order-reader");
    }

    #[test]
    fn 中台没配地址时写进工程的是可见占位文字而不是空串() {
        // 留空会生成 HUB_ADDR= 的工程，那种错误要到插件连不上中台时才暴露
        let values = values_for(go(), "p", "p", None);
        assert_eq!(values.hub_addr, HUB_ADDR_PLACEHOLDER);
        assert!(!values.hub_addr.is_empty());

        // 配了空格串也当作没配——复制粘贴带上的空白最容易骗过 is_some()
        let values = values_for(go(), "p", "p", Some("   "));
        assert_eq!(values.hub_addr, HUB_ADDR_PLACEHOLDER);
    }

    #[test]
    fn 中台配了地址时原样写进工程() {
        let values = values_for(go(), "p", "p", Some("https://hub.example.com:8094"));
        assert_eq!(values.hub_addr, "https://hub.example.com:8094");
    }

    #[test]
    fn 插件名校验用的是中台注册期那条规则() {
        // 这条测试的意义是「锁定判据来源」：页面放行的名字必须与注册时接受的
        // 一致。中台那条比 hub-plugin new 宽松（允许大写与下划线），这里跟着宽松。
        for ok in ["order-reader", "wms_reader", "A1"] {
            assert!(is_valid_plugin_name(ok), "{ok} 应合法");
        }
        for bad in ["", "-lead", "has space", "中文名"] {
            assert!(!is_valid_plugin_name(bad), "{bad} 应非法");
        }
    }

    #[test]
    fn go_的_sdk_module与_cli_里的一致() {
        // 中台打的包与 CLI 生成的工程必须能互换：import 路径不一致就编译不过。
        // 这里读 CLI 的源码比对，而不是再抄一份常量——抄一份就是等着它漂移。
        let cli = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../sdk/go/cmd/hub-plugin/main.go"
        ))
        .expect("读 sdk/go/cmd/hub-plugin/main.go 失败");

        let want = format!("SDKModule = \"{}\"", go().sdk_module);
        assert!(
            cli.contains(&want),
            "中台记的 sdk_module 与 CLI 的 SDKModule 对不上。CLI 里应有：{want}"
        );
    }

    // ---------------------------------------------------------------- 升级包

    #[test]
    fn 升级包_成功_产出zip与规定文件名() {
        let (bytes, filename) =
            upgrade_package_bytes("go", "order-reader", "order-reader", None).unwrap();

        assert_eq!(filename, "plugin-hub-go-upgrade-order-reader.zip");
        // hub-api 不依赖 zip crate（解包校验归 hub-templates 的测试），这里验
        // 「确实是个 zip 且不是空的」：本地文件头魔数 PK\x03\x04 是 zip 的硬标志
        assert!(
            bytes.starts_with(b"PK\x03\x04"),
            "升级包应以 zip 本地文件头开头"
        );
        assert!(!bytes.is_empty(), "升级包不可能是空的");

        // UPGRADE.md 用 @@name@@ 指代项目：渲染取值必须真的进去了
        // （zip 是 Stored 存储，正文按明文出现在包里，可以直接搜）
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("UPGRADE.md"),
            "包里应有三件套之一 UPGRADE.md 的正文"
        );
        assert!(
            !text.contains("@@name@@"),
            "包里残留了未渲染的 @@name@@——存量工程拿到的说明会指代不明"
        );
    }

    #[test]
    fn 升级包_未知语言_404并列出认得的() {
        let err = upgrade_package_bytes("不存在", "order-reader", "order-reader", None)
            .expect_err("未知语言应报错");
        match &err {
            ApiError::NotFound(message) => {
                assert!(message.contains("不存在"), "应点名是哪门语言：{message}");
                assert!(message.contains("go"), "应列出认得的语言：{message}");
            }
            other => panic!("应是 404，实际 {other:?}"),
        }
    }

    #[test]
    fn 升级包_名字非法_400() {
        // 判据与 download 共用中台注册期那条规则，这条锁住「升级包没有另搞一套」
        for bad in ["", "-lead", "has space"] {
            let err = upgrade_package_bytes("go", bad, bad, None).expect_err("非法名应报错");
            assert!(
                matches!(err, ApiError::BadRequest(_)),
                "{bad:?} 应是 400，实际 {err:?}"
            );
        }
    }

    #[test]
    fn 升级包_素材未就绪_404而不是500() {
        // **素材存在才断言**：五门素材并行产出，这条在还有门缺席时验「缺席的门
        // 报 404」，五门齐之后整条空过——不锁任何一门「现在必须是缺席的」。
        let missing = hub_templates::languages()
            .iter()
            .find(|l| hub_templates::upgrade_files(l.id).is_err());

        if let Some(lang) = missing {
            let err = upgrade_package_bytes(lang.id, "order-reader", "order-reader", None)
                .expect_err("素材未就绪应报错");
            match &err {
                ApiError::NotFound(message) => {
                    // 错误要点名缺的是哪门——这是排障时唯一能自助的地方
                    assert!(
                        message.contains(lang.id),
                        "应点名 {lang}：{message}",
                        lang = lang.id
                    );
                }
                other => panic!("素材未就绪应是 404，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn 列表里的升级包大小_有素材的门量出来大于零() {
        // list() 对有素材的门按 sample_values 量真实包大小、对没素材的门给 0。
        // Err→0 那一行是三行装配，不值得为它起一个 ApiState（要连数据库）；
        // 这里锁的是另一半：有素材时量出来的数字必须是个真包的大小（>0），
        // 而不是 unwrap_or_default 把打包失败吞成了 0——那会让前端把「能下载」
        // 的门也藏掉。
        for lang in hub_templates::languages() {
            if hub_templates::upgrade_files(lang.id).is_err() {
                continue;
            }
            let size = hub_templates::upgrade_archive_size(lang, &sample_values(lang))
                .unwrap_or_else(|e| panic!("{} 的升级包打不出来（列表会显示成 0）：{e}", lang.id));
            assert!(size > 0, "{} 的升级包不该是空的", lang.id);
        }
    }

    #[test]
    fn 列表里的包大小是近似值但不离谱() {
        // 这个数字随插件名长度浮动（名字出现在包内路径与正文里二十来处，约 58 字节/字符），
        // 而列表是在用户填名字**之前**取的，所以它不可能精确。但也不能差出一个量级——
        // 那样卡片上那句「约 32 KB」就是在误导人估量下载成本。
        //
        // 曾经这里断言的是「显示值 == 用样名下载的值」，那是个同义反复：
        // 拿同一个样名跟自己比，永远为真，什么也证明不了。
        //
        // 容差取 2KB（约 6%）而不是更紧的界：偏差本来就随名字长度线性增长，
        // 界压到几十字节只能靠再写一遍同样的公式，那测的就是公式本身而不是结论了。
        const MAX_DRIFT: usize = 2048;

        for lang in hub_templates::languages() {
            let listed = hub_templates::archive_size(lang, &sample_values(lang)).unwrap();

            for name in ["a", "order-reader", "a-rather-long-plugin-name"] {
                let actual = hub_templates::build_zip(lang, &values_for(lang, name, name, None))
                    .unwrap()
                    .len();
                let drift = listed.abs_diff(actual);
                assert!(
                    drift < MAX_DRIFT,
                    "{} 卡片上写 {listed} 字节，但用名字 {name:?} 实际下到 {actual} 字节，\
                     差了 {drift}——超出了近似值可接受的范围",
                    lang.id
                );
            }
        }
    }
}
