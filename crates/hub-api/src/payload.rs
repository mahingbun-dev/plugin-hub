//! 载荷的四类边界：内联、引用、流式、异步。
//!
//! 这一层管的是**「载荷放在哪」**这个决定，四类边界各有一条明确的线：
//!
//! | 边界 | 判据 | 放哪 |
//! |---|---|---|
//! | 内联 | ≤ [`MAX_INLINE_BYTES`] | 信封的 `payload`（`Any`） |
//! | 引用 | 更大 | 落 [`hub_store::payloads`]，信封里只留 `payload_ref` |
//! | 流式 | 逐条结果 | 走 gRPC 的 `HandleStream`（插件面） |
//! | 异步 | 耗时超过一次请求能等的 | 走异步链，调用方拿 run_id |
//!
//! 后两类不是「另一个大小档」，而是**不同的交互形态**——一条数据太多要边算边给，
//! 一次执行太久要先把句柄还回去。它们分别在插件面与异步链里落地，这里只管前两类。
//!
//! ## 为什么 4MB
//!
//! 这个数来自「一次 RPC 的报文」而不是「我们想存多大」：插件面经 nginx，
//! `client_max_body_size` 与 gRPC 消息上限都在几十 MB 量级，4MB 留出了足够的安全
//! 余量，同时又不至于让常见的业务报文（几 KB 到几百 KB）频繁走引用这条更绕的路。
//!
//! ## 引用通道的口子
//!
//! 中台**不存全量报文**是贯穿始终的留存策略（见 `docs/design.md`），引用通道是这条
//! 原则的**限时例外**：带 TTL（默认 1 小时），过期即清，只做中转不做归档。
//!
//! 取回的入口是 `GET /blobs/{id}`。放在 HTTP 面而不是插件面的 gRPC `HubState` 上，
//! 是因为后者尚未实现；两者是同一份数据的两个门，`HubState` 落地后会成为插件取引用的
//! 正规路径，这个 HTTP 端点则留给控制台与排障。

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use hub_proto::PayloadRef;
use hub_proto::v1::Envelope;
use hub_store::Store;
use hub_store::payloads;
use serde::Serialize;

use crate::error::ApiError;

/// 内联上限。**信封**里装得下多少——超过它就走引用。
pub const MAX_INLINE_BYTES: usize = 4 * 1024 * 1024;

/// HTTP 面单次请求体的硬上限。**这是「我们愿意收多大」，与内联上限是两件事**。
///
/// 两者必须分开且这个更大，否则引用通道是够不着的：要走到「超限就落库再给个 uri」
/// 那一步，请求体得先被收下来。axum 默认的请求体上限是 2MB，比内联上限还小——那样
/// 超限请求会在进到我们的 handler 之前就被挡成 413，引用通道一行代码都跑不到。
///
/// 超过这个数才是真的拒收：那不是「这份数据该换个方式传」，而是「这次调用不该发」。
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// 引用载荷的默认存活时间。
///
/// 一小时：够插件取用与排障复看，又不至于让「限时中转」变成事实上的归档。
pub const DEFAULT_BLOB_TTL: Duration = Duration::from_secs(3600);

/// 走引用时留在信封里的信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PayloadRefInfo {
    /// 取回地址
    pub uri: String,

    pub sha256: String,
    pub size_bytes: u64,
}

/// 把载荷放进信封：装得下就内联，装不下就落引用。
///
/// 返回 `Some` 表示走了引用，调用方应当把它回给请求方——调用方需要知道自己的数据没有
/// 原样躺在信封里，否则它会以为插件收到了完整载荷。
pub async fn attach(
    store: &Store,
    envelope: &mut Envelope,
    payload: prost_types::Any,
) -> Result<Option<PayloadRefInfo>, ApiError> {
    if payload.value.len() <= MAX_INLINE_BYTES {
        envelope.payload = Some(payload);
        return Ok(None);
    }

    let size = payload.value.len();

    let stored = payloads::put(store.pool(), &payload.value, DEFAULT_BLOB_TTL)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?;

    envelope.payload = None;
    envelope.payload_ref = Some(PayloadRef {
        uri: format!("/blobs/{}", stored.id),
        sha256: stored.sha256.clone(),
        size_bytes: stored.size as u64,
        // 载荷是编码后的字节，类型名比 MIME 更能说明它是什么
        content_type: payload.type_url.clone(),
    });

    tracing::info!(bytes = size, blob = %stored.id, "载荷超过内联上限，改走引用通道");

    Ok(Some(PayloadRefInfo {
        uri: format!("/blobs/{}", stored.id),
        sha256: stored.sha256,
        size_bytes: stored.size as u64,
    }))
}

/// `GET /blobs/{id}` —— 取回引用通道里的载荷原始字节。
///
/// 已过期与不存在都返回 404：**对外不该区分这两者**。区分了就等于告诉调用方
/// 「这个 id 曾经存在过」，而 id 本身不该承担这个信息。
pub async fn get_blob(
    State(state): State<crate::ApiState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let row = payloads::get(state.store.pool(), &id)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("载荷 {id} 不存在或已过期")))?;

    // 摘要与大小放在头里：取回来的一方能就地校验自己拿到的是不是那一刻写进去的字节，
    // 而不必先读一遍正文
    let headers = [
        (header::CONTENT_TYPE, "application/octet-stream".to_string()),
        (header::CONTENT_LENGTH, row.size.to_string()),
        (
            header::HeaderName::from_static("x-payload-sha256"),
            row.sha256.clone(),
        ),
    ];

    let mut response = Response::new(Body::from(row.bytes));
    *response.status_mut() = StatusCode::OK;
    for (name, value) in headers {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert(name, value);
        }
    }

    Ok(response.into_response())
}
