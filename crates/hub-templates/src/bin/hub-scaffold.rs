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
//! ```
//!
//! `@@hub_addr@@` 缺省填本地 mock 中台的地址（与 `hub-plugin new` 同一个值）：
//! 在开发者机器上起工程时，要连的就是本地的 mock 中台。而中台渲染下载包时填的是
//! **那台中台自己**的地址——同一个占位符表达的始终是「这份工程该连哪台中台」，
//! 两处的答案本来就不同。

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use hub_templates::{build_zip, find, Values};

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
        format!("不认识的插件语言 {lang_id:?}；认得的有：{}", known.join(" / "))
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

const USAGE: &str = "\
hub-scaffold —— 在本地渲染一份插件脚手架工程（与中台下载接口同一份代码）

用法:
  hub-scaffold <语言> <插件名> --dir <目录> [选项]

选项:
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
fn extract(bytes: &[u8], dest: &PathBuf, plugin: &str) -> Result<usize, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("产出的包不是合法 zip：{e}"))?;
    let prefix = format!("{plugin}/");
    let mut written = 0usize;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("读包内第 {i} 项失败：{e}"))?;
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
