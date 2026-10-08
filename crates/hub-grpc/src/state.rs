//! 外置状态 API（`HubState`）—— 插件被强制无状态，跨调用保留的东西都走这里。
//!
//! 身份来自**注册时下发的凭证**（gRPC metadata `x-hub-state-token`），不是插件
//! 自报的任何字段：插件面本就不鉴权，自报的插件名不构成身份。中台按凭证反查出
//! 插件名，强制拼进键前缀，插件自报的 `namespace` 只能作为子空间。
//!
//! 租户维度**未实现**：请求里没有 Envelope 上下文，推导不出"当前租户"。
//! 详见 `proto/hub/v1/state.proto` 的说明。

use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use sqlx::PgPool;

use hub_proto::v1::hub_state_server::HubState;
use hub_proto::v1::{
    Envelope, KvDeleteRequest, KvDeleteResponse, KvEntry, KvGetRequest, KvGetResponse, KvKey,
    KvPutRequest, KvPutResponse, KvScanRequest, KvScanResponse, PublishRequest, PublishResponse,
    Subject, SubjectKind,
};
use tonic::{Request, Response, Status};

// 键名规则与服务端上限住在 `hub-core::state`：本地 mock 中台要用**同一份**判据，
// 而它不能依赖本 crate（会把 tonic 服务端连同 redis、sqlx 一起拖进一个本该
// 「一个进程、无外部依赖」的替身里）。
//
// **re-export 而不是让调用方改 import 路径**：`tests/state.rs` 与
// `tests/state_rules.rs` 一直是从 `hub_grpc::state::` 取这些名字的，保持它们可用，
// 这次搬迁对调用方就是透明的。
pub use hub_core::state::{
    KEY_PREFIX, MAX_SCAN_LIMIT, MAX_VALUE_BYTES, STATE_TOKEN_METADATA, valid_segment,
};

/// 触发链记账用的 meta 键：链路经过的 flow 名，逗号分隔。
///
/// 放 `meta` 而不是别处：信封是唯一一条从上游流到下游的东西，而链信息必须随它走。
const PUBLISH_CHAIN_META: &str = "hub.publish_chain";

/// 触发链长度上限。
///
/// 与「环检测」是两道互补的闸：环检测能挡住 A→B→A 这种短环，但挡不住一条
/// **没有重复节点却无限长**的链（每跳都是新 flow）。这一条挡后者。
///
/// 取 8：正常的编排链是 2–4 个节点，8 已经相当宽松；而这个数越大，一次风暴
/// 影响的插件越多。
const MAX_PUBLISH_DEPTH: usize = 8;

/// 每个插件每分钟最多投递多少条。
///
/// 与「环检测」互补的那一道：环检测看的是**形状**，它管不住一个合法但高频的
/// 插件（写错循环、跑测试）把总线打满。按插件维度记账，一个插件刷不出别人的额度。
///
/// 先用常量：它需要按真实流量调，但**没有证据之前不给它一个可配的旋钮**——
/// 配错了（比如调到 10000）等于没限额，而那种事不会有人发现。
const PUBLISH_QUOTA_PER_MINUTE: i64 = 60;

/// 拼 Redis 键。`plugin` 来自凭证反查，**不可由插件指定**。
fn redis_key(plugin: &str, namespace: &str, key: &str) -> String {
    format!("{KEY_PREFIX}:{plugin}:{namespace}:{key}")
}

