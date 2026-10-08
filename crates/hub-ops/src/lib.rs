//! 主机面运维通道。
//!
//! 管理面按设计是全插件化的（中台内不含鉴权逻辑），**这条通道是唯一的逃生口**：
//! 当 auth 插件自己坏了（配置写错、镜像拉不到、健康探活失败），HTTP 面进不去，
//! 只能靠它把中台救回来。
//!
//! 信任边界靠**文件系统**而不是鉴权：socket 放在只有 root 能访问的目录里，能连上它的
//! 人本来就有容器/宿主机的 root。这与「网络面的管理面完全无守卫」是两回事，别混为一谈
//! ——前者是刻意留的运维通道，后者是刻意接受的风险（见 `docs/design.md` 风险表）。
//!
//! 协议：一行一个 JSON 请求、一行一个 JSON 响应，一问一答后关连接。刻意做得这么土，
//! 是因为出问题时你需要能 `socat` 或手敲 JSON 把它用起来。

pub mod client;
pub mod protocol;
pub mod server;

pub use client::request;
pub use protocol::{OpsRequest, OpsResponse};
pub use server::{OpsState, serve};
