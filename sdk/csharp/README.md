# HubKit —— anc-hub 插件 SDK（C# / .NET）

anc-hub（控制中台）的插件侧服务端骨架。插件作者只需要实现 `IPlugin`，其余全部由骨架处理：
起 gRPC 服务、向中台自注册、按中台指定的周期发心跳、被摘除后自动重新注册、优雅退出时注销。

它是 `sdk/go` 的 C# 等价物：同一份 nginx / TLS / 注册 / 心跳约定，同一套日志形状，
同一份跨语言规则文件。

## 目录

```
HubKit/              SDK 库（命名空间 HubKit）
  Config.cs          HubConfig：环境变量 → 配置
  Log.cs             HubLogger：一条记录一行 JSON，打到 stderr
  Plugin.cs          IPlugin / Descriptors / RejectCodes
  Envelope.cs        Envelopes：信封与 JSON 载荷的读写（PayloadJson / WithPayloadJson / Budget / Valid / Invalid）
  Rules.cs           跨语言规则（插件名、HubState 段标识）
  State.cs           HubState 客户端（StateClient / IStateAware）、Publish 封装与本地参数校验
  Gateway.cs         插件间发现与互调（GatewayClient / IGatewayAware / InvokePluginAsync）
  Conformance.cs     L1 自检（Conformance.Local）与 L2 运行时自检（Conformance.RuntimeAsync）
  Registrar.cs       注册 → 心跳 → 被摘除则重新注册 → 凭证被拒则重新注册
  PluginHost.cs      入口：PluginHost.RunAsync / PluginChannel / PluginRuntimeService
  protos/            契约 proto 的**拷贝**（见下）
  testdata/          跨语言规则文件的**拷贝**（见下）
HubKit.Tests/        SDK 自己的测试（146 条，其中状态集成测试打真中台、中台不在时跳过）
HubProbe/            调试工具 hubprobe：conform / health
templates/plugin/    脚手架模板（@@占位符@@ 在生成时替换）
sync-contract.sh     从仓库同步 proto 与规则文件
```

## 最小插件

```csharp
using Hub.V1;
using HubKit;

public sealed class MyPlugin : IPlugin
{
    public PluginManifest Manifest => new()
    {
        Name = "my-plugin",
        Version = "0.1.0",
        Consumes = { new MessageContract { FqName = Envelopes.StructFqName } },
    };

    public byte[] Descriptor => Descriptors.Of();   // 只用 Struct 载荷时没有自己的 proto

    public Task<ValidateResponse> ValidateAsync(Envelope env, CancellationToken ct) =>
        Task.FromResult(Envelopes.Valid());

    public Task<Envelope> HandleAsync(Envelope env, CancellationToken ct) =>
        Task.FromResult(Envelopes.WithPayloadJson(env, new Dictionary<string, object?> { ["ok"] = true }));
}

return await PluginHost.RunAsync(new MyPlugin(), HubConfig.FromEnv());
```

配置**全部**来自环境变量（`HUB_ADDR` 必填、`HUB_ADVERTISE_ADDR` 必填、`HUB_LISTEN_ADDR`
缺省 `:9000`、`HUB_INSTANCE_ID` 缺省「主机名-PID」、`HUB_LOG_LEVEL`、`HUB_TLS_MAX_VERSION`），
字段名与 Go 侧逐一对应。

## 四个刻意的行为

- **注册会一直重试**：中台可能比插件晚起来，插件先启动是常态（`HUB_ADDR` 连不上时每 5 秒一次）。
- **被摘除后自动重新注册**：心跳响应里带 `reregister_required` 时重走注册流程。
  这是实例掉线后能自愈的关键。它有单测覆盖（`RegistrarTests.被摘除后心跳被判要求重注册时会自己重新注册`）。
- **心跳失败不停止心跳**：网络抖一下就让插件哑掉，比不心跳更糟。
- **状态凭证失效后自动重新注册**：HubState 调用收到 `Unauthenticated` 时重走注册流程换新凭证
  （中台重启会轮换凭证、实例被摘除后旧凭证即失效）。这是防御层而不是主恢复路径——
  凭证落了库，中台重启对插件本来是透明的。见下面的「跨调用保留状态」。

## 跨调用保留状态（HubState）

插件被**强制无状态**：实例内存不保证跨调用保留，需要跨调用保留的东西（登录缓存、
去重标记、游标……）走中台的 HubState。插件实现 `IStateAware`，宿主在**注册成功后**
把客户端注入给你：

```csharp
public sealed class MyPlugin : IPlugin, IStateAware
{
    // 注入来自注册循环那个任务，而 handler 跑在别的任务上——同步是**实现方**的责任。
    // 凭证随每次注册轮换，所以注入的永远是同一个客户端对象、最新的凭证；
    // 不要把它长期拷进别处，也不该缓存凭证。
    private volatile StateClient? _state;

    public void SetState(StateClient state) => _state = state;

    public async Task<Envelope> HandleAsync(Envelope env, CancellationToken ct)
    {
        var state = _state ?? throw new InvalidOperationException("状态客户端还没注入（注册未成功？）");

        await state.PutAsync("login", "user-42", cacheBytes, TimeSpan.FromMinutes(10), ct);

        var (value, found) = await state.GetAsync("login", "user-42", ct);
        var entries = await state.ScanAsync("login", "user-", limit: 100, ct);
        var removed = await state.DeleteAsync("login", "user-42", ct);
        ...
    }
}
```

