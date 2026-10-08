using Google.Protobuf.WellKnownTypes;
using Grpc.Core;
using Hub.V1;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 网关客户端（发现与互调）的行为测试。
///
/// 用假通道而不是真中台，分工与 <see cref="StateTests"/> 相同：这里验的是**客户端
/// 自己的规矩**——凭证放哪、信封怎么整备、链怎么原样复制、业务结果怎么翻成类型化
/// 异常。互调的治理行为（链校验、配额、subject 覆盖、deadline 夹紧的最终裁决）发生
/// 在真中台，替身不复刻。
/// </summary>
public class GatewayTests
{
    private static CancellationToken Ct => TestContext.Current.CancellationToken;

    private static GatewayClient Client(FakeGatewayChannel channel, int timeoutMs = 1000) =>
        new(channel, TimeSpan.FromMilliseconds(timeoutMs));

    /// <summary>最简单的下游替身：把收到的 JSON 载荷回显出去（在下游信封上回）。</summary>
    private static InvokeResponse Echo(InvokeRequest request)
    {
        var payload = Envelopes.PayloadJson(request.Envelope);
        var text = payload is not null && payload.TryGetValue("text", out var value) ? value as string ?? "" : "";
        return new InvokeResponse
        {
            Outcome = InvokeOutcome.Handled,
            Envelope = Envelopes.WithPayloadJson(
                request.Envelope,
                new Dictionary<string, object?> { ["echo"] = text }),
        };
    }

    /// <summary>
    /// ULID 字符集在这里**故意再写一遍**而不是复用被测代码的常量：
    /// 两边共用一份写错的字母表，测了等于没测。
    /// </summary>
    private static bool IsUlid(string s)
    {
        if (s.Length != 26 || s[0] > '7')
        {
            return false;
        }

        return s.All(c => c is (>= '0' and <= '9') or (>= 'A' and <= 'Z') and not 'I' and not 'L' and not 'O' and not 'U');
    }

    [Fact]
    public async Task 发现三件套都带上凭证且参数原样落到底层()
    {
        var channel = new FakeGatewayChannel
        {
            Plugins = [new PluginSummary { Name = "other-plugin", Online = true, InstanceCount = 1 }],
            DescribeResponse = new DescribeMessageResponse(),
            ContractResponse = new GetContractResponse { Name = "other-plugin", Version = "1.0.0" },
        };
        var gateway = Client(channel);
        gateway.SetToken("tok-1");

        var plugins = await gateway.ListPluginsAsync(includeOffline: true, Ct);
        var described = await gateway.DescribeMessageAsync("wms.v1.OrderCreated", Ct);
        var contract = await gateway.GetContractAsync("other-plugin", version: "1.0.0", fqName: "wms.v1.OrderCreated", Ct);

        Assert.Single(plugins);
        Assert.NotNull(described);
        Assert.Equal("other-plugin", contract.Name);

        Assert.Equal([true], channel.ListRequests);
        Assert.Equal("wms.v1.OrderCreated", Assert.Single(channel.DescribeRequests));
        Assert.Equal(("other-plugin", "1.0.0", "wms.v1.OrderCreated"), Assert.Single(channel.ContractRequests));
        Assert.All(channel.Tokens, t => Assert.Equal("tok-1", t));
    }

    [Fact]
    public async Task 发现类调用受单次调用上限约束()
    {
        var channel = new FakeGatewayChannel { BlockUntilDeadline = true };
        var gateway = Client(channel, 200);

        var start = DateTime.UtcNow;
        var ex = await Assert.ThrowsAsync<RpcException>(() => gateway.ListPluginsAsync(cancellationToken: Ct));

        Assert.Equal(StatusCode.DeadlineExceeded, ex.StatusCode);
        Assert.True(DateTime.UtcNow - start < TimeSpan.FromSeconds(2), "没有在上限附近返回");
    }

