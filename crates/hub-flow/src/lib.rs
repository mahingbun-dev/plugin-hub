//! plugin-hub 编排层：flow 定义、静态校验与执行计划。
//!
//! 这个 crate 是**纯逻辑的**——不碰数据库、不发网络请求。它回答三个问题：
//!
//! 1. 一条编排长什么样（[`FlowDefinition`]）
//! 2. 它能不能存（[`validate`]）——接错线、连成环、契约对不上，都在保存时拦住
//! 3. 它该怎么跑（[`plan`]）——排出可并发的层，交给执行引擎
//!
//! 把这三件事放在一起、且不依赖任何基础设施，是为了让它们能被穷尽地测试：
//! 编排错了会静默跑偏，这是最不能靠「上线看看」来发现的一类问题。

pub mod definition;
pub mod plan;
pub mod validate;

pub use definition::{Edge, FlowDefinition, MAX_NODES, NAME_PATTERN, Node, is_valid_name};
pub use plan::{ExecutionPlan, PlanError, plan};
pub use validate::{
    Code, FlowIssue, MAX_NODE_TIMEOUT_MS, PluginAvailability, PluginVersion, ResolvedNode,
    Severity, has_errors, pick_version, summarize, validate,
};
