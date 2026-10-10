//! 管理面的鉴权：权限位、路径映射与中间件。
//!
//! **中台定义权限位，插件回答「这个凭证有哪几个」**。中台不知道谁是管理员——
//! 那是 anc 平台权限体系的事；它只知道「这个接口需要什么」。这条分工是刻意的：
//! 把「谁能做什么」放进中台，就等于把平台的权限体系在这里抄了一份，两边迟早不一致。
//!
//! ⚠️ 这一层**默认关闭**：没配 `HUB_AUTH_PLUGIN` 时整层不生效，管理面维持原样
//! （无内置守卫）。这是过渡期的形态——按设计，管理面最终是全插件化的。
//! 之所以给一个开关而不是直接启用：UAT 上还没部署 auth 插件，
//! 直接启用会让管理面在插件就位之前谁也进不去。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hub_engine::InvokeOutcome;
use hub_proto::v1::{Envelope, PayloadType, Subject, SubjectKind};
use serde::Serialize;

use crate::ApiState;

// ---------------------------------------------------------------- 权限位

/// 读：插件目录、编排定义、执行记录、调用链、死信列表、触发器列表
pub const SCOPE_READ: &str = "hub:read";

/// 调用：触发编排、调 MCP 工具。它会真的打插件、真的产生副作用
pub const SCOPE_INVOKE: &str = "hub:invoke";

/// 改：保存草稿、登记触发器。改的是「还没生效的那一份」
pub const SCOPE_EDIT: &str = "hub:edit";

/// 发布：把草稿推成生产流量走的那一版。**单独一位，不与 edit 合并**——
/// 改草稿是低风险的日常动作，发布不是；把两者并成一个位等于让所有编辑者都能发布
pub const SCOPE_PUBLISH: &str = "hub:publish";

/// 管理：治理快照、死信重放、删除触发器
pub const SCOPE_ADMIN: &str = "hub:admin";

/// 全部权限位。给控制台与文档用。
pub const ALL_SCOPES: [&str; 5] = [
    SCOPE_READ,
    SCOPE_INVOKE,
    SCOPE_EDIT,
    SCOPE_PUBLISH,
    SCOPE_ADMIN,
];

// ---------------------------------------------------------------- 路径映射

/// 一个请求要过哪道闸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// 不鉴权：探活、指标、业务数据入口
    Open,

    /// 需要这个权限位
    Needs(&'static str),

    /// 这个路径不在映射表里
    ///
    /// **放行**，交给 axum 自己的 404——在这里凭空发明一个权限位，
    /// 会让「路径写错了」表现为 403，把真正的问题藏起来。
    /// 漏配的风险由 `每个路由都有权限映射` 那条测试兜住。
    Unmapped,
}

