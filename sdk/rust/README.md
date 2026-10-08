# anc-hub 插件 SDK（Rust）

用 Rust 写 anc-hub 插件的工具箱。

| 件 | 位置 | 作用 |
|---|---|---|
| ① 服务端骨架 | `src/run.rs`（`hubkit::run`） | 实现两个方法就有个能跑的插件：gRPC 服务、自注册、心跳、被摘除后自愈、优雅退出全由它处理 |
| ② 契约定义 | `src/proto/` | 由 `crates/hub-proto` 的 proto 生成并提交，**不需要装 protoc** |
| ③ 脚手架 | `templates/plugin` + `scripts/scaffold.sh` | 生成可运行的插件工程 |
| ④ 一致性套件 | `src/conformance.rs` | 插件接入前必须跑通的 **L1** 检查，跑的就是中台注册期的规则 |
| ⑤ 载荷与信封 | `src/envelope.rs` | `google.protobuf.Struct` ↔ `serde_json::Value`、deadline / budget、校验响应 |
| ⑥ 跨语言规则 | `src/rules.rs` | 插件名与状态键的判定，与中台、Go SDK 共用同一份契约文件 |
| ⑦ 外置状态 | `src/state.rs`（`hubkit::state`） | **HubState** 客户端：跨调用要保留的东西放这里，凭证由骨架注入，401 自动换新的 |
| ⑧ 发现与互调 | `src/gateway.rs`（`hubkit::gateway`） | **PluginGateway** 客户端：插件清单 / 消息端点 / 契约查询 + 同步互调 `invoke_plugin`，凭证与外置状态同一份 |

## 快速开始

```bash
# 生成一个可运行的插件工程（本脚本只是本地验证用；
# 中台控制台走的是 crates/hub-templates 的渲染器，规则相同）
sdk/rust/scripts/scaffold.sh order-reader /tmp/order-reader

cd /tmp/order-reader
cargo test     # L1 契约自洽（含中台注册期的同一套规则）
cargo run      # 起来后自己向 HUB_ADDR 注册，并按中台指定的周期发心跳
```

生成的插件**零 proto 工具链依赖**：它用 `google.protobuf.Struct` 承载 JSON 载荷，
也就是「直接调用」那条路（agent 经 MCP、外部系统经 HTTP）。需要与其他插件在 flow 里
传递类型化消息时，再加自己的 `.proto`。

## 最小插件

```rust,no_run
use hubkit::{Envelope, PluginError, PluginManifest, ValidateResponse};

struct MyPlugin;

#[hubkit::async_trait]
impl hubkit::Plugin for MyPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            name: "order-reader".to_string(),
            version: "0.1.0".to_string(),
            consumes: vec![hubkit::proto::MessageContract {
                fq_name: hubkit::envelope::STRUCT_FQ_NAME.to_string(),
                description: String::new(),
            }],
            ..Default::default()
        }
    }

    // descriptor：只用 Struct 载荷的插件没有自己的 proto，保持默认的返回空即可

    async fn validate(&self, envelope: &Envelope) -> Result<ValidateResponse, PluginError> {
        let Some(payload) = hubkit::envelope::payload_json(envelope) else {
            return Ok(hubkit::envelope::invalid(vec![hubkit::envelope::issue(
                "payload", "需要 JSON 对象载荷",
            )]));
        };
        if payload.get("orderId").and_then(|v| v.as_str()).is_none() {
            return Ok(hubkit::envelope::invalid(vec![hubkit::envelope::issue(
                "payload.orderId", "缺少必填字段 orderId",
            )]));
        }
        Ok(hubkit::envelope::valid())
    }

    async fn handle(&self, envelope: Envelope) -> Result<Envelope, PluginError> {
        let payload = hubkit::envelope::payload_json(&envelope).unwrap_or_default();
        Ok(hubkit::envelope::with_payload_json(
            &envelope,
            serde_json::json!({ "orderId": payload.get("orderId") }),
        )?)
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = hubkit::run(MyPlugin, hubkit::Config::from_env()).await {
        eprintln!("插件启动失败：{e}");
        std::process::exit(1);
    }
}
```

## 环境变量

| 变量 | 必填 | 说明 |
|---|---|---|
| `HUB_ADDR` | 是 | 中台插件面地址（gRPC）。本地 `http://127.0.0.1:8093` |
| `HUB_ADVERTISE_ADDR` | 是 | **中台能拨通**的本插件地址 |
| `HUB_LISTEN_ADDR` | 否 | 本插件监听地址，缺省 `:9000` |
| `HUB_INSTANCE_ID` | 否 | 实例标识，缺省「主机名-PID」 |
| `HUB_LOG_LEVEL` | 否 | `debug` / `info` / `warn` / `error`，缺省 `info` |

