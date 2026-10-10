//! 在本地渲染一份插件脚手架工程，不依赖中台。
//!
//! **为什么需要它**：Go / Python / Node / Rust 各自带一个本地脚手架（`hub-plugin new`
//! 等），开发者在自己机器上就能起工程；而 **C# 没有**——它的模板只能由中台的下载
//! 接口渲染。于是「验一下这份模板还能不能用」这件事在 CI 里做不到（CI 里没有中台），
//! 而模板腐烂恰恰是这套东西最大的风险。
//!
//! 它走的是和中台下载接口**完全同一条路**：[`hub_templates::build_zip`]。所以它不是
//! 「另一套渲染逻辑」，而是把那个接口搬到了命令行上——两边不会漂移，因为就是同一份。
//!
//! ```text
//! hub-scaffold <语言> <插件名> --dir <目录> [--package <包名>] [--hub-addr <地址>]
//! hub-scaffold upgrade <语言> --dir <目标项目根> [--name <插件名>] [--package <包名>]
//! ```
//!
//! `@@hub_addr@@` 缺省填本地 mock 中台的地址（与 `hub-plugin new` 同一个值）：
//! 在开发者机器上起工程时，要连的就是本地的 mock 中台。而中台渲染下载包时填的是
//! **那台中台自己**的地址——同一个占位符表达的始终是「这份工程该连哪台中台」，
//! 两处的答案本来就不同。
//!
//! `upgrade` 子命令给**存量**工程补 M6（互调/发现）素材：解升级包（同一份
//! [`hub_templates::build_upgrade_zip`]），按相对路径落盘，已存在的跳过、新文件写入，
//! 逐文件报告——幂等，重复执行全是 SKIP。

use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use hub_templates::{Values, build_upgrade_zip, build_zip, find};