    /// <summary>
    /// 与 <see cref="StateClient"/> 同一条规矩：还没注册（凭证为空）时**不发**这个头。
    /// </summary>
    [Fact]
    public async Task 未注册时不带凭证头()
    {
        var channel = new FakeGatewayChannel();
        var gateway = Client(channel);

        await gateway.ListPluginsAsync(cancellationToken: Ct);

        Assert.Null(Assert.Single(channel.Tokens));
    }

    [Fact]
    public async Task 凭证随注册轮换后用的是新凭证()
    {
        var channel = new FakeGatewayChannel();
        var gateway = Client(channel);

        gateway.SetToken("tok-1");
        await gateway.ListPluginsAsync(cancellationToken: Ct);
        gateway.SetToken("tok-2");
        await gateway.DescribeMessageAsync("a", Ct);

        Assert.Equal("tok-1", channel.Tokens[0]);
        Assert.Equal("tok-2", channel.Tokens[1]);
    }

    /// <summary>
    /// 网关与状态用的是**同一张**凭证，撞上 401 也是同一件事：
    /// 客户端原样抛错，同时通过共享的信号叫醒注册循环去换新凭证。
    /// </summary>
    [Fact]
    public async Task 凭证被拒时原样抛出并且叫醒共享的注册循环信号()
    {
        var state = new StateClient(new FakeStateChannel());
        var channel = new FakeGatewayChannel
        {
            Fail = () => new RpcException(new Status(StatusCode.Unauthenticated, "状态凭证无效或已失效")),
        };
        var gateway = new GatewayClient(channel, state.Denied, TimeSpan.FromSeconds(1));

        var ex = await Assert.ThrowsAsync<RpcException>(() => gateway.ListPluginsAsync(cancellationToken: Ct));
        Assert.Equal(StatusCode.Unauthenticated, ex.StatusCode);

        using var wait = new CancellationTokenSource(TimeSpan.FromMilliseconds(500));
        Assert.True(await state.Denied.WaitAsync(wait.Token), "网关面的 401 也该叫醒注册循环");
    }

    /// <summary>401 之外的失败一律不叫醒注册循环：它们跟凭证无关，重注册解决不了。</summary>
    [Theory]
    [InlineData(StatusCode.DeadlineExceeded)]
    [InlineData(StatusCode.Cancelled)]
    [InlineData(StatusCode.NotFound)]
    [InlineData(StatusCode.Internal)]
    [InlineData(StatusCode.Unavailable)]
    public async Task 其它错误不会叫醒注册循环(StatusCode code)
    {
        var state = new StateClient(new FakeStateChannel());
        var channel = new FakeGatewayChannel
        {
            Fail = () => new RpcException(new Status(code, "与凭证无关")),
        };
        var gateway = new GatewayClient(channel, state.Denied, TimeSpan.FromSeconds(1));

        var ex = await Assert.ThrowsAsync<RpcException>(() => gateway.ListPluginsAsync(cancellationToken: Ct));
        Assert.Equal(code, ex.StatusCode);

        using var wait = new CancellationTokenSource(TimeSpan.FromMilliseconds(200));
        Assert.False(await state.Denied.WaitAsync(wait.Token), "与凭证无关的错误不该触发重新注册");
    }

    /// <summary>
    /// 原始 Invoke **不做业务结果映射**：REJECTED/ERROR 是应答字段，不是异常。
    /// 分两层的原因：映射过的便捷入口会把「中台到底回了什么」藏起来，
    /// 想看原始应答的人不该被迫去拼信封。
    /// </summary>
    [Fact]
    public async Task 原始Invoke的业务结果原样返回不做映射()
    {
        var channel = new FakeGatewayChannel
        {
            InvokeResponse = new InvokeResponse
            {
                Outcome = InvokeOutcome.Rejected,
                Reason = "目标插件的固定提示",
                Issues = { Envelopes.Issue("payload.text", "缺少必填字段") },
            },
        };
        var gateway = Client(channel);

        var response = await gateway.InvokeAsync(new InvokeRequest { Plugin = "x" }, Ct);

        Assert.Equal(InvokeOutcome.Rejected, response.Outcome);
        Assert.Equal("目标插件的固定提示", response.Reason);
        Assert.Single(response.Issues);
    }