要点（都有测试钉住，逐条对齐 Go 侧 `sdk/go/hubkit/state.go`）：

| 事项 | 说明 |
|---|---|
| **凭证从哪来** | 注册回执 `RegisterResponse.state_token`，宿主读出来交给状态客户端；插件作者**不该**自己管 token |
| **凭证怎么传** | 每个请求带在 gRPC metadata 的 `x-hub-state-token` 上（`StateClient.TokenMetadata`，与 `hub-rules.json` 的 `stateTokenMetadata` 同一份事实来源） |
| **身份与前缀** | 中台按凭证反查插件名并强制加前缀 `hub:state:{插件名}:`，插件自报的 namespace 只是子空间。前缀由中台拼，插件不用拼——也拼不出别人的 |
| **版本不隔离** | 前缀里**没有版本号**：同一插件的所有版本共用一个状态空间。升版本不会清空状态（登录缓存正是要这样），但两个版本往同一个 namespace 写就是互相覆盖。要按版本隔离请自己把版本写进 namespace |
| **TTL** | 粒度是**秒**（线协议 `ttl_seconds`），`TimeSpan.Zero` 表示不过期；不足 1 秒会被**截断为 0**（= 永不过期），想让键很快消失请传 `>= 1s` |
| **上限** | 单值 1MB、一次 Scan 最多 1000 条（`StateClient.MaxValueBytes` / `MaxScanLimit`，取自中台实现）。**本地先拦一道**：这两种错在本地就以 `HubStateException` 抛出，错误里写清上限与实收，比中台那句 `InvalidArgument` 好懂，也省一次注定失败的往返 |
| **失败怎么处理** | 中台判定的错误（凭证无效、后端故障）原样抛 `RpcException`，本地能判定的参数错误抛 `HubStateException`。插件通常该 fail-open（比如拿不到缓存就回源）——重注册是后台的补救，不该掩盖这一次失败 |
| **超时** | 单次调用有上限（`HubConfig.StateCallTimeout`，缺省 2s），走 gRPC 的 deadline，因此超时是 `DeadlineExceeded`；调用方自己的 `CancellationToken` 取消则是 `Cancelled`，两者不会互相化装。它是**上限而非承诺**：调用方预算更早就按调用方的来 |

**Publish（异步触发 flow）**也在 `StateClient` 上：`PublishAsync(target, envelope)` 返回中台分配的
run_id；未受理（防环、目标 flow 不存在……）抛 `PublishRejectedException`，reason 随异常携带——
那是**业务结果**不是网络故障，该改逻辑或换目标，而不是退避重试。

需要下游**处理结果**的同步场景，走下面的「插件互调与发现」。

## 插件互调与发现（PluginGateway）

插件之间不直连：直连会绕过中台的治理、审计、熔断与身份体系。同步互调一律经中台代调
（A→hub→B），发现能力（谁在线、谁生产/消费什么消息、契约长什么样）也只从中台取。
插件实现 `IGatewayAware`，宿主在**注册成功后**注入 `GatewayClient`（凭证就是状态凭证，
同一张 `x-hub-state-token`，随每次注册轮换）：

```csharp
public sealed class MyPlugin : IPlugin, IGatewayAware
{
    private volatile GatewayClient? _gateway;

    public void SetGateway(GatewayClient gateway) => _gateway = gateway;

    public async Task<Envelope> HandleAsync(Envelope env, CancellationToken ct)
    {
        var gateway = _gateway ?? throw new InvalidOperationException("网关客户端还没注入（注册未成功？）");

        // 发现：能调谁 / 这个消息谁生产谁消费 / 它吃什么吐什么
        var plugins = await gateway.ListPluginsAsync(cancellationToken: ct);
        var endpoints = await gateway.DescribeMessageAsync("wms.v1.OrderCreated", ct);
        var contract = await gateway.GetContractAsync("other-plugin", fqName: "wms.v1.OrderCreated", ct);

        // 互调：传入**当前信封**，trace 才能贯通、链才能防环
        var output = await gateway.InvokePluginAsync(
            "other-plugin",
            new InvokeOptions
            {
                PayloadJson = new Dictionary<string, object?> { ["text"] = "hi" },
                Timeout = TimeSpan.FromSeconds(5),
                CurrentEnvelope = env,
            },
            ct);
        var payload = Envelopes.PayloadJson(output);
        ...
    }
}
```

结果语义（与中台的 `InvokeOutcome` 对应）：

| outcome | 表现 | 调用方该做什么 |
|---|---|---|
| `HANDLED` | 返回下游的信封 | 取载荷继续 |
| `REJECTED` | 抛 `InvokeRejectedException`，issues 随异常携带（path 定位到字段） | 照 issues 改数据重试 |
| `ERROR` | 抛 `InvokeFailedException`，reason 随异常携带 | 按 reason 分流：未授权/成环改逻辑，「没有健康实例」可退避重试 |
| 基础设施故障 | 原样抛 `RpcException`（未鉴权、中台不可达） | 与 HubState 的错误同一套处置 |

