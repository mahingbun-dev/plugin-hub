//! 鉴权结果：中台验过凭证之后放进请求扩展的东西。
//!
//! 它由 `hub-api::authz` 的鉴权中间件产出，被两个消费方读取：
//! HTTP 面的 `ingress` 与 MCP 工具面——两者都要把它填进信封的 `subject`。
//!
//! 放在 `hub-core` 而不是 `hub-api`：`hub-api` 与 `hub-mcp` **互不依赖**
//! （`hub-api` 没有 `hub-mcp`，`hub-mcp` 也没有 `hub-api`），而两边都要读这个类型。
//! 它是这两个面之间唯一需要共享的鉴权类型，放共用的基础 crate 最合适。

/// 已认证的调用主体。
///
/// 由鉴权中间件产出并放进请求扩展；下游靠它回答「这次调用是谁发起的」，
/// 进而填进信封的 `subject`。
///
/// **它只代表「这个凭证有效」，不代表「这个人有权做这件事」**——权限的判定是
/// 另一回事：HTTP 面靠 `required_scope` 比对 `scopes`，插件侧靠自己的业务规则。
/// 把「谁」与「能做什么」分开，是为了让前者可以放心透传，后者各自就近判定。
///
/// 刻意不 derive `Serialize`：它只在进程内传递，从不出现在响应体里。
/// 带了反而会让人以为它可以对外输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedSubject {
    /// 用户标识（`userCode`）
    pub user_code: String,

    /// 展示名，仅用于让审计读起来像人话
    pub user_name: String,

    /// 中台定义的权限位
    pub scopes: Vec<String>,
}
