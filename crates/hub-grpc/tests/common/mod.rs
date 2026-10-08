//! HubState 集成测试的夹具：真 PostgreSQL + 真 Redis。

use std::sync::Arc;

use hub_grpc::state::{STATE_TOKEN_METADATA, StateService};
use hub_proto::v1::hub_state_client::HubStateClient;
use hub_proto::v1::hub_state_server::HubStateServer;
use hub_proto::v1::plugin_registry_server::PluginRegistryServer;
use hub_registry::probe::AlwaysHealthy;
use hub_registry::{PluginProbe, Registry, RegistryConfig};
use hub_store::Store;
use redis::aio::ConnectionManager;
use sqlx::PgPool;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

/// 夹具句柄。
///
/// `redis` 与 `store` 是留给用例的**直连句柄**：需要绕过 API 去核对 Redis 里/
/// 库里的真实状态（例如"键前缀到底是不是中台拼的"）时用得到。`redis` 已被
/// `键前缀由中台强制拼出` 使用；`store` 目前还没有用例读它，保留是为了让要断言
/// 落库状态的用例不必改夹具——因此这里显式放行 dead_code（CI 的
/// `clippy -D warnings` 会把它当错误）。
#[allow(dead_code)]
pub struct Ctx {
    pub store: Store,
    pub redis: ConnectionManager,
    pub addr: String,
}

/// 起一个真中台（只装插件面 + 状态面），跑在随机端口上。
///
/// `pool` 由 `#[sqlx::test]` 注入：**每个用例一个独立数据库**，迁移已由它跑过。
/// 刻意不去连 `DATABASE_URL` —— 那是本机开发库，同时也是并发运行的中台实例用的库，
/// 测试数据落进去会污染它。
pub async fn setup(pool: PgPool) -> Ctx {
    setup_with_quota(pool, None).await
}

/// 与 [`setup`] 相同，但能把投递配额调小（`Some(n)`）。
///
/// 验「配额打满之后被拒」不必真发 60 条消息——那既慢，又把「配额是多少」这个
/// 实现细节焊进测试里：配额一改，测试就得跟着改那个数字。
pub async fn setup_with_quota(pool: PgPool, quota: Option<i64>) -> Ctx {
    let store = Store::from_pool(pool);

    // Redis 用独立 db 且**默认硬编码**，不读 `REDIS_URL`：本地 .env 里的那个指向 db 2，
    // 那正是中台总线在用的库，测试键会混进去。与 hub-bus 的集成测试同一做法
    // （crates/hub-bus/tests/redis_bus.rs 硬编码 /9）。
    //
    // `TEST_REDIS_URL` 只用来在 CI 里指向另一个端口——CI 自己起一个 Redis，
    // 不假定 runner 上正好有一个（跟测试 PG 用非默认端口是同一个理由）。
    let redis_url =
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/15".to_string());
    let redis = connect_redis(&redis_url)
        .await
        .expect("连 Redis 失败——本机 6379 上应有 Redis");

    let registry = Registry::new(
        store.clone(),
        // 探测恒通过：集成测试验的是状态面，不该卡在可达性探测上。
        // 用 hub-registry 自带的实现，不再自己写一个假的。
        Arc::new(AlwaysHealthy) as Arc<dyn PluginProbe>,
        RegistryConfig::default(),
    );

    // 监听随机端口
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("监听失败");
    let addr = listener.local_addr().expect("取地址失败");

    // 装上总线。**夹具与生产装配保持一致**——以 `None` 起服务会让「Publish 没装」
    // 变成测试里的默认形态，而那是生产里不该出现的一种配置：这样测出来的东西
    // 与真实部署就不是同一个了（这一条的教训是上一轮踩出来的：当时一批用例
    // 漏配了一个地址，于是真的打向了真实环境，而测试全绿）。
    let bus = hub_bus::Bus::connect(
        &redis_url,
        hub_bus::BusConfig {
            consumer: "hub-grpc-test".to_string(),
            ..hub_bus::BusConfig::default()
        },
    )
    .await
    .expect("连总线失败——夹具用同一个 Redis，只是独立消费者名");

    let client = Arc::new(hub_plugin_client::PluginClient::new(Default::default()));
    let invoker = hub_engine::Invoker::new(registry.clone(), (*client).clone());
    let async_exec = hub_engine::AsyncExecutor::new(
        store.clone(),
        bus,
        hub_engine::FlowExecutor::new(registry.clone(), invoker),
    );

    let reg_svc = hub_grpc::RegistryService::new(registry);
    // 自己组装而不是走 `hub_grpc::services()`：那个便利函数不接受配额，
    // 而验「打满之后被拒」不该真发 60 条消息——那既慢，又把「配额是多少」
    // 这个实现细节焊进测试（配额一改，测试就得跟着改数字）。
    let state_svc = match quota {
        Some(quota) => StateService::new(store.pool().clone(), redis.clone())
            .with_async(async_exec)
            .with_publish_quota(quota),
        None => StateService::new(store.pool().clone(), redis.clone()).with_async(async_exec),
    };

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(PluginRegistryServer::new(reg_svc))
            .add_service(HubStateServer::new(state_svc))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
    });

    Ctx {
        store,
        redis,
        addr: format!("http://{addr}"),
    }
}

