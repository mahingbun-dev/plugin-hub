//! 契约代码生成。
//!
//! protoc 由 `protoc-bin-vendored` 提供：UAT 服务器无外网、离线构建镜像里也不装 protoc，
//! 把二进制随依赖一起 vendor 进来，才能保证 `cargo build --offline` 可用。

use std::path::{Path, PathBuf};

const PROTOS: &[&str] = &[
    "hub/v1/envelope.proto",
    "hub/v1/plugin.proto",
    "hub/v1/registry.proto",
    "hub/v1/state.proto",
    "hub/v1/bus.proto",
    "hub/v1/gateway.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // build.rs 单线程执行，这是把 protoc 路径传给代码生成器的唯一途径。
    // Rust 2024 把 set_var 标为 unsafe（可能与并发读者竞争），此处不存在并发读者。
    unsafe { std::env::set_var("PROTOC", &protoc) };

    let proto_root = PathBuf::from("proto");
    let mut includes: Vec<PathBuf> = vec![proto_root.clone()];
    // google/protobuf/*.proto（Any 等 well-known types）随 protoc 一起分发
    if let Ok(well_known) = protoc_bin_vendored::include_path() {
        includes.push(well_known);
    }

    let files: Vec<PathBuf> = PROTOS.iter().map(|p| proto_root.join(p)).collect();

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&files, &includes)?;

    for proto in PROTOS {
        println!(
            "cargo:rerun-if-changed={}",
            Path::new("proto").join(proto).display()
        );
    }
    Ok(())
}