    /// <summary>互调不吃 callTimeout：预算由信封 deadline 表达，发现类的上限不该掐死业务调用。</summary>
    [Fact]
    public async Task 互调不吃callTimeout预算由信封deadline表达()
    {
        var channel = new FakeGatewayChannel { BlockUntilDeadline = true, InvokeHandler = Echo };
        // 发现类的上限压到 300ms；信封预算给 1.2s
        var gateway = new GatewayClient(channel, TimeSpan.FromMilliseconds(300));

        var envelope = new Envelope
        {
            MessageId = Envelopes.NewUlid(),
            DeadlineMs = DateTimeOffset.UtcNow.AddMilliseconds(1200).ToUnixTimeMilliseconds(),
        };

        var start = DateTime.UtcNow;
        var ex = await Assert.ThrowsAsync<RpcException>(
            () => gateway.InvokeAsync(new InvokeRequest { Plugin = "x", Envelope = envelope }, Ct));
        var elapsed = DateTime.UtcNow - start;

        Assert.Equal(StatusCode.DeadlineExceeded, ex.StatusCode);
        Assert.True(elapsed >= TimeSpan.FromMilliseconds(900), $"互调不该被 300ms 的发现上限掐死，实际 {elapsed}");
    }

    /// <summary>
    /// 便捷入口的 happy path：HANDLED 返回**下游的信封**——载荷是回显出来的，
    /// 证明调用方拿到的是下游处理结果而不是自己发出去的那份。
    /// </summary>
    [Fact]
    public async Task invokePlugin受理时返回下游信封()
    {
        var channel = new FakeGatewayChannel { InvokeHandler = Echo };
        var gateway = Client(channel);
        gateway.SetToken("tok-1");

        var output = await gateway.InvokePluginAsync(
            "echo-plugin",
            new InvokeOptions { PayloadJson = new Dictionary<string, object?> { ["text"] = "你好" } },
            Ct);

        Assert.Equal("你好", Envelopes.PayloadJson(output)!["echo"]);
        Assert.Equal("tok-1", Assert.Single(channel.Tokens));
    }

    [Fact]
    public async Task invokePlugin被目标插件拒绝时issues随异常携带()
    {
        var channel = new FakeGatewayChannel
        {
            InvokeResponse = new InvokeResponse
            {
                Outcome = InvokeOutcome.Rejected,
                Reason = "校验未通过",
                Issues = { Envelopes.Issue("payload.text", "缺少必填字段 text") },
            },
        };
        var gateway = Client(channel);

        var ex = await Assert.ThrowsAsync<InvokeRejectedException>(
            () => gateway.InvokePluginAsync("echo-plugin", cancellationToken: Ct));

        Assert.Equal("校验未通过", ex.Reason);
        var issue = Assert.Single(ex.Issues);
        Assert.Equal("payload.text", issue.Path);
        // message 里也要能直接看到是哪个字段没过，日志不必先翻属性
        Assert.Contains("payload.text", ex.Message);
        Assert.Contains("缺少必填字段 text", ex.Message);
        // 想对两类失败做同一件事（记日志、退避）时抓基类
        InvokeFailureException asBase = ex;
        Assert.Equal("校验未通过", asBase.Reason);
    }

    [Fact]
    public async Task invokePlugin被中台拦下时reason随异常携带()
    {
        var channel = new FakeGatewayChannel
        {
            InvokeResponse = new InvokeResponse { Outcome = InvokeOutcome.Error, Reason = "检测到互调环: a->b->a" },
        };
        var gateway = Client(channel);

        var ex = await Assert.ThrowsAsync<InvokeFailedException>(
            () => gateway.InvokePluginAsync("echo-plugin", cancellationToken: Ct));

        Assert.Equal("检测到互调环: a->b->a", ex.Reason);
        Assert.Contains("检测到互调环", ex.Message);
    }

