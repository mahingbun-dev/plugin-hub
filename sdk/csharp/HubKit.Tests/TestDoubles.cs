using Grpc.Core;
using Hub.V1;
using HubKit;

namespace HubKit.Tests;

/// <summary>
/// 测试用的插件替身：manifest 合法、Descriptor 为空（只用 well-known 载荷）、
/// Validate / Handle 都做最朴素的事。
///
/// 没直接拿模板生成的插件来测：那是**另一个工程**的产物，SDK 的测试不该依赖它
/// ——它一旦没生成出来，SDK 自己的测试就跟着红了，分不清是谁坏了。
///
/// 刻意**不实现** <see cref="IStateAware"/>：想验注入的用例请用
/// <see cref="StateAwarePlugin"/>，那样「实现了才注入」这件事也顺带钉住了。
/// </summary>
internal class FakePlugin : IPlugin
{
    public string Name { get; init; } = "test-plugin";

    public string Version { get; init; } = "0.1.0";

    public byte[] Descriptor { get; init; } = [];

    public List<string> Consumes { get; init; } = [Envelopes.StructFqName];

    public List<string> Produces { get; init; } = [];

    public List<ToolDecl> Tools { get; init; } = [];

    public PluginManifest Manifest
    {
        get
        {
            var manifest = new PluginManifest
            {
                Name = Name,
                Version = Version,
                Description = "测试替身",
            };

            // 用 AddRange 而不是对象初始化器里的 `Consumes = { ... }`：
            // 后者每次只能塞一个元素，塞不进一个 IEnumerable
            manifest.Consumes.AddRange(Consumes.Select(fq => new MessageContract { FqName = fq }));
            manifest.Produces.AddRange(Produces.Select(fq => new MessageContract { FqName = fq }));
            manifest.Tools.AddRange(Tools);
            return manifest;
        }
    }

    public Task<ValidateResponse> ValidateAsync(Envelope envelope, CancellationToken cancellationToken) =>
        Task.FromResult(Envelopes.Valid());

    public Task<Envelope> HandleAsync(Envelope envelope, CancellationToken cancellationToken) =>
        Task.FromResult(Envelopes.WithPayloadJson(envelope, new Dictionary<string, object?> { ["ok"] = true }));
}

/// <summary>
/// 中台那条链路的替身。
///
/// 三个响应各自出现的时机由构造参数决定——「被摘除 → 重新注册」这条路径
/// 只有让心跳回一次 <c>reregister_required</c> 才走得到，而真中台不会凭空回它。
/// </summary>
internal sealed class FakeRegistryChannel : IRegistryChannel
{
    private readonly object _gate = new();

    /// <summary>中台侧此刻的实例表：instance_id → 这一行的属主凭证。</summary>
    private readonly Dictionary<string, string> _rows = [];

    /// <summary>回绝注册时用的拒绝原因；为空表示接受。</summary>
    public List<Rejection> RejectRegister { get; init; } = [];

    /// <summary>
    /// 每次注册下发的状态凭证（第 n 次注册取第 n 项）。为空时一律下发
    /// <c>"test-state-token"</c>——留一个非空缺省值，是因为真中台正常情况下都会下发，
    /// 大多数用例不该被「没凭证」这件事干扰。
    ///
    /// 列表里的项可以是空串：那就是「中台没下发凭证」这条路径。
    /// </summary>
    public List<string> StateTokens { get; init; } = [];

    /// <summary>第几次心跳开始回 <c>reregister_required</c>；0 表示从不。</summary>
    public int ReregisterAtHeartbeat { get; init; }

    /// <summary>第几次心跳开始抛异常（模拟网络抖动）；0 表示从不。</summary>
    public int FailAtHeartbeat { get; init; }

    /// <summary>true 时注销抛异常（模拟中台已挂）。注册照常——注销失败那条路径要先注册成功过。</summary>
    public bool FailUnregister { get; init; }

    public int RegisterCalls { get; private set; }

    public int HeartbeatCalls { get; private set; }

    public int UnregisterCalls { get; private set; }

    /// <summary>
    /// 收到的注销请求（整个请求，不只是 instance_id）——注销还要看凭证，
    /// 只看 id 的话「带没带凭证、带的是哪张」在测试里就看不见了。
    /// </summary>
    public List<UnregisterRequest> Unregisters { get; } = [];

