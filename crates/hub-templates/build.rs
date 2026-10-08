//! 扫描各语言的模板目录，生成编译期嵌入的文件清单。
//!
//! **为什么要 build.rs 而不是手写清单**：模板增删文件时，手写清单一定会漏——
//! 而漏掉的后果是「网页上下载到的包少了一个文件」，要等开发者解压后才发现。
//! 扫描出来就不可能漏。
//!
//! **为什么不用 include_dir / rust-embed**：本仓库是离线供应链，vendor 快照要
//! 一起送到 UAT，能不加的依赖就不加；而 `include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), ...))`
//! 已经能做到「编译期嵌入 + 路径随仓库走」，不需要任何 crate。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// 语言清单：(语言 id, 模板目录相对路径, 展示名, 一句话说明, 包名占位提示)。
///
/// 加一门语言 = 在这里加一行 + 把模板目录建出来。目录不存在会**直接构建失败**，
/// 而不是静默少一门语言——「声明了却没有」是配置错误，应当在构建时就炸。
const LANGUAGES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "go",
        "../../sdk/go/cmd/hub-plugin/templates",
        "Go",
        "内网默认的插件语言，SDK 功能最全（服务端骨架、mock 中台、契约自检、调试工具）",
        "example.com/order-reader",
    ),
    (
        "python",
        "../../sdk/python/templates/plugin",
        "Python",
        "用 grpcio 托 gRPC 服务；契约产物已提交进仓库，开发者不必装 protoc",
        "order_reader",
    ),
    (
        "node",
        "../../sdk/node/templates/plugin",
        "Node",
        "TypeScript 骨架；运行时直读 .proto（与契约原文逐字节可比），无构建步骤",
        "@example/order-reader",
    ),
    (
        "rust",
        "../../sdk/rust/templates/plugin",
        "Rust",
        "插件用 hubkit 起 gRPC 服务；契约产物提交进仓库，开发者不装 protoc",
        "order-reader",
    ),
    (
        "csharp",
        "../../sdk/csharp/templates/plugin",
        "C#",
        "用 .NET 托 gRPC 服务；SDK 随包下发（HubKit 库 + hubprobe + 自己的测试）",
        "OrderReader",
    ),
];

/// 模板目录里不作为产物输出的子目录。
///
/// `_shared/` 放的是各语言共用的素材（如接入指南共通层），它在渲染时被内联进
/// `AGENTS.md`，本身不该作为文件发给开发者。
const NON_PAYLOAD_DIRS: &[&str] = &["_shared"];