    [Fact]
    public async Task invokePlugin未给出原因时异常也不含糊其辞()
    {
        var channel = new FakeGatewayChannel { InvokeResponse = new InvokeResponse { Outcome = InvokeOutcome.Error } };
        var gateway = Client(channel);

        var ex = await Assert.ThrowsAsync<InvokeFailedException>(
            () => gateway.InvokePluginAsync("echo-plugin", cancellationToken: Ct));

        Assert.Empty(ex.Reason);
        Assert.Contains("未给出原因", ex.Message);
    }

    /// <summary>UNSPECIFIED 只可能来自不按契约实现的假中台；按未知失败处理而不是猜。</summary>
    [Fact]
    public async Task invokePlugin对未知的结果分类按失败处理()
    {
        var channel = new FakeGatewayChannel { InvokeResponse = new InvokeResponse() };
        var gateway = Client(channel);

        var ex = await Assert.ThrowsAsync<InvokeFailedException>(
            () => gateway.InvokePluginAsync("echo-plugin", cancellationToken: Ct));

        Assert.Contains("未知的结果分类", ex.Reason);
    }

    /// <summary>中台违约（协议上 HANDLED 必带信封）：显式炸出来，别让调用方把「没有结果」当「结果是空的」。</summary>
    [Fact]
    public async Task invokePlugin对不带信封的HANDLED显式报错()
    {
        var channel = new FakeGatewayChannel
        {
            InvokeResponse = new InvokeResponse { Outcome = InvokeOutcome.Handled },
        };
        var gateway = Client(channel);

        var ex = await Assert.ThrowsAsync<InvokeFailedException>(
            () => gateway.InvokePluginAsync("echo-plugin", cancellationToken: Ct));

        Assert.Contains("HANDLED", ex.Reason);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("   ")]
    public async Task invokePlugin缺少目标插件名在本地就被拦住(string? plugin)
    {
        var channel = new FakeGatewayChannel();
        var gateway = Client(channel);

        await Assert.ThrowsAsync<ArgumentException>(
            () => gateway.InvokePluginAsync(plugin!, cancellationToken: Ct));

        Assert.Equal(0, channel.Calls);
    }

    [Fact]
    public async Task invokePlugin的两种载荷只能二选一()
    {
        var channel = new FakeGatewayChannel();
        var gateway = Client(channel);

        await Assert.ThrowsAsync<ArgumentException>(
            () => gateway.InvokePluginAsync(
                "x",
                new InvokeOptions
                {
                    PayloadJson = new Dictionary<string, object?>(),
                    Payload = new Struct(),
                },
                Ct));

        Assert.Equal(0, channel.Calls);
    }

    /// <summary>
    /// 「传入当前信封」的核心语义：trace 从上游贯通（三个 id 复制过去）、
    /// 链**原样**带下去——不追加自己，链上记 caller 是中台代调时的事，
    /// SDK 抢着做会让链上出现重复节点。message_id 则必须是新的：它是幂等键。
    /// </summary>
    [Fact]
    public async Task invokePlugin复制当前信封的trace上下文且链原样带上()
    {
        var channel = new FakeGatewayChannel { InvokeHandler = Echo };
        var gateway = Client(channel);

        var current = new Envelope
        {
            MessageId = "current-message",
            TraceId = "current-trace",
            RunId = "run-7",
            NodeId = "node-3",
        };
        current.Meta[GatewayClient.CallChainMeta] = "p1,p2";

        await gateway.InvokePluginAsync(
            "echo-plugin",
            new InvokeOptions { CurrentEnvelope = current, PayloadJson = new Dictionary<string, object?> { ["text"] = "hi" } },
            Ct);

        var sent = Assert.Single(channel.Invokes).Envelope!;
        Assert.Equal("current-trace", sent.TraceId);
        Assert.Equal("run-7", sent.RunId);
        Assert.Equal("node-3", sent.NodeId);
        Assert.Equal("p1,p2", sent.Meta[GatewayClient.CallChainMeta]);
        Assert.NotEqual("current-message", sent.MessageId);
        Assert.True(IsUlid(sent.MessageId), $"message_id 应是新 ULID，实际 {sent.MessageId}");
        Assert.Equal(PayloadType.Request, sent.Type);
    }