/// 本地渲染时 `@@hub_addr@@` 的取值。与 `sdk/go/cmd/hub-plugin` 的 `localHubAddr` 一致。
const LOCAL_HUB_ADDR: &str = "http://127.0.0.1:8093";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("hub-scaffold: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }

    // upgrade 子命令的参数形状不同（没有插件名位置参数），先分流再解析
    if args.first().map(String::as_str) == Some("upgrade") {
        return run_upgrade(&args[1..]);
    }

    let mut positional = Vec::new();
    let mut dir: Option<PathBuf> = None;
    let mut package: Option<String> = None;
    let mut hub_addr: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => dir = Some(PathBuf::from(take_value(&args, &mut i, "--dir")?)),
            "--package" => package = Some(take_value(&args, &mut i, "--package")?),
            "--hub-addr" => hub_addr = Some(take_value(&args, &mut i, "--hub-addr")?),
            other if other.starts_with("--") => {
                return Err(format!("不认识的选项 {other}\n\n{USAGE}"));
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    let [lang_id, name] = positional.as_slice() else {
        return Err(format!("需要「语言」与「插件名」两个参数\n\n{USAGE}"));
    };

    let lang = find(lang_id).ok_or_else(|| {
        let known: Vec<&str> = hub_templates::languages().iter().map(|l| l.id).collect();
        format!(
            "不认识的插件语言 {lang_id:?}；认得的有：{}",
            known.join(" / ")
        )
    })?;

    let dir = dir.ok_or("缺少 --dir（生成到哪个目录）")?;
    if dir.exists() && std::fs::read_dir(&dir).map_err(io_err)?.next().is_some() {
        // 与 Go 侧脚手架同一条判断：**不覆盖**已有内容。静默往别人写了一半的目录里
        // 灌文件，比直接报错危险得多。
        return Err(format!("{} 已存在且不是空目录", dir.display()));
    }

    // 包名缺省就是插件名（与中台下载接口一致）；各语言要什么形式由模板的
    // package_hint 说明，这里不替调用方猜。
    let package = package.unwrap_or_else(|| name.clone());
    let hub_addr = hub_addr.unwrap_or_else(|| LOCAL_HUB_ADDR.to_string());

    let values = Values::for_plugin(name.clone(), package, lang.sdk_module, hub_addr);
    let bytes = build_zip(lang, &values).map_err(|e| format!("渲染失败：{e}"))?;

    std::fs::create_dir_all(&dir).map_err(io_err)?;
    let file_count = extract(&bytes, &dir, name)?;

    println!(
        "已生成{}插件工程 {}（{} 个模板文件 + 随包 SDK）",
        lang.display_name,
        dir.display(),
        file_count
    );
    Ok(())
}

/// `hub-scaffold upgrade <语言> --dir <目标项目根> [--name <插件名>] [--package <包名>]`。
///
/// 给**存量**工程补 M6 升级素材：解 [`build_upgrade_zip`] 的产物，把 `{name}/` 下的
/// 文件按相对路径落到 `--dir`。逐文件报告 ADD / SKIP——「这次到底动了什么」必须一眼
/// 可见，静默改文件正是升级脚本最忌讳的事。全部 SKIP 时退出码仍是 0：幂等升级的
/// 「没做任何事」不是失败。
fn run_upgrade(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut dir: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut package: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => dir = Some(PathBuf::from(take_value(args, &mut i, "--dir")?)),
            "--name" => name = Some(take_value(args, &mut i, "--name")?),
            "--package" => package = Some(take_value(args, &mut i, "--package")?),
            other if other.starts_with("--") => {
                return Err(format!("不认识的选项 {other}\n\n{USAGE}"));
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    let [lang_id] = positional.as_slice() else {
        return Err(format!("upgrade 需要「语言」一个位置参数\n\n{USAGE}"));
    };

    let lang = find(lang_id).ok_or_else(|| {
        let known: Vec<&str> = hub_templates::languages().iter().map(|l| l.id).collect();
        format!(
            "不认识的插件语言 {lang_id:?}；认得的有：{}",
            known.join(" / ")
        )
    })?;

    let dir = dir.ok_or("缺少 --dir（要升级哪个工程）")?;
    if !dir.is_dir() {
        // upgrade 面向存量工程：目录不在多半是路径拼错了。**不替调用方建目录**——
        // 建出来的是一个空目录，那不是升级，还会把「路径写错」这条线索抹掉。
        return Err(format!(
            "{} 不存在（upgrade 只对已有工程执行，不新建）",
            dir.display()
        ));
    }

    // 插件名只影响渲染取值（UPGRADE.md 里 @@name@@ 指代项目、包内顶层目录名），
    // 不影响落盘路径。缺省取 --dir 的目录名——工程目录名通常就是项目名。
    let name = name.unwrap_or_else(|| {
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".to_string())
    });
    if name.is_empty() || name.contains('/') {
        // 名字会进 zip 的路径前缀，带斜杠会让解包时的前缀匹配全部落空，
        // 报出来的错（「包内路径不在 … 之下」）完全不指向真正的原因
        return Err(format!("插件名 {name:?} 非法：不能为空、不能含 /"));
    }

    // @@package@@ 的取值：显式给的直接用；没给时 csharp 门从存量工程探测
    // （见 resolve_upgrade_package）。注意**不能**照抄 new 子命令的「缺省取
    // 插件名」——upgrade 面向的是已有工程，csharp 的素材里 @@package@@ 是
    // namespace，插件名（常带 -）代进去产出 `namespace order-reader;` 这种
    // 非法代码，要到开发者 dotnet build 时才以一屏 CS0116 暴露，没有一句
    // 指向「包名渲染错了」。
    let package = match package {
        Some(p) => p,
        None => resolve_upgrade_package(lang, &dir, &name),
    };

    let values = Values::for_plugin(
        name.clone(),
        package,
        lang.sdk_module,
        LOCAL_HUB_ADDR.to_string(),
    );
    let bytes = build_upgrade_zip(lang, &values).map_err(|e| format!("打不出升级包：{e}"))?;

    let written = extract_upgrade(&bytes, &dir, &name)?;

    if written == 0 {
        println!("已是最新，无需变更");
    }
    // 检查清单摘要只指向 UPGRADE.md：完整步骤（README 追加、门专属自检）在
    // upgrade.sh 与 UPGRADE.md 里，CLI 不复述第二遍——两份清单迟早对不上。
    println!(
        "后续步骤见 {}（先读它，再 bash upgrade.sh 完成升级）",
        dir.join("UPGRADE.md").display()
    );
    Ok(())
}

/// 决定 upgrade 渲染用的 `@@package@@` 取值（没给 `--package` 时的缺省）。
///
/// **csharp 门从存量工程探测**：它的 @@package@@ 是 C# 命名空间，而插件名常带
/// `-`（如 order-reader），直接代进去会渲染出 `namespace order-reader;` 这种非法
/// 代码——dotnet build 报一屏 CS0116，没有一句指向「包名错了」。存量工程的
/// csproj 里就写着正确答案（RootNamespace / AssemblyName），没有理由放着它去猜。
///
/// 其余语言的升级素材不用 @@package@@，缺省取插件名——与各语言本地脚手架的
/// 缺省一致，不在这里另立一套。csharp 探测不到时回退插件名**并打印提示**：
/// 回退在插件名带 - / 数字开头时必然产出编不过的代码，必须让操作者当场看见。
fn resolve_upgrade_package(
    lang: &hub_templates::Language,
    dir: &std::path::Path,
    name: &str,
) -> String {
    if lang.id != "csharp" {
        return name.to_string();
    }
    match read_csproj_namespace(dir) {
        Some(ns) => {
            println!("探测到工程命名空间（来自 csproj 的 RootNamespace/AssemblyName）：{ns}");
            ns
        }
        None => {
            println!(
                "⚠️ 没在 {} 里读到 RootNamespace/AssemblyName，回退用插件名 {name:?} 渲染命名空间；\
                 插件名带 - 或数字开头时产物编不过（CS0116 一屏错误），\
                 此时请显式给 --package <命名空间（如 OrderReader）>",
                dir.display()
            );
            name.to_string()
        }
    }
}

/// 读 csharp 工程根下第一个 *.csproj 的命名空间：RootNamespace 优先，次选
/// AssemblyName。
///
/// 手写文本解析（找 `<标签>值</标签>`）而不引 XML crate：这里只需要两个
/// PropertyGroup 属性，为零依赖铁律引入一个解析器不划算。取根目录下**字典序
/// 最小**的一个——脚手架产物根下只有 Plugin.csproj 一个；tests/ 与 sdk/ 里
/// SDK 自己的工程都在子目录，不会被读目录的这一层扫到。
fn read_csproj_namespace(dir: &std::path::Path) -> Option<String> {
    let csproj = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "csproj"))
        .min()?;
    let text = std::fs::read_to_string(csproj).ok()?;

    // 两个标签分开找：RootNamespace 缺席时还要继续试 AssemblyName，
    // 不能用 `?` 短路出整个函数
    for tag in ["RootNamespace", "AssemblyName"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let Some((_, rest)) = text.split_once(&open) else {
            continue;
        };
        let Some((value, _)) = rest.split_once(&close) else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

const USAGE: &str = "\
hub-scaffold —— 在本地渲染一份插件脚手架工程（与中台下载接口同一份代码）

用法:
  hub-scaffold <语言> <插件名> --dir <目录> [选项]
      生成一份新的插件工程
  hub-scaffold upgrade <语言> --dir <目标项目根> [选项]
      给存量工程补 M6（互调/发现）升级素材：已存在的文件跳过、新文件写入，
      逐文件报告 ADD / SKIP；全部跳过时打印「已是最新」并退出 0

选项（upgrade）:
  --dir <目录>     要升级的工程根（必填；必须已存在——upgrade 不新建工程）
  --name <插件名>  填进 @@name@@ 的项目名（进 UPGRADE.md 与包内目录名），
                   缺省取 --dir 的目录名
  --package <包名> 填进 @@package@@ 的包名 / C# 命名空间。缺省：csharp 从
                   存量工程的 *.csproj 探测（RootNamespace，次选 AssemblyName），
                   其余语言取 --name（与各语言本地脚手架的缺省一致）；
                   csharp 探测不到时回退 --name 并打印提示（插件名带 - 或
                   数字开头时产物编不过，此时应显式传）

选项（生成新工程）:
  --dir <目录>        生成到哪（必填；目录已存在且非空时报错，不覆盖）
  --package <包名>    包名 / module 路径 / C# 命名空间，缺省与插件名相同
  --hub-addr <地址>   填进 @@hub_addr@@ 的中台插件面地址
                      缺省 http://127.0.0.1:8093（本地 mock 中台）

语言: go / python / node / rust / csharp（以 --help 所在版本实际支持为准）
";

/// 取 `--flag` 后面那个值。
fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} 后面缺一个值"))
}