状态调用的时间上限（缺省 2s）**不在环境变量里**，它是 `Config::state_call_timeout`：
它不该是运维在部署时随手改的旋钮，改大只会让一次中台抖动多吃掉一些业务预算。

## 外置状态（HubState）：跨调用要保留的东西放这里

插件被**强制无状态**：实例内存不保证跨调用保留。`hubkit::state` 就是中台那份
KV（`crates/hub-proto/proto/hub/v1/state.proto`）的客户端。

```rust,ignore
use std::sync::{Arc, Mutex};
use hubkit::{Plugin, StateClient};

struct MyPlugin {
    // 骨架从注册循环那个任务调用 set_state，而 handle 跑在别的任务上——
    // 同步是实现方的责任
    state: Mutex<Option<Arc<StateClient>>>,
}

#[hubkit::async_trait]
impl Plugin for MyPlugin {
    // …manifest / validate / handle 照旧…

    async fn handle(&self, env: hubkit::Envelope) -> Result<hubkit::Envelope, hubkit::PluginError> {
        let state = self.state.lock().unwrap().clone();
        if let Some(state) = state {
            // 0 表示不过期；不足 1 秒的 TTL 会被截断成 0（线协议是秒）
            state.put("session", "token", b"abc", std::time::Duration::ZERO).await?;
            let cached = state.get("session", "token").await?;   // Ok(None) = 键不存在
        }
        Ok(env)
    }

    fn set_state(&self, state: Arc<StateClient>) {
        *self.state.lock().unwrap() = Some(state);
    }
}
```

几条会踩到的：

- **不要自己管凭证**。它是中台在注册回执里下发的，SDK 拿不到第二条路；
  骨架在**每次注册成功后**把它交给状态客户端，并调用 `Plugin::set_state`。
- **键前缀不含版本号**。中台按凭证反查插件名，把键拼成
  `hub:state:{插件名}:{namespace}:{key}`——同一插件的所有版本**共用一个状态空间**，
  升版本不会清空状态（对登录缓存这类状态正是要的），但两个版本写同一个 namespace
  就是互相覆盖。要按版本隔离，自己把版本写进 namespace。
- **这不是插件间的隔离承诺**。凭证只证明「注册方自称是某个插件名并通过了该名字的
  校验」，不证明它就是那个插件：任何能连上插件面的进程都能注册一个契约兼容的新版本、
  拿到反查为该名字的凭证。需要真正的隔离时别指望这一层。
- **namespace / key / prefix 只允许 `[A-Za-z0-9_.-]`，非空、最长 200 字节**。
  放行 `*` 是漏洞不是功能：前缀靠字符串拼接，通配符会让 `Scan` 变成跨命名空间的
  模式匹配。判定与中台共用一份契约文件（`sdk/go/hubkit/testdata/hub-rules.json`），
  本地先拦一道，省一次网络往返。
- **上限本地也拦**：单值 1 MiB、一次 `Scan` 最多 1000 条（`hubkit::MAX_VALUE_BYTES` /
  `MAX_SCAN_LIMIT`，取值来自中台实现）。`scan` 的 limit 必须显式给且在 `1..=1000`，
  没有「不填就是全扫」。
- **401 会自愈**。凭证随每次注册轮换（中台重启、实例被摘除后自愈都会重新注册）。
  状态调用撞上 `UNAUTHENTICATED` 时，客户端会**叫醒注册循环**去换一张新的，
  但这一次调用仍以错误返回——重注册是后台的补救，不该掩盖这一次失败。
  重注册有速率下限（复用 `register_retry_interval` 作冷却窗口），
  免得持续被拒时把注册打成自旋。
- **`publish`（触发下游 flow）也在这里**。受理了返回执行 id（`run_id`）；
  被防环、链深或配额拦下时 `reason` 随 [`StateError::Rejected`] 带给调用方——
  它是**业务结果**，重试同一条多半还是被拒。它拿不到下游处理结果，
  那种场景走网关的 `invoke_plugin`（见下一节）。

## 插件发现与互调（PluginGateway）：找得到谁、调得动谁

`hubkit::gateway` 是插件清单 / 消息端点 / 契约查询与**同步互调**的客户端
（`crates/hub-proto/proto/hub/v1/gateway.proto`）。插件之间不直连：直连会绕过
中台的治理、审计、熔断与身份体系，互调一律 `A→hub→B` 由中台代调。