/// 这个请求要过哪道闸。
///
/// 判定顺序是从具体到宽泛：`/dead-letters/{id}/replay` 必须排在 `/dead-letters`
/// 之前，否则重放会被当成一次「读」放过去。
pub fn required_scope(method: &Method, path: &str) -> Guard {
    // 探活与指标不鉴权：它们不依赖任何业务状态，且 nginx / Prometheus 要能直接探。
    // 给它们加鉴权只会让「中台是不是活着」变成一个需要凭证才能回答的问题。
    if path == "/health" || path == "/metrics" {
        return Guard::Open;
    }

    // 业务数据入口**要鉴权**。
    //
    // 这里改过一次：原设计是「不鉴权，鉴权由插件自己承担」，理由是插件可以自己认凭证。
    // 但只要有一个插件要求「每次调用必须可归因到具体的人」（SQL 执行器就是第一个），
    // 那条设计就撑不住了——插件自己认凭证意味着每个插件都要重复实现一遍鉴权，
    // 且拿不到中台这里已经算好的权限位。
    //
    // 复用 `hub:invoke`：它的定义「触发编排、调 MCP 工具——会真的打插件、产生副作用」
    // 与「走 ingress 调插件」完全同义。中台不为具体业务领域新造权限位。
    //
    // 未配 `HUB_AUTH_PLUGIN` 时整层不生效，这条路照旧放行（过渡期不阻塞）。
    if path.starts_with("/ingress/") {
        return Guard::Needs(SCOPE_INVOKE);
    }

    // 引用通道不鉴权，但**理由不是「凭证已经给过插件了」**（那只在 ingress 不鉴权时
    // 成立）。真实的保护是两条：id 是 ULID（80 位随机，猜不出来），且 1 小时即过期
    // ——这是一条 capability URL，谁拿到 id 谁能取。
    //
    // 它的边界要说清：**id 一旦随响应流出（被转发、被记进日志），就不能再当作秘密**。
    // 所以大载荷里不该放「拿到 id 就等于拿到权限」的东西。
    if path.starts_with("/blobs/") {
        return Guard::Open;
    }

    // 插件脚手架模板。**刻意不鉴权**：它是「怎么开发一个插件」的开发资料，不是运行中
    // 的数据——管理面鉴权管的是后者。也不属于「认不出来的路径」：那样虽然同样放行，
    // 但语义是「路径写错了」，会把真正的问题藏起来。
    //
    // 要收紧时把这一条改成 Needs(SCOPE_READ) 即可（模板包里有内网地址与 SDK 仓库路径）。
    if path == "/plugin-templates" || path.starts_with("/plugin-templates/") {
        return Guard::Open;
    }

    // ---- 具体路径排在通配之前 ----

    if path.ends_with("/replay") && path.starts_with("/dead-letters") {
        return Guard::Needs(SCOPE_ADMIN);
    }

    if path.starts_with("/dead-letters") {
        return Guard::Needs(SCOPE_READ);
    }

    if path == "/triggers" || path.starts_with("/triggers/") {
        // 读触发器是只读；启停与删除改变「这条编排还会不会被触发」
        return if method == Method::GET {
            Guard::Needs(SCOPE_READ)
        } else {
            Guard::Needs(SCOPE_EDIT)
        };
    }

    // 发布单独一个位。它排在 `/flows` 那条通配之前，否则会被当成一次「写草稿」
    if path.ends_with("/publish") {
        return Guard::Needs(SCOPE_PUBLISH);
    }

    // 触发会真的跑一次编排、真的打插件
    if path.ends_with("/trigger") || path.ends_with("/trigger-async") {
        return Guard::Needs(SCOPE_INVOKE);
    }

    // 触发器登记挂在 flow 下面（`/flows/{flow}/triggers`）
    if path.starts_with("/flows") && path.ends_with("/triggers") {
        return Guard::Needs(SCOPE_EDIT);
    }

    if path.starts_with("/flows") {
        return if method == Method::GET {
            Guard::Needs(SCOPE_READ)
        } else {
            Guard::Needs(SCOPE_EDIT)
        };
    }

    if path.starts_with("/runs") || path.starts_with("/traces") {
        return Guard::Needs(SCOPE_READ);
    }

    // 管理面：插件目录、实例、契约影响面、治理快照
    if path.starts_with("/admin/") {
        return Guard::Needs(SCOPE_ADMIN);
    }

    // MCP 工具面。它既能读也能调用，按其中**更宽松的那一档**算——
    // 保守取值只会让权限配得更细的人打不着工具，而那正是这一位想表达的能力
    if path == "/mcp" {
        return Guard::Needs(SCOPE_INVOKE);
    }

    Guard::Unmapped
}

// ---------------------------------------------------------------- 中间件

/// 鉴权配置。没配就是这一层不生效。
#[derive(Debug, Clone)]
pub struct AuthzConfig {
    /// auth 插件名
    pub plugin: String,

    /// 版本约束；`None` 表示跟随最新版本
    pub version: Option<String>,

    /// MCP 登录闸门是否开启（与 hub-mcp 的 `HUB_MCP_LOGIN_GATE` 同源）。
    ///
    /// 开启时 `/mcp` 的**无凭证与无效凭证请求放行到工具面**，由闸门接管
    /// （elicitation 弹窗 / 指引卡）——否则 MCP 客户端没有浏览器 Cookie，
    /// HTTP 层 401 会把它死锁在会话建立之前，连 `login` 工具都调不到。
    /// 关闭时 `/mcp` 与其余路径同样 401，行为与改造前完全一致。
    pub mcp_login_gate: bool,
}