    /// <summary>收到的注销请求里携带的凭证，与 <see cref="Unregisters"/> 同序。</summary>
    public IReadOnlyList<string> UnregisterTokens
    {
        get
        {
            lock (_gate)
            {
                return Unregisters.Select(r => r.StateToken).ToList();
            }
        }
    }

    /// <summary>
    /// 某个实例此刻在不在注册表里。**注销成功与否的唯一观察点**：
    /// <c>UnregisterResponse</c> 是空的，真中台也只把结果写进日志，所以替身同样只让
    /// 「行还在不在」说话（见 <see cref="UnregisterAsync"/>）。
    /// </summary>
    public bool IsRegistered(string instanceId)
    {
        lock (_gate)
        {
            return _rows.ContainsKey(instanceId);
        }
    }

    /// <summary>此刻注册表里的实例 id。测试结束时用它确认「一行都没被误删」。</summary>
    public IReadOnlyList<string> RegisteredInstances
    {
        get
        {
            lock (_gate)
            {
                return _rows.Keys.OrderBy(k => k, StringComparer.Ordinal).ToList();
            }
        }
    }

    public Task<RegisterResponse> RegisterAsync(RegisterRequest request, CancellationToken cancellationToken)
    {
        int n;
        lock (_gate)
        {
            RegisterCalls++;
            n = RegisterCalls;
        }

        if (RejectRegister.Count > 0)
        {
            var rejected = new RegisterResponse { Accepted = false, InstanceId = request.InstanceId };
            rejected.Rejections.AddRange(RejectRegister);
            return Task.FromResult(rejected);
        }

        var token = StateTokens.Count == 0
            ? "test-state-token"
            : StateTokens[Math.Min(n, StateTokens.Count) - 1];

        lock (_gate)
        {
            // 与真中台一致：重新注册会**轮换**这一行的属主凭证（旧凭证随之作废），
            // 而 instance_id 撞在一起的另一个插件注册时覆盖的也正是这一行。
            _rows[request.InstanceId] = token;
        }

        return Task.FromResult(new RegisterResponse
        {
            Accepted = true,
            InstanceId = request.InstanceId,
            // 1 秒是协议能表达的最小周期（int32 秒）。测试不必真等它——
            // HubConfig.HeartbeatIntervalOverride 会把它压到毫秒级
            HeartbeatIntervalSeconds = 1,
            StateToken = token,
        });
    }

    public Task<HeartbeatResponse> HeartbeatAsync(HeartbeatRequest request, CancellationToken cancellationToken)
    {
        int n;
        lock (_gate)
        {
            n = ++HeartbeatCalls;
        }

        // 只让**那一拍**失败：写成 n >= FailAtHeartbeat 的话，后面每一拍都会失败，
        // 就测不出「抖一下之后还能继续心跳」这件事了
        if (FailAtHeartbeat > 0 && n == FailAtHeartbeat)
        {
            throw new InvalidOperationException("模拟的网络抖动");
        }

        var reregister = ReregisterAtHeartbeat > 0 && n == ReregisterAtHeartbeat;
        return Task.FromResult(new HeartbeatResponse
        {
            Accepted = !reregister,
            HeartbeatIntervalSeconds = 1,
            ReregisterRequired = reregister,
        });
    }

    /// <summary>
    /// 注销：**照真中台的判定走**（<c>crates/hub-registry/src/lib.rs</c> 的 <c>unregister</c>）
    /// ——凭证为空、或与那一行的属主不符，一律拒绝且**不删任何行**。
    ///
    /// 替身必须真的校验，不能只记账：不校验的话，「插件发了一份不带凭证的注销」这条测试
    /// 会照样全绿，而这正是新行为要挡的那件事。
    /// </summary>
    public Task<UnregisterResponse> UnregisterAsync(UnregisterRequest request, CancellationToken cancellationToken)
    {
        lock (_gate)
        {
            UnregisterCalls++;

            // 记账放在抛异常之前：调用方确实发过这一发，测试要能看见它试过
            Unregisters.Add(request);

            // 凭证为空、行不在、或属主对不上，三者一样**不删任何行**
            // （真中台把判定与删除放在同一条 SQL 里，这里等价地放在同一把锁里）
            if (request.StateToken.Length > 0
                && _rows.TryGetValue(request.InstanceId, out var owner)
                && owner == request.StateToken)
            {
                _rows.Remove(request.InstanceId);
            }
        }

        if (FailUnregister)
        {
            throw new InvalidOperationException("模拟的中台故障");
        }

        return Task.FromResult(new UnregisterResponse());
    }
}