    /// <summary>顶层发起（没有当前信封）就是一次新的调用：新 trace、不带链，run/node 也不虚构。</summary>
    [Fact]
    public async Task invokePlugin顶层发起时生成新trace且不带链()
    {
        var channel = new FakeGatewayChannel { InvokeHandler = Echo };
        var gateway = Client(channel);

        await gateway.InvokePluginAsync("echo-plugin", cancellationToken: Ct);

        var sent = Assert.Single(channel.Invokes).Envelope!;
        Assert.True(IsUlid(sent.TraceId), $"trace_id 应是新 ULID，实际 {sent.TraceId}");
        Assert.False(sent.Meta.ContainsKey(GatewayClient.CallChainMeta));
        Assert.Empty(sent.RunId);
        Assert.Empty(sent.NodeId);
    }

    /// <summary>当前信封没带链（顶层进来的消息）时也不虚构一条空链出来。</summary>
    [Fact]
    public async Task invokePlugin当前信封没有链时不带链()
    {
        var channel = new FakeGatewayChannel { InvokeHandler = Echo };
        var gateway = Client(channel);

        var current = new Envelope { TraceId = "current-trace" };

        await gateway.InvokePluginAsync(
            "echo-plugin",
            new InvokeOptions { CurrentEnvelope = current },
            Ct);

        var sent = Assert.Single(channel.Invokes).Envelope!;
        Assert.Equal("current-trace", sent.TraceId);
        Assert.False(sent.Meta.ContainsKey(GatewayClient.CallChainMeta));
    }

    /// <summary>
    /// 本次预算比整体预算更晚时以本次为准（信封 deadline 被夹到 now+timeout 附近），
    /// 且预算原样写进 <c>timeout_ms</c>、gRPC 的 Deadline 跟着信封走——
    /// 到点客户端先放弃，而不是等中台或下游超时。
    /// </summary>
    [Fact]
    public async Task invokePlugin本次预算更晚时夹紧deadline并写进timeoutMs()
    {
        var channel = new FakeGatewayChannel { InvokeHandler = Echo };
        var gateway = Client(channel);

        var current = new Envelope
        {
            TraceId = "current-trace",
            DeadlineMs = DateTimeOffset.UtcNow.AddSeconds(10).ToUnixTimeMilliseconds(),
        };

        var start = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        await gateway.InvokePluginAsync(
            "echo-plugin",
            new InvokeOptions { CurrentEnvelope = current, Timeout = TimeSpan.FromSeconds(5) },
            Ct);

        var request = Assert.Single(channel.Invokes);
        Assert.Equal(5000u, request.TimeoutMs);

        var sent = request.Envelope!;
        // 整体预算 10s 更晚：夹到本次预算附近（1.5s 的余量容住测试机的时间粒度）
        Assert.InRange(sent.DeadlineMs, start + 3500, start + 6500);

        var rpcDeadline = channel.LastOptions!.Value.Deadline!.Value;
        Assert.InRange(
            new DateTimeOffset(rpcDeadline).ToUnixTimeMilliseconds(),
            start + 3500,
            start + 6500);
    }

    /// <summary>整体预算比本次预算更早时以它为准——调用不该活得比触发它的那次处理更久。</summary>
    [Fact]
    public async Task invokePlugin整体预算更早时以它为准()
    {
        var channel = new FakeGatewayChannel { InvokeHandler = Echo };
        var gateway = Client(channel);

        var start = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        var current = new Envelope
        {
            TraceId = "current-trace",
            DeadlineMs = start + 2000,
        };

        await gateway.InvokePluginAsync(
            "echo-plugin",
            new InvokeOptions { CurrentEnvelope = current, Timeout = TimeSpan.FromSeconds(10) },
            Ct);

        var request = Assert.Single(channel.Invokes);
        Assert.Equal(start + 2000, request.Envelope!.DeadlineMs);
        // timeout_ms 仍然原样下发：夹紧的最终裁决在中台
        Assert.Equal(10000u, request.TimeoutMs);
    }