/// 鉴权中间件。
pub async fn authorize(
    State(state): State<ApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(config) = state.authz.clone() else {
        // 没配 auth 插件：整层不生效。管理面维持原样（无内置守卫）
        return next.run(request).await;
    };

    let needed = match required_scope(request.method(), request.uri().path()) {
        Guard::Open | Guard::Unmapped => return next.run(request).await,
        Guard::Needs(scope) => scope,
    };

    // /mcp + 闸门开启 = 「HTTP 层让路、闸门接管」的形态。只对无凭证与无效凭证
    // 生效（见下面两个分支）；**有效凭证照常认证**，闸门只是兜底不是旁路。
    let mcp_gate_active = config.mcp_login_gate && request.uri().path() == "/mcp";

    let Some(credential) = credential_of(&request) else {
        if mcp_gate_active {
            // 匿名进工具面：闸门会拦下插件调用并弹窗/给指引卡。
            // tools/list、initialize 这类元操作本就要匿名可达——agent 得先
            // 看到工具列表才知道有 login 可调。
            return next.run(request).await;
        }
        return unauthorized("未携带登录凭证", None);
    };

    let subject = match authenticate(&state, &config, &credential).await {
        Ok(subject) => subject,
        Err(response) => {
            // 凭证无效（401）在 /mcp + 闸门开时同样交给闸门接管：MCP 客户端的
            // token 过期后，它需要的是「重新建立身份」的指引卡，不是一条它
            // 没法处理的 HTTP 401。
            //
            // **只让 401 走这条路**：503（插件不可达）/500（契约破损）是基础设施
            // 故障，原样穿透——降级放行会把 auth 插件抖动伪装成「没人登录」。
            if mcp_gate_active && response.status() == StatusCode::UNAUTHORIZED {
                return next.run(request).await;
            }
            return response;
        }
    };

    if !subject.scopes.iter().any(|s| s == needed) {
        return forbidden(needed, &subject);
    }

    // 认证过的身份挂进扩展，后面的 handler 要审计时能拿到——
    // 没有它的话，审计里只能记「有人做了这件事」，记不下「是谁」。
    // 只在可信路径上插入：这是中间件写的，请求方伪造不了
    request.extensions_mut().insert(Arc::new(subject));
    next.run(request).await
}

// ---------------------------------------------------------------- 凭证提取

/// 请求携带的登录凭证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// 浏览器 Cookie 原文（4A 会话形态，存量通道）
    Cookie(String),
    /// `Authorization: Bearer <token>` 的 token（平台登录态，嵌入场景新通道）
    Bearer(String),
}

impl Credential {
    /// WWW-Authenticate 里该指引的认证入口。
    fn scheme(&self) -> &'static str {
        match self {
            Credential::Cookie(_) => "Cookie",
            Credential::Bearer(_) => "Bearer",
        }
    }
}

/// 取请求里的登录凭证。
///
/// **Bearer 优先于 Cookie**：显式带 token 的调用方（MCP 客户端 headers 配置、
/// 服务端转发）意图明确，且是嵌入场景的新通道；Cookie 是存量通道，两者同现时
/// 听新的。两者都取不到返回 `None`——交给调用方按路径决定 401 还是闸门接管。
fn credential_of(request: &Request) -> Option<Credential> {
    // RFC 7235：auth-scheme 大小写不敏感，所以 `Bearer`/`bearer` 都认
    if let Some(value) = request.headers().get(header::AUTHORIZATION)
        && let Ok(value) = value.to_str()
        && let Some((scheme, token)) = value.trim().split_once(' ')
        && scheme.eq_ignore_ascii_case("bearer")
    {
        let token = token.trim();
        if !token.is_empty() {
            return Some(Credential::Bearer(token.to_string()));
        }
    }

    request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Credential::Cookie(value.to_string()))
}

// 类型定义搬到了 `hub-core`：MCP 工具面（`hub-mcp`）也要读它来填信封的 `subject`，
// 而 `hub-mcp` 不依赖 `hub-api`（两个面互相独立）。这里 re-export，
// 让现有调用方的 `use crate::authz::AuthenticatedSubject` 不必改。
pub use hub_core::AuthenticatedSubject;