internal static class TestWait
{
    /// <summary>轮询等待一个条件成立。等到就返回 true，超时返回 false。</summary>
    public static async Task<bool> Until(Func<bool> condition, TimeSpan timeout)
    {
        var deadline = DateTime.UtcNow + timeout;
        while (DateTime.UtcNow < deadline)
        {
            if (condition())
            {
                return true;
            }

            await Task.Delay(5);
        }

        return condition();
    }

    /// <summary>异步版本：条件本身要打网络往返（比如「这个键过期了没」）时用它。</summary>
    public static async Task<bool> UntilAsync(Func<Task<bool>> condition, TimeSpan timeout)
    {
        var deadline = DateTime.UtcNow + timeout;
        while (DateTime.UtcNow < deadline)
        {
            if (await condition())
            {
                return true;
            }

            await Task.Delay(50);
        }

        return await condition();
    }
}

/// <summary>
/// 实现了 <see cref="IStateAware"/> 的插件替身。
///
/// 注入来自注册循环那个任务，测试在别的任务上读，所以这里用锁护住——
/// 裸字段在并发下就是数据竞争（真插件踩的是同一个坑，所以 <see cref="IStateAware"/>
/// 的文档里专门写了「同步是实现方的责任」）。
/// </summary>
internal sealed class StateAwarePlugin : FakePlugin, IStateAware
{
    private readonly object _gate = new();
    private StateClient? _state;

    /// <summary>被注入过几次。「每次重注册都重新注入」这件事靠它证明。</summary>
    public int Injected { get; private set; }

    public void SetState(StateClient state)
    {
        lock (_gate)
        {
            _state = state;
            Injected++;
        }
    }

    /// <summary>当前拿到的客户端；还没注入时是 null。</summary>
    public StateClient? State()
    {
        lock (_gate)
        {
            return _state;
        }
    }
}

/// <summary>
/// HubState 那条链路的替身。
///
/// 记下每次调用看到的**凭证**（从 <see cref="CallOptions.Headers"/> 里取，也就是
/// <see cref="StateClient.TokenMetadata"/> 那个键）：凭证是放对了地方、还是根本没带上，
/// 只有在这一层才看得出。
/// </summary>
internal sealed class FakeStateChannel : IStateChannel
{
    private readonly object _gate = new();

    /// <summary>每次调用看到的凭证；没带这个头时是 null。</summary>
    public List<string?> Tokens { get; } = [];

    public List<(string Namespace, string Key)> Gets { get; } = [];

    public List<(string Namespace, string Key, byte[] Value, long TtlSeconds)> Puts { get; } = [];

    public List<(string Namespace, string Key)> Deletes { get; } = [];

    public List<(string Namespace, string Prefix, uint Limit)> Scans { get; } = [];

    public List<string> Publishes { get; } = [];

    /// <summary>Publish 的受理结果与回执：rejected 时 <see cref="RejectReason"/> 随响应带回。</summary>
    public bool Accepted { get; set; } = true;

    public string RunId { get; set; } = "run-1";

    public string RejectReason { get; set; } = "中台侧的原因";

    /// <summary>到达过通道的调用总数（本地拦下的调用不该计进去）。</summary>
    public int Calls { get; private set; }

    /// <summary>最近一次调用看到的选项：凭证、deadline、调用方的取消令牌都在里面。</summary>
    public CallOptions? LastOptions { get; private set; }

    /// <summary>Get 的返回：值 + 键在不在。</summary>
    public byte[] Value { get; set; } = [];

    public bool Found { get; set; }

    /// <summary>Delete 的返回。</summary>
    public bool Deleted { get; set; } = true;

    public List<KvEntry> Entries { get; init; } = [];

    /// <summary>非 null 时，每次调用都抛它（模拟中台的各种失败）。</summary>
    public Func<RpcException>? Fail { get; init; }

    /// <summary>
    /// true 时每次调用都卡到 deadline 过（或调用方的 token 取消），
    /// 然后就像 grpc-dotnet 那样抛 <c>DeadlineExceeded</c>/<c>Cancelled</c>。
    /// </summary>
    public bool BlockUntilDeadline { get; init; }

    public async Task<KvGetResponse> KvGetAsync(KvGetRequest request, CallOptions options)
    {
        await Before(options, () => Gets.Add((request.Key.Namespace, request.Key.Key))).ConfigureAwait(false);
        return new KvGetResponse { Found = Found, Value = Google.Protobuf.ByteString.CopyFrom(Value) };
    }

