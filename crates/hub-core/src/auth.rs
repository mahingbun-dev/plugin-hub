//! 鉴权结果：中台验过凭证之后放进请求扩展的东西。
//!
//! 它由 `hub-api::authz` 的鉴权中间件产出，被两个消费方读取：
//! HTTP 面的 `ingress` 与 MCP 工具面——两者都要把它填进信封的 `subject`。
//!
//! 放在 `hub-core` 而不是 `hub-api`：`hub-api` 与 `hub-mcp` **互不依赖**
//! （`hub-api` 没有 `hub-mcp`，`hub-mcp` 也没有 `hub-api`），而两边都要读这个类型。
//! 它是这两个面之间唯一需要共享的鉴权类型，放共用的基础 crate 最合适。

/// 信封 meta 里携带**平台登录态**的契约键。
///
/// 两个面共用同一个键：MCP 面的来源是 login 换来的 masToken（或请求自带的
/// Bearer token），HTTP 面的来源是请求自带并验过的 Bearer token。下游插件
/// （dc-dict 这类要拿登录态调平台/DC 接口的）按「键存在与否」识别登录态，
/// 所以**没有登录态时绝不注入空壳键**——「没有」≠「有但为空」。
pub const MAS_TOKEN_META: &str = "hub.mas_token";

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

    /// 本次请求携带并**验过**的 Bearer token（平台登录态）。
    ///
    /// 只有 `Authorization: Bearer` 形态的请求才有——Cookie 形态是 4A 会话，
    /// 对下游需要平台登录态的插件（dc-dict 等）说不通，不透传。
    ///
    /// 用途与 login 缓存的 masToken 一样：随信封 meta（`hub.mas_token`）带给
    /// 下游插件，让「带 token 进来的嵌入调用」端到端打通。hub 自身不落盘
    /// （内存里的请求扩展，随请求生灭）；审计侧 `redact_mas_token` 已有兜底。
    ///
    /// ⚠️ 隐私边界：**绝不**把它写进响应体或日志字段——它只是转手的凭证，
    /// 不是结果的一部分。
    pub bearer_token: Option<String>,
}