pub async fn connect_redis(url: &str) -> redis::RedisResult<ConnectionManager> {
    let client = redis::Client::open(url)?;
    // **必须显式设响应超时**：redis-rs 的 ConnectionManager 默认 500ms，
    // 会误杀正常请求。与 hub-bus 保持一致（crates/hub-bus/src/lib.rs:151 附近），
    // 该 setter 收的是 Option<Duration>。
    let cfg = redis::aio::ConnectionManagerConfig::new()
        .set_response_timeout(Some(std::time::Duration::from_secs(5)));
    client.get_connection_manager_with_config(cfg).await
}

impl Ctx {
    pub async fn state_client(&self) -> HubStateClient<Channel> {
        HubStateClient::connect(self.addr.clone())
            .await
            .expect("连状态面失败")
    }

    /// 走完整注册流程建一个实例，返回中台下发的状态凭证。
    pub async fn register_plugin(&self, name: &str, version: &str, advertise: &str) -> String {
        let mut client =
            hub_proto::v1::plugin_registry_client::PluginRegistryClient::connect(self.addr.clone())
                .await
                .expect("连注册面失败");

        // 直接构造注册请求：本夹具的 probe 恒健康，不需要真的起一个插件进程，
        // 因此 advertise_addr 填什么都行。manifest 与 descriptor 取自 hub-testkit。
        let behavior = hub_testkit::Behavior::named(name, version);
        let req = hub_proto::v1::RegisterRequest {
            plugin_name: name.to_string(),
            version: version.to_string(),
            instance_id: format!("{name}-i1"),
            advertise_addr: advertise.to_string(),
            manifest: Some(behavior.manifest()),
            descriptor_set: behavior.descriptor(),
        };
        let resp = client
            .register(req)
            .await
            .expect("注册调用失败")
            .into_inner();
        assert!(
            resp.accepted,
            "注册应被接受，拒绝原因: {:?}",
            resp.rejections
        );
        resp.state_token
    }
}

/// 给请求挂上状态凭证。
///
/// 放在夹具里而不是各测试文件各写一份：它是每个调状态面的用例都要做的第一件事，
/// 而「凭证挂在哪个 metadata 键上」是这一层的契约——散在多处迟早会有一处写错，
/// 而写错的表现是 401，看起来像凭证本身有问题。
pub fn with_token<T>(mut req: tonic::Request<T>, token: &str) -> tonic::Request<T> {
    req.metadata_mut().insert(
        STATE_TOKEN_METADATA,
        MetadataValue::try_from(token).expect("凭证应是合法 header 值"),
    );
    req
}