    /// <summary>没传预算也没继承预算时：不虚构 deadline、不设 gRPC 上限，由中台的默认预算兜底。</summary>
    [Fact]
    public async Task invokePlugin没有预算时不虚构deadline()
    {
        var channel = new FakeGatewayChannel { InvokeHandler = Echo };
        var gateway = Client(channel);

        await gateway.InvokePluginAsync("echo-plugin", cancellationToken: Ct);

        var request = Assert.Single(channel.Invokes);
        Assert.Equal(0, request.Envelope!.DeadlineMs);
        Assert.Equal(0u, request.TimeoutMs);
        Assert.Null(channel.LastOptions!.Value.Deadline);
    }

    /// <summary>
    /// 已经过期的预算不再设 gRPC deadline：让「必然被拒」的应答能带着 reason 回来，
    /// 比一句干巴巴的 DeadlineExceeded 多一条排查线索。
    /// </summary>
    [Fact]
    public async Task invokePlugin预算已过期时不设gRPC上限()
    {
        var channel = new FakeGatewayChannel { InvokeHandler = Echo };
        var gateway = Client(channel);

        var current = new Envelope { TraceId = "t", DeadlineMs = 1 };

        await gateway.InvokePluginAsync(
            "echo-plugin",
            new InvokeOptions { CurrentEnvelope = current },
            Ct);

        Assert.Equal(1, Assert.Single(channel.Invokes).Envelope!.DeadlineMs);
        Assert.Null(channel.LastOptions!.Value.Deadline);
    }

    /// <summary>两种载荷形态：JSON 对象包成 Struct；业务 proto 消息按 Any 打包。</summary>
    [Fact]
    public async Task invokePlugin的两种载荷形态都能发()
    {
        var channel = new FakeGatewayChannel { InvokeHandler = Echo };
        var gateway = Client(channel);

        await gateway.InvokePluginAsync(
            "echo-plugin",
            new InvokeOptions { PayloadJson = new Dictionary<string, object?> { ["text"] = "json" } },
            Ct);
        await gateway.InvokePluginAsync(
            "echo-plugin",
            new InvokeOptions { Payload = new Struct() },
            Ct);

        var requests = channel.Invokes;
        Assert.Equal(2, requests.Count);
        Assert.Equal(Envelopes.StructTypeUrl, requests[0].Envelope!.Payload.TypeUrl);
        Assert.Equal("type.googleapis.com/google.protobuf.Struct", requests[1].Envelope!.Payload.TypeUrl);
    }

    [Fact]
    public async Task 超出线上协议上限的预算在本地就被拦住()
    {
        var channel = new FakeGatewayChannel();
        var gateway = Client(channel);

        await Assert.ThrowsAsync<ArgumentException>(
            () => gateway.InvokePluginAsync(
                "x",
                new InvokeOptions { Timeout = TimeSpan.FromMilliseconds(uint.MaxValue) + TimeSpan.FromMilliseconds(1) },
                Ct));

        Assert.Equal(0, channel.Calls);
    }

    /// <summary>
    /// 互调的两个常量与跨语言契约文件一致：链随 meta 传递、深度上限含本次 caller。
    /// 键名或深度漂了，成环的请求就会一路绿灯打到下游——中台那侧的同一条断言
    /// 在 <c>crates/hub-grpc/tests/state_rules.rs</c>，三方读同一份文件。
    /// </summary>
    [Fact]
    public void 互调常量与跨语言契约文件一致()
    {
        using var document = System.Text.Json.JsonDocument.Parse(
            File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "testdata", "hub-rules.json")));
        var gatewayRules = document.RootElement.GetProperty("gateway");

        Assert.Equal(GatewayClient.CallChainMeta, gatewayRules.GetProperty("callChainMeta").GetString());
        Assert.Equal(GatewayClient.MaxInvokeDepth, gatewayRules.GetProperty("maxInvokeDepth").GetInt32());
        Assert.Equal("hub.call_chain", GatewayClient.CallChainMeta);
        Assert.Equal(8, GatewayClient.MaxInvokeDepth);
    }
}