```rust,ignore
use std::sync::{Arc, Mutex};
use hubkit::{GatewayClient, InvokeOptions, Plugin};

struct MyPlugin {
    gateway: Mutex<Option<Arc<GatewayClient>>>,
}

#[hubkit::async_trait]
impl Plugin for MyPlugin {
    // …manifest / validate 照旧…

    async fn handle(&self, env: hubkit::Envelope) -> Result<hubkit::Envelope, hubkit::PluginError> {
        let gateway = self.gateway.lock().unwrap().clone();
        if let Some(gateway) = gateway {
            // 传当前信封：trace 贯通、互调链防环生效。
            // 链是「原样复制」的——不要自己往链上追加自己，中台负责追加 caller。
            let out = gateway
                .invoke_plugin(
                    "order-pricer",
                    serde_json::json!({ "sku": "A-1" }),
                    InvokeOptions { current_envelope: Some(&env), ..Default::default() },
                )
                .await?;   // GatewayError 直接 `?` 成 PluginError
            let _ = hubkit::envelope::payload_json(&out);
        }
        Ok(env)
    }

    fn set_gateway(&self, gateway: Arc<GatewayClient>) {
        *self.gateway.lock().unwrap() = Some(gateway);
    }
}
```

几条会踩到的：

- **业务结果不是 gRPC 错误**。下游拒绝（`GatewayError::Rejected`，带结构化
  `issues`）与中台裁决（`GatewayError::Error`，带 `reason`：未授权 / 超限 / 成环）
  走类型化错误；只有基础设施故障才是 `GatewayError::Rpc`。`match` 变体就能决定
  「改数据重试」还是「退避」。
- **`version` / `timeout_ms` 不给就是「最新版 / 中台兜底（30s）」**。多版本共存时
  锁版本更稳；`timeout_ms` 会被中台夹进信封 deadline（
  `min(传入 deadline, now + timeout_ms)`）。
- **发现调用短超时**（缺省 2s，`Config::gateway_call_timeout`）：清单 / 契约是
  基础设施查询，卡住了该早点放弃。互调不吃这个上限——它的等待跟随调用方声明的预算。
- **权限要声明**。中台的 `HUB_PLUGIN_CALL_POLICY=declared` 时，只有你 manifest 的
  `invokes` 列表里声明过的插件才调得动（`PluginManifest.invokes`）。

## proto 产物为什么是**提交进仓库**的

`src/proto/hub.v1.rs`（约 1500 行）由 `protogen/` 从 `crates/hub-proto/proto` 生成后
提交，**构建期不生成、也没有 `build.rs`**。理由：

1. **离线优先**。本仓库的取向是离线可构建（根目录有 400MB 的 `vendor/` 快照、
   `cargo build --offline` 要能用）。构建期生成就要求每个构建环境都能跑到
   `tonic-prost-build` 与一个能用的 protoc，而 `protoc-bin-vendored` 又只覆盖部分
   平台——离线的交叉构建会直接卡死在这里。
2. **与 Go 侧同一个选择**（`sdk/go/proto/hubv1/` 也是提交的产物），两边一致，
   加第三、第四门语言时不必为每一门单独论证一遍。
3. **插件团队不需要装 protoc**。SDK 是随脚手架包发出去的，拿到工程的人不该为了
   编译一个 SDK 先在自己机器上装一套 protobuf 工具链。

代价与它的对策：

- **换 prost / tonic 大版本后必须重新生成**：
  ```console
  cargo run --manifest-path sdk/rust/protogen/Cargo.toml
  ```
  生成器自己带 `protoc-bin-vendored`，维护者不必装 protoc。
- **产物是死代码，不会随 proto 自动更新**。所以 `protogen/src/main.rs` 只读
  `crates/hub-proto/proto`，**不在这里放第二份 .proto**——契约的唯一来源是中台仓库
  那一份，抄一份过来两份迟早分叉。

`protogen/` 与 `templates/`、`scripts/` **不随包下发**（随包下发的清单由中台侧的
`crates/hub-templates/build.rs` 决定，`scripts/scaffold.sh` 里的排除项与它一致）。

## 本 SDK 的测试

```bash
cargo test          # 单元 + 集成 + 文档测试
cargo clippy --all-targets
```

集成测试自己伪造一个中台（`tests/support/mod.rs`），验的是**中台那边看到了什么**
（它还维护一张最小的实例表，所以「注销到底删掉了谁的行」也验得到）：

- `tests/registration.rs` —— 自注册、按中台指定的周期发心跳、中台不在线时一直重试、
  被拒时把每条原因打出来、退出时主动注销
