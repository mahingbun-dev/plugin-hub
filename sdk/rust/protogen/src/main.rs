//! 从 `crates/hub-proto/proto` 生成 Rust 契约代码，写进 `sdk/rust/src/proto/`。
//!
//! **产物提交进仓库，构建期不生成**——这是本 SDK 的一个明确取舍，理由见
//! `sdk/rust/README.md` 的「proto 产物为什么是提交的」。简言之：离线优先。
//!
//! 用法（改过 proto 之后）：
//!
//! ```console
//! cargo run --manifest-path sdk/rust/protogen/Cargo.toml
//! ```
//!
//! 它只读 `crates/hub-proto/proto`，**不在这里放第二份 .proto**：契约的唯一来源
//! 是中台仓库那一份，抄一份到这里，两份迟早会分叉。

use std::path::{Path, PathBuf};

/// 要生成的 proto。**只列插件侧用得到的五个**：
///   - envelope.proto —— Envelope，所有数据的统一形状
///   - plugin.proto   —— PluginRuntime 服务（插件要实现的服务端）
///   - registry.proto —— PluginRegistry 服务（插件要调的客户端）
///   - state.proto    —— HubState 服务（插件调的外置状态客户端）
///   - gateway.proto  —— PluginGateway 服务（插件调的发现与互调客户端）
///
/// bus.proto（总线消息）插件侧不用，不生成——生成了就是一堆没人用的死代码，
/// 还得为它压 `#[allow(dead_code)]`。
const PROTOS: &[&str] = &[
    "hub/v1/envelope.proto",
    "hub/v1/plugin.proto",
    "hub/v1/registry.proto",
    "hub/v1/state.proto",
    "hub/v1/gateway.proto",
];

/// 生成产物在 SDK 里的落点。
const OUTPUT: &str = "src/proto/hub.v1.rs";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // protogen/ → sdk/rust/
    let sdk_rust = here.parent().expect("protogen 应当有父目录").to_path_buf();
    // sdk/rust/ → sdk/ → 仓库根
    let repo_root = sdk_rust
        .parent()
        .and_then(Path::parent)
        .expect("sdk/rust 应当有祖父目录")
        .to_path_buf();

    let proto_root = repo_root.join("crates/hub-proto/proto");
    if !proto_root.is_dir() {
        return Err(format!(
            "找不到 proto 源目录 {}。本工具必须在中台仓库检出里运行——\
             sdk/rust 单独发给开发者时是不含 proto 源文件的（产物已经提交）。",
            proto_root.display()
        )
        .into());
    }

    // protoc 由 vendored 二进制提供，维护者不必自己装
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // 单线程，无并发读者；Rust 2024 把 set_var 标为 unsafe 正因并发写有风险
    unsafe { std::env::set_var("PROTOC", &protoc) };

    let mut includes = vec![proto_root.clone()];
    if let Ok(well_known) = protoc_bin_vendored::include_path() {
        // google/protobuf/*.proto（Any、Struct 等）随 protoc 一起分发
        includes.push(well_known);
    }

    // 先落到临时目录再拷过去：prost-build 只会往 OUT_DIR 里写，
    // 而我们要的是「产物进 src/、提交进仓库」，所以由我们决定最终落点。
    let staging = std::env::temp_dir().join(format!("hubkit-protogen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    unsafe { std::env::set_var("OUT_DIR", &staging) };

    let files: Vec<PathBuf> = PROTOS.iter().map(|p| proto_root.join(p)).collect();
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&files, &includes)?;

    let generated = staging.join("hub.v1.rs");
    let raw = std::fs::read_to_string(&generated)
        .map_err(|e| format!("{} 没生成出来: {e}", generated.display()))?;

    let dest = sdk_rust.join(OUTPUT);
    std::fs::write(&dest, with_header(&raw))?;
    let _ = std::fs::remove_dir_all(&staging);

    println!("已生成 {}", dest.display());
    Ok(())
}

/// 在产物前面加一段头注释。
///
/// 生成的代码有近三千行，打开它的人多半是「它是不是过时了」这个疑问来的——
/// 头注释是唯一能当场回答这件事的东西。
fn with_header(raw: &str) -> String {
    format!(
        "// 由 sdk/rust/protogen 从 crates/hub-proto/proto 生成，请勿手改。\n\
         //\n\
         // 重新生成：cargo run --manifest-path sdk/rust/protogen/Cargo.toml\n\
         //\n\
         // 产物提交进仓库是刻意的：插件团队不需要装 protoc、也不需要联网就能编译本 SDK。\n\
         // 代价是换 prost / tonic 大版本后必须重跑上面那条命令。\n\
         \n\
         {raw}"
    )
}