/// 随包下发的 SDK 源码：语言 id → (相对路径, 打进包里的目录名, **SDK 的引用路径**, 排除项)。
///
/// **为什么要把 SDK 源码随包发**：生成的工程要用 `replace` 指到 SDK，而后者的
/// 路径在开发者机器上不存在（内网没有私有 Go module 代理，模板里原本写的是
/// `/path/to/plugin-hub/sdk/go` 这个占位）——于是「下载一个工程」这件事根本
/// 落不了地：包能下、能解压，就是编不过。带上源码，`replace` 就能指向包内的
/// 相对路径，拿到手就能跑。
///
/// **第三个字段（SDK 的引用路径）必须在这里给**，不能写死在别处：`sdk/go` 的
/// module 路径与 `sdk/python` 的包名是不同的东西，中台侧一旦把它写死成某一个
/// 语言的值，加语言时就要去改那个写死的地方——而那正是「机制没验证过就并行
/// 铺开」会长的东西。
///
/// 排除项不是随意的，逐门说明：
///
/// - **Go**：`vendor/`（15MB，且它是 SDK 自己的 vendored 依赖；新工程是一个新
///   module，用不上别人的 vendor——实测会被 Go 判为 inconsistent vendoring 直接拒绝），
///   `cmd/hub-plugin/`（脚手架自己，里面 `//go:embed all:templates`，而 templates
///   不随源码发出，带进去会**编译失败**）。
/// - **Python**：`tests/` 与 `scaffold.py`（前者是 SDK 自测、后者是脚手架自己）、
///   `templates/`（中台渲染用），其余是构建产物——其中 `*.egg-info` **必须**排掉，
///   它里面的 `PKG-INFO` 记着打包者机器上的路径。
/// - **Rust**：`target/`（1.1GB）、`templates/`、`scripts/`（脚手架）、`protogen/`
///   （生成工具——产物已提交进仓库，开发者不需要重跑它）。
/// - **C#**：`templates/`，以及**逐个写死**的 `bin`/`obj`。这里不能图省事写
///   `bin`/`obj`——[`excluded`] 对具体路径是按前缀匹配的，而 C# 的产物在
///   `HubKit/bin` 这种一层之下，写 `bin` 一个都匹配不到。`HubKit.Tests/` 的源码
///   与 `HubProbe/` 是**有意**随包下发的（前者给开发者跑 L1，后者是 L2 的工具）。
const SDK_SOURCES: &[(&str, &str, &str, &str, &[&str])] = &[
    (
        "go",
        "../../sdk/go",
        "sdk",
        "github.com/mahingbun-dev/plugin-hub/sdk/go",
        &["vendor", "cmd/hub-plugin"],
    ),
    (
        "python",
        "../../sdk/python",
        "sdk",
        "hubkit",
        &[
            "tests",
            "templates",
            "scaffold.py",
            "__pycache__",
            ".pytest_cache",
            ".venv",
            "*.egg-info",
        ],
    ),
    (
        "node",
        "../../sdk/node",
        "sdk",
        "@example/hubkit",
        &["node_modules", ".git", "templates"],
    ),
    (
        "rust",
        "../../sdk/rust",
        "sdk",
        "hubkit",
        &["target", "templates", "protogen", "scripts"],
    ),
    (
        "csharp",
        "../../sdk/csharp",
        "sdk",
        "HubKit",
        &[
            "templates",
            "HubKit/bin",
            "HubKit/obj",
            "HubKit.Tests/bin",
            "HubKit.Tests/obj",
            "HubProbe/bin",
            "HubProbe/obj",
        ],
    ),
];

