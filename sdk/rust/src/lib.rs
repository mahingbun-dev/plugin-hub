//! **plugin-hub 插件 SDK（Rust）**——插件侧的服务端骨架。
//!
//! 插件作者只需要实现 [`Plugin`]，然后调用 [`run`]，骨架会处理掉其余一切：
//! gRPC 服务、向中台自注册、心跳续期、被摘除后自动重新注册、优雅退出。
//!
//! ```no_run
//! use hubkit::{Config, Envelope, Plugin, PluginError, PluginManifest, ValidateResponse};
//!
//! struct MyPlugin;
//!
//! #[async_trait::async_trait]
//! impl Plugin for MyPlugin {
//!     fn manifest(&self) -> PluginManifest {
//!         PluginManifest {
//!             name: "order-reader".to_string(),
//!             version: "0.1.0".to_string(),
//!             ..Default::default()
//!         }
//!     }
//!
//!     async fn validate(&self, _env: &Envelope) -> Result<ValidateResponse, PluginError> {
//!         Ok(hubkit::envelope::valid())
//!     }
//!
//!     async fn handle(&self, env: Envelope) -> Result<Envelope, PluginError> {
//!         Ok(env)
//!     }
//! }
//!
//! # async fn f() -> Result<(), hubkit::HubkitError> {
//! hubkit::run(MyPlugin, Config::from_env()).await
//! # }
//! ```
//!
//! 插件被**强制无状态**：实例内存不保证跨调用保留（见中台的 `docs/design.md`），
//! 换来热切换零代价、水平扩展无障碍、重放与测试都简单。需要跨调用保留的东西走
//! [`state`]（HubState）——覆盖 [`Plugin::set_state`] 即可拿到客户端，
//! 骨架会在**每次注册成功后**注入，凭证不用也不该由插件自己管。
//!
//! # 模块速览
//!
//! | 模块 | 作用 |
//! |---|---|
//! | [`plugin`](Plugin) / [`run`] | 服务端骨架：gRPC、自注册、心跳、自愈、优雅退出 |
//! | [`state`] | **外置状态（HubState）**：跨调用要保留的东西放这里，别再存实例内存 |
//! | [`gateway`] | **插件发现与互调（PluginGateway）**：找得到谁、调得动谁，都经中台 |
//! | [`config`] | 运行参数，全部来自环境变量 |
//! | [`envelope`] | 载荷读写、deadline / budget、校验响应构造 |
//! | [`conformance`] | **L1** 契约一致性自测，接入前必须跑通 |
//! | [`rules`] | 跨语言规则判定（插件名、状态键），与中台共用一份契约文件 |
//! | [`proto`] | 契约代码。由 `protogen` 从 `crates/hub-proto/proto` 生成并提交 |
//! | [`log`] | JSON 日志，打到 stderr |

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod config;
pub mod conformance;
pub mod envelope;
pub mod error;
pub mod gateway;
pub mod log;
pub mod proto;
pub mod rules;
pub mod run;
pub mod state;

pub use config::{Config, ConfigError, DEFAULT_LISTEN_ADDR};
pub use error::{HubkitError, PluginError};
pub use gateway::{GatewayClient, GatewayError, InvokeOptions, CALL_CHAIN_META};
pub use log::{Level, Logger};
pub use proto::PluginManifest;
pub use run::{run, run_with_shutdown, Plugin};
pub use state::{
    StateClient, StateEntry, StateError, MAX_SCAN_LIMIT, MAX_VALUE_BYTES, STATE_TOKEN_METADATA,
};

/// 常用类型的平铺再导出。
///
/// 插件作者写实现时最常碰到的几个类型，从 crate 根部就能拿到：
/// `use hubkit::{Envelope, ValidateResponse, ValidationIssue, PluginError};`
pub use proto::{Envelope, HandleResponse, ValidateResponse, ValidationIssue};

/// 属性宏的再导出。
///
/// 插件作者写 `#[hubkit::async_trait]` 就行，**不必自己依赖 async-trait**：
/// 少一个要跟着 SDK 走的版本，也就少一种「宏的版本与 trait 的版本对不上」的可能。
/// tonic 的生成代码也是这么做的（经 `tonic::codegen` 再导出）。
pub use async_trait::async_trait;

pub use tonic;