    public async Task<KvPutResponse> KvPutAsync(KvPutRequest request, CallOptions options)
    {
        await Before(
            options,
            () => Puts.Add((
                request.Key.Namespace,
                request.Key.Key,
                request.Value.ToByteArray(),
                request.TtlSeconds))).ConfigureAwait(false);
        return new KvPutResponse();
    }

    public async Task<KvDeleteResponse> KvDeleteAsync(KvDeleteRequest request, CallOptions options)
    {
        await Before(options, () => Deletes.Add((request.Key.Namespace, request.Key.Key))).ConfigureAwait(false);
        return new KvDeleteResponse { Deleted = Deleted };
    }

    public async Task<KvScanResponse> KvScanAsync(KvScanRequest request, CallOptions options)
    {
        await Before(
            options,
            () => Scans.Add((request.Namespace, request.Prefix, request.Limit))).ConfigureAwait(false);
        var response = new KvScanResponse();
        response.Entries.AddRange(Entries);
        return response;
    }

    public async Task<PublishResponse> PublishAsync(PublishRequest request, CallOptions options)
    {
        await Before(options, () => Publishes.Add(request.Target)).ConfigureAwait(false);

        // 与真中台同形：受理与否、原因都在**响应字段**里，不是 gRPC 错误——
        // 客户端「把 accepted=false 翻成类型化异常」这件事要靠替身给出 rejected 才验得到
        return Accepted
            ? new PublishResponse { Accepted = true, RunId = RunId }
            : new PublishResponse { Accepted = false, Reason = RejectReason };
    }

    /// <summary>每次调用的公共前置：记账、带上凭证、按需失败或卡住。</summary>
    private async Task Before(CallOptions options, Action record)
    {
        lock (_gate)
        {
            Calls++;
            LastOptions = options;
            Tokens.Add(TokenOf(options));
            record();
        }

        if (Fail is not null)
        {
            throw Fail();
        }

        if (BlockUntilDeadline)
        {
            await Block(options).ConfigureAwait(false);
        }
    }

    private static async Task Block(CallOptions options) => await TestBlocking.Block(options).ConfigureAwait(false);

    private static string? TokenOf(CallOptions options) => TestTokens.Of(options);
}
/// <summary>从 <see cref="CallOptions"/> 里取状态凭证头。两个假通道（状态/网关）共用同一套口径。</summary>
internal static class TestTokens
{
    public static string? Of(CallOptions options)
    {
        var headers = options.Headers;
        if (headers is null)
        {
            return null;
        }

        foreach (var entry in headers)
        {
            if (entry.Key == StateClient.TokenMetadata)
            {
                return entry.Value;
            }
        }

        return null;
    }
}

/// <summary>与 grpc-dotnet 同形的「对端不响应」：卡到 deadline/取消，错误码也一致。</summary>
internal static class TestBlocking
{
    public static async Task Block(CallOptions options)
    {
        var deadline = options.Deadline ?? DateTime.UtcNow.AddSeconds(10);
        var remaining = deadline - DateTime.UtcNow;
        if (remaining > TimeSpan.Zero)
        {
            try
            {
                await Task.Delay(remaining, options.CancellationToken).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                // 与 grpc-dotnet 一致：调用方的 token 取消 → Cancelled（不是 DeadlineExceeded）
                throw new RpcException(new Status(StatusCode.Cancelled, "调用被取消"));
            }
        }

        throw new RpcException(new Status(StatusCode.DeadlineExceeded, "超过 deadline"));
    }
}

/// <summary>
/// PluginGateway 那条链路的替身——C# 侧没有独立的 mockhub 产品件，这一层就是它在
/// 本语言的同位物：实现发现三件套与 Invoke 的假实现，把「中台到底收到了什么」
/// （凭证、请求参数、信封）全记下来供断言。
///
/// 与 <see cref="FakeStateChannel"/> 同一分工：SDK 这一半（信封怎么构造、链怎么
/// 原样复制、业务结果怎么翻成类型化异常）在这里验；互调的治理行为（链校验、配额、
/// subject 覆盖）发生在真中台，替身不复刻。
/// </summary>
internal sealed class FakeGatewayChannel : IGatewayChannel
{
    private readonly object _gate = new();