const TEMPLATE_SUFFIX: &str = ".tmpl";

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());

    let mut out = String::new();
    out.push_str("// 由 build.rs 生成，请勿手改。\n\n");

    for (id, rel, display, desc, package_hint) in LANGUAGES {
        let dir = manifest_dir.join(rel);
        if !dir.is_dir() {
            // 声明了却不存在，是配置错误而不是「这门语言还没做好」——后者应当
            // 直接不写进 LANGUAGES。静默跳过会让页面少一门语言而没有任何提示。
            panic!(
                "模板目录不存在：{}\n（语言 {id} 声明在 build.rs 的 LANGUAGES 里，但目录不在。\
                 要么把目录建出来，要么把这一行删掉。）",
                dir.display()
            );
        }

        let files = collect(&dir, &dir, NON_PAYLOAD_DIRS);
        if files.is_empty() {
            panic!("模板目录 {} 里一个文件都没有", dir.display());
        }

        // 改模板要能触发重建，否则改完跑出来的还是旧内容
        println!("cargo:rerun-if-changed={}", dir.display());
        for f in &files {
            println!("cargo:rerun-if-changed={}", dir.join(f).display());
        }

        let var = format!("FILES_{}", id.to_uppercase().replace('-', "_"));
        let _ = writeln!(out, "/// {display} 的模板文件：(输出路径, 模板字节)。");
        let _ = writeln!(out, "static {var}: &[(&str, &[u8])] = &[");
        for f in &files {
            // 输出路径去掉 .tmpl 后缀：.gitignore 这类名字必须还原，否则生成出来的
            // 工程里躺着一个 .gitignore.tmpl。
            let output = f.strip_suffix(TEMPLATE_SUFFIX).unwrap_or(f);
            // 用 CARGO_MANIFEST_DIR 拼绝对路径：本文件被 include! 进 src/lib.rs，
            // include_bytes! 的相对路径会按「被包含处」解析，行为难以预料；
            // concat! 出来的绝对路径没有歧义，也不受机器差异影响。
            let _ = writeln!(
                out,
                "    ({output:?}, include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {rel_part:?}))),",
                rel_part = format!("/{rel}/{f}"),
            );
        }
        let _ = writeln!(out, "];\n");

        // 随包下发的 SDK 源码。没有声明的语言就是空——那样生成的工程只能靠
        // 开发者自己准备 SDK，模板里要写清楚（见 SDK_SOURCES 的说明）。
        let (sdk_dir, sdk_var, sdk_module) = match SDK_SOURCES.iter().find(|(l, ..)| l == id) {
            Some((_, sdk_rel, sdk_in_zip, sdk_module, skip)) => {
                let sdk_root = manifest_dir.join(sdk_rel);
                if !sdk_root.is_dir() {
                    panic!(
                        "SDK 源码目录不存在：{}\n（语言 {id} 声明在 build.rs 的 SDK_SOURCES 里）",
                        sdk_root.display()
                    );
                }
                let sdk_files = collect(&sdk_root, &sdk_root, skip);
                if sdk_files.is_empty() {
                    panic!("SDK 源码目录 {} 里一个文件都没有", sdk_root.display());
                }

                println!("cargo:rerun-if-changed={}", sdk_root.display());
                for f in &sdk_files {
                    println!("cargo:rerun-if-changed={}", sdk_root.join(f).display());
                }

                let var = format!("SDK_{}_FILES", id.to_uppercase().replace('-', "_"));
                let _ = writeln!(
                    out,
                    "/// {id} 的 SDK 源码：随包下发，让下载的工程不必再去检出中台仓库。"
                );
                let _ = writeln!(out, "static {var}: &[(&str, &[u8])] = &[");
                for f in &sdk_files {
                    let _ = writeln!(
                        out,
                        "    ({f:?}, include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {rel_part:?}))),",
                        rel_part = format!("/{sdk_rel}/{f}"),
                    );
                }
                let _ = writeln!(out, "];\n");
                // 把排除清单也吐出来：Go 侧（sdk/go/cmd/hub-plugin/sdkfiles.go 的
                // sdkExcluded）有一份对应的，两处必须一致。一致性由一条测试比着
                // 源码来守——「两份手写清单迟早对不上」是本仓库反复踩过的坑。
                let _ = writeln!(
                    out,
                    "/// {id} 的 SDK 源码排除项（与 Go 侧的 sdkExcluded 必须一致）。\n\
                     /// 只有测试读它，所以允许 dead_code。\n\
                     #[allow(dead_code)]\n\
                     static {var}_EXCLUDED: &[&str] = &[{skipped}];\n",
                    skipped = skip
                        .iter()
                        .map(|s| format!("{s:?}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                );

                (*sdk_in_zip, var, *sdk_module)
            }
            None => ("", "&[]".to_string(), ""),
        };

        let _ = writeln!(
            out,
            "static LANG_{}: Language = Language {{ id: {id:?}, display_name: {display:?}, \
             description: {desc:?}, package_hint: {package_hint:?}, files: {var}, \
             source_dir: {rel:?}, \
             sdk_dir: {sdk_dir:?}, sdk_files: {sdk_var}, sdk_module: {sdk_module:?} }};\n",
            id.to_uppercase().replace('-', "_"),
        );
    }

    let _ = writeln!(out, "static LANGUAGES: &[&Language] = &[");
    for (id, ..) in LANGUAGES {
        let _ = writeln!(out, "    &LANG_{},", id.to_uppercase().replace('-', "_"));
    }
    let _ = writeln!(out, "];");

    // 构建时间戳。**只吐原始秒数，格式化交给库**——build script 里的 #[cfg(test)]
    // 不会被 `cargo test` 执行，把换算放这儿等于写了一个永远不会跑的测试。
    // CI 里可用 SOURCE_DATE_EPOCH 钉住，让同样的输入得到同样的二进制。
    let _ = writeln!(out, "\nstatic BUILT_AT_EPOCH: i64 = {};", build_epoch());

    // 共通层素材：各语言模板的 AGENTS.md 里用 @@onboarding@@ 引用它。
    // 缺了就是 None——但 None 会让渲染出一个**空章节**而不报错，所以库那边有一条
    // 测试兜着：只要有模板引用了 @@onboarding@@，这里就必须是 Some。
    let _ = writeln!(
        out,
        "\nstatic SHARED_ONBOARDING_SRC: Option<&str> = {};",
        shared_onboarding_expr(&manifest_dir),
    );

    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("templates_gen.rs");
    std::fs::write(&dest, out).expect("写入生成的模板清单失败");
}

/// 生成「共通层素材」那一行的表达式。
///
/// 共通层寄居在 Go 的模板目录下（`_shared/onboarding.md`）：Go 的 `//go:embed`
/// 不能向上取父目录，放到 `sdk/shared/` 就得为 Go 侧另加一套生成与校验脚本；
/// 而中台（Rust）读任何路径都不受限制。所以位置迁就了 Go 一侧。
fn shared_onboarding_expr(manifest_dir: &Path) -> String {
    for (_, rel, ..) in LANGUAGES {
        let p = manifest_dir.join(rel).join("_shared/onboarding.md");
        if p.is_file() {
            println!("cargo:rerun-if-changed={}", p.display());
            return format!(
                "Some(include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/{rel}/_shared/onboarding.md\")))"
            );
        }
    }
    "None".to_string()
}

/// 递归收集文件，返回相对于 `root` 的路径，已排序。
///
/// `skip` 的每一项要么是**相对 root 的具体路径**（可以带斜杠，如 `cmd/hub-plugin`），
/// 要么是**带 `*` 的 glob**（如 `*.egg-info`）。判据见 [`excluded`]。
///
/// 排序是必须的：`read_dir` 的顺序由文件系统决定，不排序会让同样的输入
/// 生成不同的清单，二进制也就不可复现了。
fn collect(root: &Path, dir: &Path, skip: &[&str]) -> Vec<String> {
    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!("读目录 {} 失败: {e}", dir.display());
    });

    for entry in entries {
        let entry = entry.expect("读目录项失败");
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .expect("文件不在根目录下")
            .to_string_lossy()
            .replace('\\', "/");

        if excluded(&rel, skip) {
            continue;
        }

        if path.is_dir() {
            files.extend(collect(root, &path, skip));
            continue;
        }

        files.push(rel);
    }

    files.sort();
    files
}