要点：

| 事项 | 说明 |
|---|---|
| **链怎么传** | `Envelope.meta["hub.call_chain"]` 记录「已处理过该消息的插件序列」。`InvokePluginAsync` 传入 `CurrentEnvelope` 时把它**原样**带上——**不追加自己**，追加 caller 是中台代调时的事，SDK 抢着做会让链上出现重复节点 |
| **防环** | 中台据链拒绝互调成环（`ERROR: 检测到互调环…`）与链深超限（上限 `GatewayClient.MaxInvokeDepth = 8`）。SDK 不复算这些——插件侧自己再算一遍等于跟中台抢职责 |
| **超时** | 互调**不吃** `HubConfig.StateCallTimeout`：预算就是信封的 deadline（`InvokeOptions.Timeout` 会写进信封 deadline 与 `timeout_ms`，中台再夹一次）。发现三件套（List/Describe/GetContract）则与状态调用一样受上限约束 |
| **身份** | subject 由中台无条件覆盖为调用方，信封里自报的 subject 不可信 |
| **凭证被拒** | 网关调用撞 401 与状态调用撞 401 共享同一个信号，都会叫醒注册循环换新凭证 |

## 命令

```bash
dotnet test                                                    # 跑 SDK 的测试（目录内任意位置；HubKit.slnx 含三个工程）
dotnet test --project HubKit.Tests                             # 等价形式
dotnet build                                                   # 编译 SDK
dotnet run --project HubProbe -- conform http://127.0.0.1:9000   # 对着运行中的插件自检
```

`.NET 10` 上 `dotnet test` 的两种体验（VSTest 通道 / MTP 模式）对**调用形式**的支持不同：
带 `--project` / `--solution` 的形式处处可用；`dotnet test <目录>` 这种位置参数形式只有在
VSTest 通道下才被接受。本目录两头都顾到了：测试栈用 xunit.v3 **3.2.2**（mtp-v1，还保留
VSTest 通道的重定向，见 `HubKit.Tests.csproj` 注释），`global.json` 又配了 MTP runner
（让目录内的 bare 调用走新体验）。**别把 xunit 升到 4.x**——那会钉死 MTP v2，位置参数形式从此无解。

## 契约素材是拷贝，漂移由测试守

`HubKit/protos/hub/v1/*.proto` 与 `HubKit/testdata/hub-rules.json` 是
`crates/hub-proto` 与 `sdk/go` 里那两份的**拷贝**。

为什么拷贝：SDK 会随模板包一起下发给插件团队，下载方手里**没有中台仓库**，
引用仓库内的相对路径在他们那儿就是断的。Go 侧的做法是把生成的 `.pb.go` 提交进
`sdk/go/proto/`，这里等价地把 `.proto` 提交进来——C# 侧的代码由 Grpc.Tools
在构建时生成，所以随包发的是**源**而不是产物。

拷贝会漂移，所以 `HubKit.Tests/ContractSyncTests.cs` 逐字节比对两边。
改了契约忘了重跑 `./sync-contract.sh`，那条测试会红。在下载包里（找不到中台仓库）
它会**跳过**，而不是判红——原件不在手边，那里无从比对。

## 已知要求与限制

| 项 | 说明 |
|---|---|
| .NET | `net10.0`（当前机器上是 SDK 10.0.105 / 运行时 10.0.5） |
| NuGet | 首次 restore 需要能访问 NuGet 源；之后走 `~/.nuget/packages` 缓存 |
| macOS(Apple Silicon) | Grpc.Tools 只提供 **x86_64** 的 macOS 版 protoc，没装 Rosetta 时构建会以 `error MSB6003: ... Bad CPU type in executable` 失败。装一次即可：`softwareupdate --install-rosetta --agree-to-license`。Linux / Windows / x64 macOS 无此问题 |
| Web 框架 | SDK 用 Kestrel 托 gRPC，随包工程的运行镜像是 **`dotnet/aspnet`** 而不是 `dotnet/runtime` |

### 与 Go 侧的差异

- **多了本地参数校验**：Go 的客户端把「值超过 1MB / Scan 的 limit 越界」交给中台去判（回 `InvalidArgument`），
  C# 侧在发请求前就拦下并抛 `HubStateException`，错误里带上限与实收。判定与中台一致，动作不同。
- **没有 mockhub**：本地替身用中台仓库里的 `cargo run -p hub-mock`（判据与真中台同源）。
  注意 mockhub 目前**还没有 HubState**，所以状态这块的本地验证走 `StateIntegrationTests` 打真中台
  （`HUB_STATE_IT_ADDR`，缺省 `http://127.0.0.1:8093`；连不上时**跳过**而不是判红）。
- **hubprobe 只有 `conform` 与 `health`，没有 `e2e`**：第四关（经中台 HTTP 面真实调用一次）
  要打中台的 ingress；mock 中台没有 ingress，所以那个子命令在这里也没法本地验证。