/// 问 auth 插件「这个凭证是谁、有哪几个权限位」。
///
/// 返回值里的 `Err` 已经是一个可以直接回给调用方的响应。
///
/// 载荷用**新契约** `{kind, credential}`：中间件按头形态已经分好了 cookie/token，
/// 插件照 kind 走对应链路（cookie → 4A+平台双链路，token → 平台直验）——
/// 不让插件猜凭证类型，猜错的白跑一趟都算贵的。
// Err 直接携带 axum Response（其 Body 本身就胖）是本 crate 的统一形态；
// 该函数在管理面每请求至多一次，不是热路径——对 result_large_err 显式豁免。
#[allow(clippy::result_large_err)]
async fn authenticate(
    state: &ApiState,
    config: &AuthzConfig,
    credential: &Credential,
) -> Result<AuthenticatedSubject, Response> {
    let (kind, credential_value) = match credential {
        Credential::Cookie(cookie) => ("cookie", cookie),
        Credential::Bearer(token) => ("token", token),
    };
    let mut envelope = Envelope {
        r#type: PayloadType::Request as i32,
        subject: Some(Subject {
            kind: SubjectKind::System as i32,
            id: "hub:authz".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let payload = match hub_proto::encode_payload(&serde_json::json!({
        "kind": kind,
        "credential": credential_value,
    })) {
        Ok(payload) => payload,
        Err(err) => {
            // 编码失败只可能是中台自己的 bug（载荷是个普通对象）
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody::new(
                    "internal_error",
                    format!("鉴权载荷编码失败: {err}"),
                )),
            )
                .into_response());
        }
    };
    envelope.payload = Some(payload);

    let outcome = state
        .invoker
        .invoke(&config.plugin, config.version.as_deref(), envelope)
        .await;

    match outcome {
        Ok(InvokeOutcome::Handled { envelope, .. }) => {
            let Some(payload) = envelope
                .payload
                .as_ref()
                .and_then(hub_proto::decode_payload)
            else {
                return Err(plugin_misconfigured("auth 插件没有返回 JSON 载荷"));
            };

            if payload.get("authenticated").and_then(|v| v.as_bool()) != Some(true) {
                let reason = payload
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("凭证无效");
                return Err(unauthorized(reason, Some(credential.scheme())));
            }

            let user_code = payload
                .get("userCode")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let scopes = payload
                .get("scopes")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            Ok(AuthenticatedSubject {
                user_code: user_code.clone(),
                user_name: payload
                    .get("userName")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&user_code)
                    .to_string(),
                scopes,
                // 只有 Bearer 形态才随身份携带 token：Cookie 是 4A 会话，
                // 对下游要平台登录态的插件没有用处
                bearer_token: match credential {
                    Credential::Bearer(token) => Some(token.clone()),
                    Credential::Cookie(_) => None,
                },
            })
        }

        // 校验器拒绝：凭证连形状都不对，等同于未认证
        Ok(InvokeOutcome::Rejected { issues, .. }) => Err(unauthorized(
            &format!(
                "凭证未通过校验：{}",
                issues
                    .iter()
                    .map(|i| i.message.as_str())
                    .collect::<Vec<_>>()
                    .join("；")
            ),
            Some(credential.scheme()),
        )),

        // **插件不可达是基础设施故障，不能降级成匿名放行**。
        // 降级意味着一次 auth 插件抖动会让整个管理面变成无守卫——
        // 那正好是这一层想避免的事
        Err(err) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody::new(
                "auth_unavailable",
                format!("鉴权插件不可用，无法判定权限：{err}"),
            )),
        )
            .into_response()),
    }
}

fn unauthorized(reason: &str, scheme: Option<&str>) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(ErrorBody::new("unauthorized", reason.to_string())),
    )
        .into_response();
    // 明确告诉调用方「去哪个认证入口」——没有它，前端只能猜是跳登录还是刷新凭证
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_str(scheme.unwrap_or("Cookie"))
            .unwrap_or_else(|_| header::HeaderValue::from_static("Cookie")),
    );
    response
}

fn forbidden(needed: &str, subject: &AuthenticatedSubject) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorBody::new(
            "forbidden",
            format!(
                "凭证有效，但缺少权限位 {needed}（当前持有：{}）",
                if subject.scopes.is_empty() {
                    "无".to_string()
                } else {
                    subject.scopes.join(", ")
                }
            ),
        )),
    )
        .into_response()
}

/// 插件返回了东西，但形状不对——这是**中台与插件的契约没对上**，
/// 不是调用方的问题，所以是 500 而不是 4xx
fn plugin_misconfigured(detail: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody::new(
            "auth_plugin_misconfigured",
            detail.to_string(),
        )),
    )
        .into_response()
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    message: String,
}