/// `rel` 是否命中排除项。三种形态，按排除项怎么写决定：
///
/// - **带 `/`**（`cmd/hub-plugin`）——按**路径前缀**，连同其下整棵一起跳过。不能
///   只看第一段：只看第一段时 `cmd/hub-plugin` 会因为第一段是 `cmd` 而漏掉，于是
///   脚手架自己被发出去，而它带 `//go:embed all:templates`，开发者拿到手编译直接失败。
/// - **带 `*`**（`*.egg-info`）——glob，比**任意一段**。
/// - **光名字**（`__pycache__`、`vendor`）——比**任意一层的同名目录**。
///
/// ⚠️ 后两种形态都**不是**前缀匹配，而这一点踩过两次坑，两次都是静默失效：
/// Python 的 `*.egg-info` 没命中 `hubkit.egg-info`、`__pycache__` 没命中
/// `hubkit/__pycache__`——它们本会原样进开发者的下载包，其中 `PKG-INFO` 还记着
/// 打包者机器上的路径。所以 `lib.rs` 里有一条测试直接断言**产出的文件清单**不含
/// 构建产物，而不是再信一遍这份手写清单：「排除项写了却没生效」比不写更糟，
/// 因为它看起来是被处理过的。
fn excluded(rel: &str, pats: &[&str]) -> bool {
    pats.iter().any(|p| {
        if p.contains('/') {
            rel == *p || rel.starts_with(&format!("{p}/"))
        } else if p.contains('*') {
            rel.split('/').any(|seg| glob_match(p, seg))
        } else {
            rel.split('/').any(|seg| seg == *p)
        }
    })
}

/// 单星号 glob：`*` 匹配任意字符（含空），其余按字面比。
///
/// 只支持一个 `*`：现有的排除项就 `*.egg-info` 这一种形态，多星号与 `?` / `[]`
/// 没有必要实现——为本仓库用不到的表达力引入一个通配引擎不划算。
fn glob_match(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        None => pat == s,
        Some((head, tail)) => {
            s.len() >= head.len() + tail.len() && s.starts_with(head) && s.ends_with(tail)
        }
    }
}

/// 构建时刻的 Unix 秒。
///
/// 优先取 `SOURCE_DATE_EPOCH`（可复现构建的通行做法），否则取当前时间。
/// 这个值会显示在控制台的模板卡片上，回答「这份二进制里的模板是什么时候的」。
fn build_epoch() -> i64 {
    match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(v) => v
            .trim()
            .parse::<i64>()
            .unwrap_or_else(|_| panic!("SOURCE_DATE_EPOCH 不是整数: {v:?}")),
        Err(_) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时间早于 1970")
            .as_secs() as i64,
    }
}