/// 把包解开到 `dest`。
///
/// 包内顶层是**一层以插件名命名的目录**（下载下来解压即得一个工程目录），
/// 而命令行上的 `--dir` 语义是「工程就落在这个目录」，所以这一层要去掉——
/// 否则会得到一个 `dir/order-reader/plugin.go`，与各语言本地脚手架的行为不一致。
fn extract(bytes: &[u8], dest: &Path, plugin: &str) -> Result<usize, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("产出的包不是合法 zip：{e}"))?;
    let prefix = format!("{plugin}/");
    let mut written = 0usize;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读包内第 {i} 项失败：{e}"))?;
        let name = entry.name().to_string();
        let Some(rel) = name.strip_prefix(&prefix) else {
            return Err(format!("包内路径 {name} 不在 {prefix} 之下"));
        };
        if rel.is_empty() {
            continue;
        }

        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(io_err)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }

        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("读 {name} 失败：{e}"))?;
        std::fs::write(&out, &buf).map_err(io_err)?;

        // 包里的可执行位（脚手架脚本那类）要跟着走，否则解出来的 .sh 跑不了
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode));
        }
        written += 1;
    }

    Ok(written)
}

fn io_err(e: std::io::Error) -> String {
    e.to_string()
}

/// 把升级包解开到 `dest`，已存在的文件跳过、新文件写入，返回写入数。
///
/// 与 [`extract`] 的差别就是 ADD / SKIP：升级面向**别人写了一半的存量工程**，
/// 覆盖已有文件（哪怕内容相同）会刷新 mtime、触发无谓的重构建，更糟的是让
/// 「这次升级动了什么」变得不可信——所以逐文件报告，一个不漏。
fn extract_upgrade(bytes: &[u8], dest: &Path, plugin: &str) -> Result<usize, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("产出的包不是合法 zip：{e}"))?;
    let prefix = format!("{plugin}/");
    let mut written = 0usize;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读包内第 {i} 项失败：{e}"))?;
        let name = entry.name().to_string();
        let Some(rel) = name.strip_prefix(&prefix) else {
            return Err(format!("包内路径 {name} 不在 {prefix} 之下"));
        };
        if rel.is_empty() || entry.is_dir() {
            continue;
        }

        let out = dest.join(rel);
        if out.exists() {
            println!("SKIP {rel}");
            continue;
        }

        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("读 {name} 失败：{e}"))?;
        std::fs::write(&out, &buf).map_err(io_err)?;

        // 可执行位跟着走（upgrade.sh 靠它落盘后就能 bash），理由与 extract 相同
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode));
        }

        println!("ADD  {rel}");
        written += 1;
    }

    Ok(written)
}