impl ErrorBody {
    fn new(error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_of(method: Method, path: &str) -> Guard {
        required_scope(&method, path)
    }

    #[test]
    fn 探活与指标不鉴权() {
        // 给它们加鉴权会让「中台是不是活着」变成一个需要凭证才能回答的问题
        assert_eq!(scope_of(Method::GET, "/health"), Guard::Open);
        assert_eq!(scope_of(Method::GET, "/metrics"), Guard::Open);
    }

    #[test]
    fn 业务入口要鉴权而引用通道不用() {
        // 原设计是「ingress 不鉴权，由插件自己承担」；改成要鉴权是因为只要有插件要求
        // 「每次调用可归因到具体的人」，那条设计就撑不住——插件自己认凭证等于每个插件
        // 重复实现一遍鉴权，还拿不到中台算好的权限位
        assert_eq!(
            scope_of(Method::POST, "/ingress/order-reader"),
            Guard::Needs(SCOPE_INVOKE)
        );

        // 引用通道是 capability URL：靠 ULID 不可猜 + 短 TTL，不靠凭证
        assert_eq!(scope_of(Method::GET, "/blobs/abc123"), Guard::Open);
    }

    #[test]
    fn 脚手架模板不鉴权() {
        // 模板是「怎么开发一个插件」的开发资料，不是运行中的数据。
        // 这两条同时锁住「增量路由别忘了进映射表」——漏配会落到 Unmapped，
        // 虽然同样放行，但那是「路径写错了」的语义，会把真实问题藏起来。
        assert_eq!(scope_of(Method::GET, "/plugin-templates"), Guard::Open);
        assert_eq!(
            scope_of(Method::GET, "/plugin-templates/go/download"),
            Guard::Open
        );
    }

    #[test]
    fn 发布与改草稿是两位() {
        // 把两者并成一个位等于让所有编辑者都能发布
        assert_eq!(
            scope_of(Method::POST, "/flows/intake/draft"),
            Guard::Needs(SCOPE_EDIT)
        );
        assert_eq!(
            scope_of(Method::POST, "/flows/intake/publish"),
            Guard::Needs(SCOPE_PUBLISH)
        );
    }

    #[test]
    fn 读与写按方法分() {
        assert_eq!(scope_of(Method::GET, "/flows"), Guard::Needs(SCOPE_READ));
        assert_eq!(
            scope_of(Method::POST, "/flows/intake/draft"),
            Guard::Needs(SCOPE_EDIT)
        );
        assert_eq!(scope_of(Method::GET, "/runs"), Guard::Needs(SCOPE_READ));
        assert_eq!(
            scope_of(Method::GET, "/traces/abc"),
            Guard::Needs(SCOPE_READ)
        );
    }

    #[test]
    fn 具体路径排在通配之前() {
        // 重放是写操作、还会真的触发一次执行，不能被 `/dead-letters` 那条读规则吃掉
        assert_eq!(
            scope_of(Method::POST, "/dead-letters/7/replay"),
            Guard::Needs(SCOPE_ADMIN)
        );
        assert_eq!(
            scope_of(Method::GET, "/dead-letters"),
            Guard::Needs(SCOPE_READ)
        );

        // 触发器登记挂在 flow 下面，别被 `/flows` 的读规则吃掉
        assert_eq!(
            scope_of(Method::POST, "/flows/intake/triggers"),
            Guard::Needs(SCOPE_EDIT)
        );

        // 触发是 invoke，不是 edit
        assert_eq!(
            scope_of(Method::POST, "/flows/intake/trigger"),
            Guard::Needs(SCOPE_INVOKE)
        );
        assert_eq!(
            scope_of(Method::POST, "/flows/intake/trigger-async"),
            Guard::Needs(SCOPE_INVOKE)
        );
    }

    #[test]
    fn 治理与插件目录归管理位() {
        assert_eq!(
            scope_of(Method::GET, "/admin/plugins"),
            Guard::Needs(SCOPE_ADMIN)
        );
        assert_eq!(
            scope_of(Method::GET, "/admin/governance"),
            Guard::Needs(SCOPE_ADMIN)
        );
        // 拒绝留痕是只读，但也在管理面下：看得到全部插件的排障事实
        assert_eq!(
            scope_of(Method::GET, "/admin/rejections"),
            Guard::Needs(SCOPE_ADMIN)
        );
        // 删除版本是管理面上唯一的破坏性恢复操作，DELETE 也必须归管理位
        assert_eq!(
            scope_of(Method::DELETE, "/admin/plugins/sql-executor/versions/0.2.0"),
            Guard::Needs(SCOPE_ADMIN)
        );
    }

    #[test]
    fn 认不出来的路径交给_404() {
        assert_eq!(scope_of(Method::GET, "/never-heard-of-it"), Guard::Unmapped);
    }
}