- `tests/unregister.rs` —— 注销要凭注册时下发的凭证认属主：带上凭证才注销得掉、
  注册从未成功过（或中台没下发凭证）时**根本不发**注销请求、中台侧凭证不符时一行都不删。
  `instance_id` 由插件自报、可跨插件相撞，而**先退出的一方原本会删掉对方那一行**
  （见 `crates/hub-registry/src/lib.rs` 的 `unregister`），这三条分别钉住链路的三段
- `tests/self_heal.rs` —— 心跳回执带 `reregister_required` / `accepted=false` 时重新注册，
  以及「反复被要求重注册也不会把注册打成自旋」
- `tests/plugin_service.rs` —— 直连插件打 `PluginRuntime` 的五个方法（L2 那一层）
- `tests/rules.rs` —— 读 Go 侧那份 `hub-rules.json`，验跨语言规则一致
- `tests/state.rs` —— 外置状态：注册成功后凭证被注入、读写删扫都带上了它、
  401 触发重新注册并换成新凭证、冷却窗口内的 401 不会把注册打成自旋。
  这个伪造中台把注册面与状态面挂在同一个端口上（与真中台一样），
  并**记下每一次请求带上来的是什么凭证**——只验「插件侧返回值对不对」的话，
  「凭证其实没发出去」这种错会漏过去
- `tests/gateway.rs` —— 网关面：发现三件套带凭证、互调信封的复制规则
  （trace 贯通、链原样复制不追加自己、新 ULID 幂等键、deadline 夹紧）、
  REJECTED / ERROR 映射成类型化错误、`publish` 的受理与被拒、
  网关调用撞 401 与状态调用走同一条自愈路径

**真中台**的验收（L3）分两条路：

用 `hub-mock`（本地替身）：

```bash
cargo build -p hub-mock && ./target/debug/hub-mock          # 8092 HTTP / 8093 gRPC
HUB_ADDR=http://127.0.0.1:8093 \
HUB_ADVERTISE_ADDR=http://127.0.0.1:19100 \
HUB_LISTEN_ADDR=127.0.0.1:19100 cargo run
curl -s http://127.0.0.1:8092/plugins                        # 看它有没有在册
```

对着**一整套真中台**（有真 PG + Redis 的那个）跑状态往返：

```bash
HUBKIT_LIVE_HUB_ADDR=http://127.0.0.1:8093 \
  cargo test --offline --test state_live -- --nocapture
```

没设这个环境变量时 `tests/state_live.rs` 会**跳过**（不是失败）——真中台不是每条
流水线上都有。它验的是 mock 验不了的东西：中台真的会查库把凭证反查成插件名、
强制拼上前缀，以及本地拦下的规则与中台的规则**不冲突**。

## 与 Go 侧的差异

功能对齐，但有几处**有意**的不同，写在这里免得被当成遗漏：

- **日志时间是 UTC**（带 `Z`），Go 侧 slog 打的是本地时间。日志要的是可比较。
- **注入状态的形态是带默认实现的钩子**（`Plugin::set_state(Arc<StateClient>)`），
  Go 侧是一个可选接口 `StateAware`。Rust 没有稳定的特化，「泛型 P 是否实现了某个
  trait」在语言层面问不出来——可选接口要靠不稳定的特化或 downcast 技巧，
  对一个发给插件团队的 SDK 来说过头了。钩子达成的是同一件事：不用状态的老插件
  一行都不用改，而且少一个「实现了却没接上」的坑。
- **状态调用超时不给「不限时」**。Go 的 `StateCallTimeout <= 0` 在客户端层面表示
  不设上限（手工构造的客户端走这条）；Rust 侧 0 一律补成缺省 2s。
  一次没有上限的状态调用会把中台的卡顿全吃进调用方的预算里。
- **`StateClient` 的 `Debug` 不打印凭证**（打 `<已隐藏>`）：`Debug` 是最容易漏的一条
  泄露路径——`tracing::debug!(?client)`、断言失败时的 `{:?}` 都会带上它。
- **心跳周期可被打断**。Go 侧的 `time.NewTicker` 与 ctx 竞争，Rust 侧用 `watch`
  做水平触发的停止信号——「信号先到、等待后开始」这种丢信号的窗口被一条单测钉住了。
  状态凭证被拒的通知走同一条等待（`tokio::select!` 三路：心跳到点 / 停止信号 / 401），
  否则一次 401 最多要等满一个心跳周期才被处理。
- **`HandleStream` 有实现**（把 `handle` 的单条结果当成一条流），Go 侧返回 `Unimplemented`。