/// 从信封的 `meta` 里取出触发链。
///
/// 编码是逗号分隔的 flow 名（见 [`PUBLISH_CHAIN_META`]）。**空段必须滤掉**：
/// 上游没设这个键时它可能是空串，`split(',')` 会给出一个空元素，而那个空元素
/// 会作为一个「叫空字符串的节点」参与环检测——脏数据不该有语义。
fn publish_chain(meta: &std::collections::HashMap<String, String>) -> Vec<String> {
    meta.get(PUBLISH_CHAIN_META)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 一条「受理了，但没投出去」的应答。
///
/// 刻意不用 gRPC 错误：被防环或配额拦下是**业务结果**，插件该据此改逻辑；
/// 而「中台或总线坏了」才是它该退避重试的。两者混在一个通道里就分不清了。
fn rejected(reason: impl Into<String>) -> PublishResponse {
    PublishResponse {
        accepted: false,
        run_id: String::new(),
        reason: reason.into(),
    }
}

/// 把信封的 `subject` 换成调用插件的身份。
///
/// proto：「subject 由中台按调用方（插件）身份覆盖，插件不能借此外冒用他人身份」。
/// 所以**无条件覆盖**，压根不看调用方传了什么——不覆盖的话，一个插件就能把信封
/// 伪装成「某个人提交的」，下游据此做的审计与二次确认会全部失去意义。
///
/// 单独成函数是为了能被直接断言：要验「覆盖真的发生了」得去读总线里的消息，
/// 而那比测这个纯函数麻烦得多、也脆得多。`gateway.rs` 的 Invoke 走同一覆盖语义，
/// 直接复用这里——两处各写一份迟早漂移。
pub(crate) fn stamp_plugin_subject(envelope: &mut Envelope, plugin: &str) {
    envelope.subject = Some(Subject {
        kind: SubjectKind::Plugin as i32,
        id: plugin.to_string(),
        ..Default::default()
    });
}

/// 按中台约定建一个状态面用的 Redis 连接。
///
/// **响应超时必须在这里显式设置**：`redis-rs` 的 `ConnectionManager` 默认把它设成
/// 500ms，会误杀正常请求（hub-bus 踩过一次，见 `crates/hub-bus/src/lib.rs` 里那段
/// 注释）。封装在这里而不是让每个调用方照抄配置——照抄漏了不会有任何编译期提示，
/// 而漏掉的后果是线上偶发的、看起来像 Redis 抖动的超时。
///
/// `set_response_timeout` 收的是 `Option<Duration>`（`None` 表示不限时）：
/// 必须给 `Some`，否则等于又退回默认值。
pub async fn connect_redis(url: &str) -> Result<ConnectionManager, redis::RedisError> {
    let client = redis::Client::open(url)?;
    let config = redis::aio::ConnectionManagerConfig::new()
        .set_response_timeout(Some(std::time::Duration::from_secs(5)));
    client.get_connection_manager_with_config(config).await
}

#[derive(Clone)]
pub struct StateService {
    pool: PgPool,
    redis: ConnectionManager,

    /// 下游投递能力（`Publish` 用）。`None` = 这个实例没装总线。
    ///
    /// `Option` 而不是必需：Kv 系列用不到它，而单测也不该为了验键前缀就去连
    /// Redis Stream。与 `hub_api::ApiState::async_exec` 同一模式——没装时
    /// `Publish` 明确回「这个能力没开」，而不是假装投递成功。
    async_exec: Option<hub_engine::AsyncExecutor>,

    /// 每个插件每分钟的投递上限。生产用 [`PUBLISH_QUOTA_PER_MINUTE`]，
    /// **测试里调小**——否则验一次「打满之后被拒」要发 60 条消息。
    publish_quota: i64,
}

impl StateService {
    /// `redis` 由 [`connect_redis`] 建好后传进来——超时配置只能在建连时给，
    /// 到这里已经补不上了，所以别绕过那个构造函数自己建连接。
    pub fn new(pool: PgPool, redis: ConnectionManager) -> Self {
        Self {
            pool,
            redis,
            async_exec: None,
            publish_quota: PUBLISH_QUOTA_PER_MINUTE,
        }
    }

    /// 装上总线，开启 `Publish`。
    pub fn with_async(mut self, exec: hub_engine::AsyncExecutor) -> Self {
        self.async_exec = Some(exec);
        self
    }

    /// 改投递配额。生产不走这条路（用默认值），测试用它把窗口调小。
    pub fn with_publish_quota(mut self, quota: i64) -> Self {
        self.publish_quota = quota;
        self
    }

    /// 这个插件本分钟还能不能投递。返回 `Some(原因)` 表示已经超了。
    ///
    /// **固定窗口计数**（`INCR` + `EXPIRE`），不是滑动窗口：窗口边界的突发是已知
    /// 代价，而滑动窗口要 ZSET 加清理——为一道「防风暴」的闸不值得。它挡的是
    /// 「写错循环 / 跑测试刷接口的插件」，不是精确的流量整形。
    ///
    /// 按插件维度记账：一个插件刷不出别人的额度。
    async fn over_quota(&self, plugin: &str) -> Result<Option<String>, Status> {
        let window = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() / 60)
            .unwrap_or(0);
        let key = format!("hub:publish_quota:{plugin}:{window}");

        let mut conn = self.redis.clone();
        // 先加再判：`INCR` 是原子的，并发的两条各自拿到自己的序号，不会互相覆盖
        let count: i64 = conn.incr(&key, 1).await.map_err(|err| {
            tracing::error!(error = %err, plugin, "投递配额记账失败");
            Status::internal("投递配额记账失败")
        })?;

        // **只在第一次设过期**。每次都设会让窗口永远停在「本分钟」，计数只增不减，
        // 插件迟早被自己的历史累计卡死——而那种故障看起来像「配额调小了」。
        if count == 1 {
            let _: Result<(), _> = conn.expire(&key, 120).await;
        }

        if count > self.publish_quota {
            return Ok(Some(format!(
                "本分钟投递已达上限 {} 次（本次是第 {count} 次）",
                self.publish_quota
            )));
        }
        Ok(None)
    }

    /// 校验凭证并返回插件名。
    ///
    /// 鉴权核心（metadata 取凭证 → 直查 PG 反查插件名）已抽成
    /// `gateway::authenticate_plugin` 供 GatewayService 共用——两个面认的是
    /// 同一种身份，两份实现迟早漂移。
    async fn authenticate<T>(&self, request: &Request<T>) -> Result<String, Status> {
        crate::gateway::authenticate_plugin(&self.pool, request).await
    }

    fn key_of(key: Option<KvKey>) -> Result<KvKey, Status> {
        key.ok_or_else(|| Status::invalid_argument("缺少 key"))
    }

    fn check_segments(ns: &str, key: &str) -> Result<(), Status> {
        if !valid_segment(ns) {
            return Err(Status::invalid_argument(format!(
                "namespace 只允许 [A-Za-z0-9_.-] 且非空，收到 {ns:?}"
            )));
        }
        if !valid_segment(key) {
            return Err(Status::invalid_argument(format!(
                "key 只允许 [A-Za-z0-9_.-] 且非空，收到 {key:?}"
            )));
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl HubState for StateService {
    async fn kv_get(
        &self,
        request: Request<KvGetRequest>,
    ) -> Result<Response<KvGetResponse>, Status> {
        let plugin = self.authenticate(&request).await?;
        let inner = request.into_inner();
        let key = Self::key_of(inner.key)?;
        Self::check_segments(&key.namespace, &key.key)?;

        let mut conn = self.redis.clone();
        let value: Option<Vec<u8>> = conn
            .get(redis_key(&plugin, &key.namespace, &key.key))
            .await
            .map_err(|err| {
                tracing::error!(error = %err, plugin = %plugin, "读状态失败");
                Status::internal("读状态失败")
            })?;

        Ok(Response::new(match value {
            // found 与"值是空字节"是两回事：前者是键在不在，后者是值本身为空
            Some(value) => KvGetResponse { found: true, value },
            None => KvGetResponse {
                found: false,
                value: Vec::new(),
            },
        }))
    }

    async fn kv_put(
        &self,
        request: Request<KvPutRequest>,
    ) -> Result<Response<KvPutResponse>, Status> {
        let plugin = self.authenticate(&request).await?;
        let inner = request.into_inner();
        let key = Self::key_of(inner.key)?;
        Self::check_segments(&key.namespace, &key.key)?;

        if inner.value.len() > MAX_VALUE_BYTES {
            return Err(Status::invalid_argument(format!(
                "value 超过上限 {} 字节，收到 {} 字节",
                MAX_VALUE_BYTES,
                inner.value.len()
            )));
        }
        if inner.ttl_seconds < 0 {
            return Err(Status::invalid_argument("ttl_seconds 不能为负"));
        }

        let full_key = redis_key(&plugin, &key.namespace, &key.key);
        let mut conn = self.redis.clone();
        if inner.ttl_seconds > 0 {
            let _: () = conn
                .set_ex(full_key, inner.value, inner.ttl_seconds as u64)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, plugin = %plugin, "写状态失败");
                    Status::internal("写状态失败")
                })?;
        } else {
            let _: () = conn.set(full_key, inner.value).await.map_err(|err| {
                tracing::error!(error = %err, plugin = %plugin, "写状态失败");
                Status::internal("写状态失败")
            })?;
        }

        Ok(Response::new(KvPutResponse {}))
    }

    async fn kv_delete(
        &self,
        request: Request<KvDeleteRequest>,
    ) -> Result<Response<KvDeleteResponse>, Status> {
        let plugin = self.authenticate(&request).await?;
        let inner = request.into_inner();
        let key = Self::key_of(inner.key)?;
        Self::check_segments(&key.namespace, &key.key)?;

        let mut conn = self.redis.clone();
        let removed: usize = conn
            .del(redis_key(&plugin, &key.namespace, &key.key))
            .await
            .map_err(|err| {
                tracing::error!(error = %err, plugin = %plugin, "删状态失败");
                Status::internal("删状态失败")
            })?;

        // 删不存在的键返回 deleted=false，不是错误：调用方的意图（这键没了）已达成
        Ok(Response::new(KvDeleteResponse {
            deleted: removed > 0,
        }))
    }

    async fn kv_scan(
        &self,
        request: Request<KvScanRequest>,
    ) -> Result<Response<KvScanResponse>, Status> {
        let plugin = self.authenticate(&request).await?;
        let inner = request.into_inner();

        if !valid_segment(&inner.namespace) {
            return Err(Status::invalid_argument(format!(
                "namespace 只允许 [A-Za-z0-9_.-] 且非空，收到 {:?}",
                inner.namespace
            )));
        }
        // prefix 允许为空（扫整个命名空间），非空时同样只接受白名单字符
        if !inner.prefix.is_empty() && !valid_segment(&inner.prefix) {
            return Err(Status::invalid_argument(format!(
                "prefix 只允许 [A-Za-z0-9_.-]，收到 {:?}",
                inner.prefix
            )));
        }
        if inner.limit == 0 || inner.limit > MAX_SCAN_LIMIT {
            return Err(Status::invalid_argument(format!(
                "limit 必须在 1..={MAX_SCAN_LIMIT} 之间，收到 {}",
                inner.limit
            )));
        }

        // 前缀里含插件名，模式匹配被限制在本插件的命名空间内
        let pattern = format!(
            "{KEY_PREFIX}:{plugin}:{}:{}*",
            inner.namespace, inner.prefix
        );
        let full_prefix = format!("{KEY_PREFIX}:{plugin}:{}:", inner.namespace);

        let mut conn = self.redis.clone();
        let mut cursor: u64 = 0;
        let mut entries = Vec::new();

        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(100)
                .query_async(&mut conn)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, plugin = %plugin, "扫描状态失败");
                    Status::internal("扫描状态失败")
                })?;

            for full in batch {
                if entries.len() >= inner.limit as usize {
                    break;
                }
                // 值一并取回：返回键不给值，调用方还得再查一次
                let value: Option<Vec<u8>> = conn.get(&full).await.map_err(|err| {
                    tracing::error!(error = %err, plugin = %plugin, "扫描取值失败");
                    Status::internal("扫描状态失败")
                })?;
                entries.push(KvEntry {
                    // 对外只暴露插件自己的键名，不带前缀
                    key: full.strip_prefix(&full_prefix).unwrap_or(&full).to_string(),
                    value: value.unwrap_or_default(),
                });
            }

            cursor = next;
            if cursor == 0 || entries.len() >= inner.limit as usize {
                break;
            }
        }

        Ok(Response::new(KvScanResponse { entries }))
    }

    /// 把一条信封投给下游 flow。
    ///
    /// proto 里写着「中台负责防环与限流，防止插件互相触发形成风暴」——这里是那两件
    /// 事唯一的落点。
    ///
    /// **拒绝一律走 `accepted: false` + `reason`，不返回 gRPC 错误**：被防环或配额
    /// 拦下是**业务结果**（插件该据此改逻辑，而不是重试），与「中台或总线坏了」是
    /// 两回事。两者混进同一个错误通道，插件侧就分不清该退避还是该改代码。
    ///
    /// 反过来，投递失败时**要分开这两种**——见下面 `enqueue` 的错误分支。
    async fn publish(
        &self,
        request: Request<PublishRequest>,
    ) -> Result<Response<PublishResponse>, Status> {
        // 身份先认：防环按链记、限额按插件记账，两者都要它
        let plugin = self.authenticate(&request).await?;

        let Some(exec) = self.async_exec.as_ref() else {
            // 没装总线时明确说「这个能力没开」。**绝不假装投递成功**——
            // 静默丢消息（或谎报受理）是最难查的一类问题
            return Err(Status::unavailable(
                "本实例未装配总线，Publish 不可用（见 hub-server 的装配）",
            ));
        };

        let inner = request.into_inner();
        let target = inner.target.trim().to_string();
        if target.is_empty() {
            return Err(Status::invalid_argument("target 不能为空"));
        }
        let mut envelope = inner
            .envelope
            .ok_or_else(|| Status::invalid_argument("缺少信封"))?;

        // ---- 防环 ----
        //
        // 链上每个 flow 的名字随 `meta` 往下传。用**链**而不是只数深度：
        // 深度只能发现「太深」，而 A→B→A 深度才 2 就已经成环了。
        let chain = publish_chain(&envelope.meta);
        if chain.iter().any(|node| node == &target) {
            return Ok(Response::new(rejected(format!(
                "检测到环：{target} 已在本次触发链上（{}）",
                chain.join(" → ")
            ))));
        }
        if chain.len() >= MAX_PUBLISH_DEPTH {
            return Ok(Response::new(rejected(format!(
                "触发链已达 {MAX_PUBLISH_DEPTH} 跳上限（{}）",
                chain.join(" → ")
            ))));
        }

        // ---- 限额 ----
        if let Some(reason) = self.over_quota(&plugin).await? {
            return Ok(Response::new(rejected(reason)));
        }

        // ---- 身份 ----
        //
        // 无条件覆盖成调用插件的身份，理由见 `stamp_plugin_subject`
        stamp_plugin_subject(&mut envelope, &plugin);

        // 把当前这一跳记进链再传下去，下游才检得出回到自己身上的环
        let mut next_chain = chain;
        next_chain.push(target.clone());
        envelope
            .meta
            .insert(PUBLISH_CHAIN_META.to_string(), next_chain.join(","));

        match exec.enqueue(&target, envelope, None).await {
            Ok(run_id) => Ok(Response::new(PublishResponse {
                accepted: true,
                run_id,
                reason: String::new(),
            })),

            // 目标不存在 / 未发布：**调用方能修的问题**（对方的配置不对），
            // 当业务结果回，附上原因让它自己判断
            Err(hub_engine::AsyncError::Store(hub_store::StoreError::Invalid(msg))) => {
                Ok(Response::new(rejected(msg)))
            }

            // 其余（总线、数据库）是故障：当故障回。这种情况下让插件重试是对的，
            // 而 `accepted: false` 会让它以为是自己写错了，于是不去重试
            Err(err) => {
                tracing::error!(error = %err, plugin = %plugin, target = %target, "Publish 投递失败");
                Err(Status::internal("投递失败，请稍后重试"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 触发链从_meta_解析且忽略空段() {
        let mut meta = std::collections::HashMap::new();
        assert!(publish_chain(&meta).is_empty(), "没有这个键时链为空");

        // 上游写了分隔符但没写名字：空段不该变成一个「叫空字符串的节点」，
        // 那会让环检测拿着一条脏数据去比对
        meta.insert(PUBLISH_CHAIN_META.to_string(), " , ,".to_string());
        assert!(publish_chain(&meta).is_empty(), "空段要被滤掉");

        meta.insert(PUBLISH_CHAIN_META.to_string(), " a , b ,c".to_string());
        assert_eq!(
            publish_chain(&meta),
            vec!["a", "b", "c"],
            "去空白且保持顺序"
        );
    }

    #[test]
    fn 投递时无条件覆盖调用方自报的身份() {
        // 调用方塞了一个「我是某个人」的身份——这正是要防的冒用
        let mut envelope = Envelope {
            subject: Some(Subject {
                kind: SubjectKind::Human as i32,
                id: "冒充者".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };

        stamp_plugin_subject(&mut envelope, "sql-executor");

        let subject = envelope.subject.expect("应当被覆盖成 Some");
        assert_eq!(
            subject.id, "sql-executor",
            "身份必须是调用插件，不是它自报的"
        );
        assert_eq!(
            subject.kind,
            SubjectKind::Plugin as i32,
            "发起主体是插件而不是人——下游据此决定该不该信任这次调用"
        );
    }

    #[test]
    fn 本就没有身份的信封也会被盖上() {
        let mut envelope = Envelope::default();
        stamp_plugin_subject(&mut envelope, "publisher");
        assert_eq!(envelope.subject.expect("应当有身份").id, "publisher");
    }
}