    /// <summary>每次调用看到的凭证；没带这个头时是 null（与 <see cref="FakeStateChannel.Tokens"/> 同口径）。</summary>
    public List<string?> Tokens { get; } = [];

    public List<bool> ListRequests { get; } = [];

    public List<string> DescribeRequests { get; } = [];

    public List<(string Plugin, string Version, string FqName)> ContractRequests { get; } = [];

    /// <summary>收到的互调请求（整个请求——信封整备得对不对，只有在这一层看得到）。</summary>
    public List<InvokeRequest> Invokes { get; } = [];

    public CallOptions? LastOptions { get; private set; }

    /// <summary>到达过通道的调用总数（本地拦下的调用不该计进去）。</summary>
    public int Calls { get; private set; }

    /// <summary>非 null 时，每次调用都抛它（模拟中台的各种失败）。</summary>
    public Func<RpcException>? Fail { get; init; }

    /// <summary>
    /// true 时每次调用都卡到 deadline 过（或调用方的 token 取消）——
    /// 用来验「发现类吃 callTimeout、互调不吃」这条刻意的行为差。
    /// </summary>
    public bool BlockUntilDeadline { get; init; }

    /// <summary>ListPlugins 的返回。</summary>
    public List<PluginSummary> Plugins { get; init; } = [];

    public DescribeMessageResponse? DescribeResponse { get; set; }

    public GetContractResponse? ContractResponse { get; set; }

    /// <summary>
    /// 非 null 时 Invoke 交给它出结果（回显这类「看请求出应答」的替身用它）；
    /// 为 null 时固定回 <see cref="InvokeResponse"/>。
    /// </summary>
    public Func<InvokeRequest, InvokeResponse>? InvokeHandler { get; init; }

    public InvokeResponse? InvokeResponse { get; set; }

    public async Task<ListPluginsResponse> ListPluginsAsync(ListPluginsRequest request, CallOptions options)
    {
        await RecordAsync(options, () => ListRequests.Add(request.IncludeOffline)).ConfigureAwait(false);
        var response = new ListPluginsResponse();
        response.Plugins.AddRange(Plugins);
        return response;
    }

    public async Task<DescribeMessageResponse> DescribeMessageAsync(DescribeMessageRequest request, CallOptions options)
    {
        await RecordAsync(options, () => DescribeRequests.Add(request.FqName)).ConfigureAwait(false);
        return DescribeResponse ?? new DescribeMessageResponse();
    }

    public async Task<GetContractResponse> GetContractAsync(GetContractRequest request, CallOptions options)
    {
        await RecordAsync(options, () => ContractRequests.Add((request.Plugin, request.Version, request.FqName)))
            .ConfigureAwait(false);
        return ContractResponse ?? new GetContractResponse();
    }

    public async Task<InvokeResponse> InvokeAsync(InvokeRequest request, CallOptions options)
    {
        InvokeResponse? response = null;
        await RecordAsync(options, () =>
        {
            Invokes.Add(request);
            response = InvokeHandler?.Invoke(request) ?? InvokeResponse;
        }).ConfigureAwait(false);
        return response!;
    }

    /// <summary>记账、带上凭证、按需失败或卡住。与 <see cref="FakeStateChannel"/> 的公共前置同一个形状。</summary>
    private async Task RecordAsync(CallOptions options, Action record)
    {
        lock (_gate)
        {
            Calls++;
            LastOptions = options;
            Tokens.Add(TestTokens.Of(options));
            record();
        }

        if (Fail is not null)
        {
            throw Fail();
        }

        if (BlockUntilDeadline)
        {
            await TestBlocking.Block(options).ConfigureAwait(false);
        }
    }
}

/// <summary>
/// 实现了 <see cref="IGatewayAware"/> 的插件替身（与 <see cref="StateAwarePlugin"/> 同款）。
/// </summary>
internal sealed class GatewayAwarePlugin : FakePlugin, IGatewayAware
{
    private readonly object _gate = new();
    private GatewayClient? _gateway;

    /// <summary>被注入过几次。「每次重注册都重新注入」这件事靠它证明。</summary>
    public int Injected { get; private set; }

    public void SetGateway(GatewayClient gateway)
    {
        lock (_gate)
        {
            _gateway = gateway;
            Injected++;
        }
    }

    /// <summary>当前拿到的客户端；还没注入时是 null。</summary>
    public GatewayClient? Gateway()
    {
        lock (_gate)
        {
            return _gateway;
        }
    }
}
