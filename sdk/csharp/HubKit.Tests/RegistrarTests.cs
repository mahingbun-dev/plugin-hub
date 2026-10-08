using Grpc.Core;
using Hub.V1;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 注册 / 心跳 / 自愈三条路径的测试。
///
/// 这块**必须**用假的中台通道测：「被摘除 → 重新注册」要等到中台把实例忘掉才走得到，
/// 而那是中台的内部状态；拿真中台去构造这一步，测的就变成中台了。
/// </summary>
public class RegistrarTests
{
    private static HubConfig Config(
        StringWriter sink,
        TimeSpan? heartbeatOverride = null,
        string instanceId = "test-instance-1") => new()
    {
        HubAddr = "http://127.0.0.1:8093",
        AdvertiseAddr = "http://127.0.0.1:19211",
        InstanceId = instanceId,
        // 生产里的 5s / 10s 在测试里等不起，压到毫秒级——整条自愈路径才有机会
        // 在一次测试里走完
        RetryInterval = TimeSpan.FromMilliseconds(10),
        HeartbeatIntervalOverride = heartbeatOverride ?? TimeSpan.FromMilliseconds(15),
        Logger = new HubLogger(HubLogLevel.Debug, sink),
    };

    private static CancellationToken Ct => TestContext.Current.CancellationToken;

    /// <summary>一个不与中台说话的 StateClient（假通道）；凭证由注册流程注入。</summary>
    private static StateClient State(FakeStateChannel? channel = null) =>
        new(channel ?? new FakeStateChannel(), TimeSpan.FromSeconds(1));

    /// <summary>
    /// 一个不与中台说话的 GatewayClient（假通道）。与 <see cref="State"/> 同款缺省——
    /// 大多数用例不关心网关面，不该被它的构造细节干扰。
    /// </summary>
    private static GatewayClient Gateway(FakeGatewayChannel? channel = null, StateClient? state = null) =>
        new(channel ?? new FakeGatewayChannel(), state?.Denied, TimeSpan.FromSeconds(1));

    [Fact]
    public async Task 被摘除后心跳被判要求重注册时会自己重新注册()
    {
        // 第 2 拍心跳回 reregister_required：中台认不出这个实例了
        var channel = new FakeRegistryChannel { ReregisterAtHeartbeat = 2 };
        var sink = new StringWriter();
        var config = Config(sink);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, State(), Gateway());

        using var cts = new CancellationTokenSource();
        var loop = registrar.RunAsync(cts.Token);

        var healed = await TestWait.Until(() => registrar.RegisterCount >= 2, TimeSpan.FromSeconds(10));

        await cts.CancelAsync();
        await loop;

        Assert.True(healed, $"没有自愈：注册了 {channel.RegisterCalls} 次、心跳了 {channel.HeartbeatCalls} 次");
        Assert.Equal(2, channel.RegisterCalls);
        Assert.Contains("中台要求重新注册（实例可能已被摘除）", sink.ToString());
    }

    [Fact]
    public async Task 心跳失败不会让插件停止心跳()
    {
        // 第 1 拍心跳抛异常（网络抖动），之后每一拍都正常
        var channel = new FakeRegistryChannel { FailAtHeartbeat = 1 };
        var sink = new StringWriter();
        var config = Config(sink);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, State(), Gateway());

        using var cts = new CancellationTokenSource();
        var loop = registrar.RunAsync(cts.Token);

        var kept = await TestWait.Until(() => channel.HeartbeatCalls >= 3, TimeSpan.FromSeconds(10));

        await cts.CancelAsync();
        await loop;

        Assert.True(kept, $"抖了一下就哑了：只心跳了 {channel.HeartbeatCalls} 次");
        // 「抖一下就重注册」是错的：注册会做一次可达性探测，白挨一轮往返
        Assert.Equal(1, channel.RegisterCalls);
        Assert.Contains("心跳失败", sink.ToString());
    }

    [Fact]
    public async Task 注册被拒会按重试间隔一直重试并逐条打出原因()
    {
        var channel = new FakeRegistryChannel
        {
            RejectRegister =
            [
                new Rejection
                {
                    Code = RejectCode.BreakingChange,
                    Message = "相对版本 1.0.0 存在破坏性契约变更",
                    Detail = "wms.v1.Order.sku（编号 1，类型 string）已被删除",
                },
            ],
        };
        var sink = new StringWriter();
        var config = Config(sink);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, State(), Gateway());

        using var cts = new CancellationTokenSource();
        var loop = registrar.RunAsync(cts.Token);

        var retried = await TestWait.Until(() => channel.RegisterCalls >= 3, TimeSpan.FromSeconds(10));

        await cts.CancelAsync();
        await loop;

        Assert.True(retried, $"没有重试：只试了 {channel.RegisterCalls} 次");
        Assert.Equal(0, registrar.RegisterCount);

        var text = sink.ToString();
        // 每条原因各占一行、三个字段都摊开——「一行里糊着 N 条原因」正是要避免的
        Assert.Contains("\"msg\":\"中台拒绝了注册\"", text);
        Assert.Contains("\"code\":\"BREAKING_CHANGE\"", text);
        Assert.Contains("\"message\":\"相对版本 1.0.0 存在破坏性契约变更\"", text);
        Assert.Contains("\"detail\":\"wms.v1.Order.sku（编号 1，类型 string）已被删除\"", text);
        // 收尾一行只出现一次，带上条数与等待时间
        Assert.Contains("\"reasons\":1", text);
        Assert.Contains("\"retry_in\":\"10ms\"", text);
    }

    [Fact]
    public async Task 心跳周期取自中台回执()
    {
        var channel = new FakeRegistryChannel();
        var sink = new StringWriter();
        var config = Config(sink);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, State(), Gateway());

        Assert.Equal(HubConfig.HeartbeatFallbackInterval, registrar.Interval);

        await registrar.RegisterOnceAsync(CancellationToken.None);

        // 假通道回的是 1 秒。中台指定的周期要真的覆盖掉兜底值——
        // 不然插件会用自己的节奏发心跳，中台那边就是一堆无效心跳
        Assert.Equal(TimeSpan.FromSeconds(1), registrar.Interval);
        Assert.Contains("\"msg\":\"已注册到中台\"", sink.ToString());
    }

    /// <summary>
    /// 注册成功过的实例退出时，注销要带上**注册响应里下发的那张凭证**。
    ///
    /// `instance_id` 是插件自报的、可以跨插件相撞，凭证才是中台认属主的依据
    /// （见 <c>UnregisterRequest.state_token</c>）。只断言「发了注销」是不够的：
    /// 一份不带凭证的注销在真中台上会被拒，表现得和「没注销」一样。
    /// </summary>
    [Fact]
    public async Task 注销带上实例标识原因与注册时下发的凭证()
    {
        var channel = new FakeRegistryChannel { StateTokens = ["tok-1"] };
        var sink = new StringWriter();
        var config = Config(sink);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, State(), Gateway());

        await registrar.RegisterOnceAsync(CancellationToken.None);
        await registrar.UnregisterAsync();

        var request = Assert.Single(channel.Unregisters);
        Assert.Equal("test-instance-1", request.InstanceId);
        Assert.Equal("插件优雅退出", request.Reason);
        Assert.Equal("tok-1", request.StateToken);

        // 中台凭这张凭证认下了属主，把这一行摘掉了
        Assert.False(channel.IsRegistered("test-instance-1"));
    }

    /// <summary>
    /// **没拿到过凭证就根本不发注销**。
    ///
    /// 凭证只在注册成功时下发，所以「手里没有凭证」等于「本实例没进过注册表」，此时这一发
    /// 注销摘不掉任何东西。而 `instance_id` 是插件自报的、可以跟别的插件撞（缺省「主机名
    /// -PID」，同一 host 网络下容器 PID 又都是 1）——多发的这一发若被中台按 `instance_id`
    /// 删行，删掉的正是**对方**那一行：实测 auth 重启一次，sql-executor 的工具从 MCP 工具面
    /// 上全部消失，而它自己的日志停在「已注册到中台」之后毫无异常。
    /// </summary>
    [Fact]
    public async Task 没拿到过凭证时根本不发注销()
    {
        var channel = new FakeRegistryChannel();
        var sink = new StringWriter();
        var config = Config(sink);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, State(), Gateway());

        await registrar.UnregisterAsync();

        Assert.Equal(0, channel.UnregisterCalls);
        Assert.Empty(channel.Unregisters);
        Assert.Contains("本实例没有注册凭证，跳过主动注销", sink.ToString());
    }

    /// <summary>
    /// 注册被拒的插件退出时也不发注销——这是上面那条的**现实形态**：注册被拒的插件手里
    /// 本来就没有凭证，而它退出时那一发注销，正是删掉属主那一行的路径。
    /// </summary>
    [Fact]
    public async Task 注册被拒的实例退出时不发注销()
    {
        var channel = new FakeRegistryChannel
        {
            RejectRegister = [new Rejection { Code = RejectCode.ManifestInvalid, Message = "本测试总是拒绝注册" }],
        };
        var sink = new StringWriter();
        var config = Config(sink);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, State(), Gateway());

        await Assert.ThrowsAsync<RegistrationRejectedException>(
            () => registrar.RegisterOnceAsync(CancellationToken.None));

        await registrar.UnregisterAsync();

        Assert.Equal(0, channel.UnregisterCalls);
        Assert.Contains("本实例没有注册凭证，跳过主动注销", sink.ToString());
    }

    /// <summary>
    /// 中台已经挂了的时候退出，不该把退出流程本身炸掉——那种情况下中台会靠心跳超时自行摘除。
    /// 这条**必须先注册成功**：跳过注销是个分支，走不到这里就测不到失败路径。
    /// </summary>
    [Fact]
    public async Task 注销失败不算错误()
    {
        var channel = new FakeRegistryChannel { FailUnregister = true };
        var sink = new StringWriter();
        var config = Config(sink);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, State(), Gateway());

        await registrar.RegisterOnceAsync(CancellationToken.None);
        await registrar.UnregisterAsync();

        Assert.Equal(1, channel.UnregisterCalls);
        Assert.Contains("注销失败（中台会在心跳超时后自行摘除）", sink.ToString());
    }

    /// <summary>
    /// 撞了 `instance_id` 的两个插件：**谁退出都删不掉对方的行**。
    ///
    /// A 先注册（行归 <c>tok-A</c>），B 用同一个 `instance_id` 抢到注册（中台轮换凭证，
    /// 行归 <c>tok-B</c>）。A 随后退出时手里是那张已被轮换掉的旧凭证——中台若只按
    /// `instance_id` 删行，删掉的就是 B 那一行，而 B 的心跳仍会命中、返回 accepted，
    /// **完全察觉不到自己从注册表里消失了**。
    ///
    /// 这条同时钉住替身本身：替身照着真中台的判定校验凭证，否则「发了不带凭证的注销」
    /// 这类用例会照样全绿。
    /// </summary>
    [Fact]
    public async Task 凭证不符时中台不删任何行()
    {
        var channel = new FakeRegistryChannel { StateTokens = ["tok-A", "tok-B"] };
        const string Shared = "shared-instance";

        var sinkA = new StringWriter();
        var configA = Config(sinkA, instanceId: Shared);
        var a = new Registrar(channel, new FakePlugin { Name = "a" }, configA, configA.Logger!, State(), Gateway());

        var sinkB = new StringWriter();
        var configB = Config(sinkB, instanceId: Shared);
        var b = new Registrar(channel, new FakePlugin { Name = "b" }, configB, configB.Logger!, State(), Gateway());

        await a.RegisterOnceAsync(CancellationToken.None);
        await b.RegisterOnceAsync(CancellationToken.None);

        // A 退出：用的是自己注册时那张 tok-A，而这一行现在归属 B
        await a.UnregisterAsync();

        // **一行都没被删掉**：这一发确实发出去了（不是被客户端跳过），只是被中台拒了
        Assert.Equal(new[] { Shared }, channel.RegisteredInstances);

        await b.UnregisterAsync();

        // A 那一发带的是自己那张旧凭证，B 那一发带的才是当前凭证——摘掉的也才是自己那一行
        Assert.Equal(new[] { "tok-A", "tok-B" }, channel.UnregisterTokens);
        Assert.Empty(channel.RegisteredInstances);
    }

    [Fact]
    public void 启动前的本地预检挡住三类明显的manifest问题()
    {
        Assert.Throws<HubConfigException>(() => PluginHost.CheckManifest(new FakePlugin { Name = "" }));
        Assert.Throws<HubConfigException>(() => PluginHost.CheckManifest(new FakePlugin { Version = " " }));

        // 声明了自有类型却交不出 descriptor：中台也会拒，但那是网络往返之后的事
        var ex = Assert.Throws<HubConfigException>(() => PluginHost.CheckManifest(
            new FakePlugin { Consumes = ["wms.v1.OrderCreated"] }));
        Assert.Contains("wms.v1.OrderCreated", ex.Message);

        // 只用 well-known 类型 + 空 descriptor 是合法的
        PluginHost.CheckManifest(new FakePlugin());
    }

    /// <summary>
    /// 注册成功后：凭证交给状态客户端、客户端注入给实现了 <see cref="IStateAware"/> 的插件。
    ///
    /// 这三件事（读回执里的 state_token、塞进客户端、注入插件）任何一步漏掉，
    /// 插件拿到的都是一个没有凭证的客户端——而它只在**调用时**才会以 401 的形式露出来。
    /// </summary>
    [Fact]
    public async Task 注册成功后把凭证交给状态客户端并注入给插件()
    {
        var channel = new FakeRegistryChannel { StateTokens = ["tok-1"] };
        var sink = new StringWriter();
        var config = Config(sink);
        var stateChannel = new FakeStateChannel();
        var state = State(stateChannel);
        var plugin = new StateAwarePlugin();
        var registrar = new Registrar(channel, plugin, config, config.Logger!, state, Gateway(state: state));

        await registrar.RegisterOnceAsync(CancellationToken.None);

        var injected = plugin.State();
        Assert.NotNull(injected);
        Assert.Same(state, injected);
        Assert.Equal(1, plugin.Injected);

        // 注入的客户端确实带着刚落库的那张凭证（拿一次真调用去问通道，而不是猜）
        await injected!.PutAsync("s", "k", [], cancellationToken: Ct);
        Assert.Equal("tok-1", Assert.Single(stateChannel.Tokens));
    }

    /// <summary>
    /// 注册成功后：网关客户端同样拿到**同一张**凭证、并注入给实现了
    /// <see cref="IGatewayAware"/> 的插件。与状态客户端同一条规则的另一半——
    /// 漏掉任何一半，插件都要等到调用时才以 401 的形式看见。
    /// </summary>
    [Fact]
    public async Task 注册成功后把凭证交给网关客户端并注入给插件()
    {
        var channel = new FakeRegistryChannel { StateTokens = ["tok-1"] };
        var sink = new StringWriter();
        var config = Config(sink);
        var state = State();
        var gatewayChannel = new FakeGatewayChannel();
        var gateway = new GatewayClient(gatewayChannel, state.Denied, TimeSpan.FromSeconds(1));
        var plugin = new GatewayAwarePlugin();
        var registrar = new Registrar(channel, plugin, config, config.Logger!, state, gateway);

        await registrar.RegisterOnceAsync(CancellationToken.None);

        var injected = plugin.Gateway();
        Assert.NotNull(injected);
        Assert.Same(gateway, injected);
        Assert.Equal(1, plugin.Injected);

        // 注入的客户端确实带着刚下发的那张凭证（网关与状态共用同一张）
        await injected!.ListPluginsAsync(cancellationToken: Ct);
        Assert.Equal("tok-1", Assert.Single(gatewayChannel.Tokens));
    }

    /// <summary>
    /// 重注册后网关客户端的凭证也跟着轮换：插件手里的引用不变，调用自动用上新凭证。
    /// </summary>
    [Fact]
    public async Task 重注册后网关客户端换成新凭证()
    {
        var channel = new FakeRegistryChannel
        {
            ReregisterAtHeartbeat = 2,
            StateTokens = ["tok-1", "tok-2"],
        };
        var sink = new StringWriter();
        var config = Config(sink);
        var state = State();
        var gatewayChannel = new FakeGatewayChannel();
        var gateway = new GatewayClient(gatewayChannel, state.Denied, TimeSpan.FromSeconds(1));
        var plugin = new GatewayAwarePlugin();
        var registrar = new Registrar(channel, plugin, config, config.Logger!, state, gateway);

        using var cts = new CancellationTokenSource();
        var loop = registrar.RunAsync(cts.Token);

        var reRegistered = await TestWait.Until(() => registrar.RegisterCount >= 2, TimeSpan.FromSeconds(10));
        await cts.CancelAsync();
        await loop;

        Assert.True(reRegistered, $"没有自愈：只注册了 {registrar.RegisterCount} 次");
        Assert.Equal(2, plugin.Injected);

        await plugin.Gateway()!.ListPluginsAsync(cancellationToken: Ct);
        Assert.Equal("tok-2", gatewayChannel.Tokens[^1]);
    }

    /// <summary>
    /// 重注册（中台重启 / 实例被摘除后自愈）后凭证会轮换，插件必须拿到**新**的那张，
    /// 而且不该因为「已经有客户端了」就被跳过注入。
    /// </summary>
    [Fact]
    public async Task 重注册会重新注入一次并换成新凭证()
    {
        // 第 2 拍心跳要求重注册；两次注册各下发一张不同的凭证
        var channel = new FakeRegistryChannel
        {
            ReregisterAtHeartbeat = 2,
            StateTokens = ["tok-1", "tok-2"],
        };
        var sink = new StringWriter();
        var config = Config(sink);
        var stateChannel = new FakeStateChannel();
        var state = State(stateChannel);
        var plugin = new StateAwarePlugin();
        var registrar = new Registrar(channel, plugin, config, config.Logger!, state, Gateway(state: state));

        using var cts = new CancellationTokenSource();
        var loop = registrar.RunAsync(cts.Token);

        var reRegistered = await TestWait.Until(() => registrar.RegisterCount >= 2, TimeSpan.FromSeconds(10));
        await cts.CancelAsync();
        await loop;

        Assert.True(reRegistered, $"没有自愈：只注册了 {registrar.RegisterCount} 次");
        Assert.Equal(2, plugin.Injected);
        Assert.Same(state, plugin.State());

        // 旧凭证的客户端引用仍然有效——因为凭证是**客户端里**轮换的，插件手里的引用不变
        await state.PutAsync("s", "k", [], cancellationToken: Ct);
        Assert.Equal("tok-2", stateChannel.Tokens[^1]);
    }

    /// <summary>
    /// 中台没下发凭证（迁移前登记的旧行）时给出警告：此时所有状态调用都会 401，
    /// 不提示的话插件只会看到一句「状态凭证无效」。
    /// </summary>
    [Fact]
    public async Task 中台没下发凭证时给出警告()
    {
        var channel = new FakeRegistryChannel { StateTokens = [""] };
        var sink = new StringWriter();
        var config = Config(sink);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, State(), Gateway());

        await registrar.RegisterOnceAsync(CancellationToken.None);

        Assert.Contains("中台未下发状态凭证，HubState 将不可用", sink.ToString());
    }

    /// <summary>
    /// 状态调用撞上 401 会触发重新注册 —— 这是「凭证被吊销但心跳一切正常」那条死路上的
    /// 唯一出口（中台重启轮换了凭证、实例被摘除后旧凭证失效）。
    /// </summary>
    [Fact]
    public async Task 状态凭证被拒会触发重新注册()
    {
        var channel = new FakeRegistryChannel();
        var sink = new StringWriter();
        // 冷却窗口（= RetryInterval）取小值，让 401 立刻生效
        var config = Config(sink);
        var stateChannel = new FakeStateChannel
        {
            Fail = () => new RpcException(new Status(StatusCode.Unauthenticated, "状态凭证无效或已失效")),
        };
        var state = State(stateChannel);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, state, Gateway(state: state));

        using var cts = new CancellationTokenSource();
        var loop = registrar.RunAsync(cts.Token);

        await TestWait.Until(() => registrar.RegisterCount >= 1, TimeSpan.FromSeconds(10));

        // 先睡过冷却窗口：刚注册就被拒的那次会被速率下限挡掉
        await Task.Delay(TimeSpan.FromMilliseconds(50), Ct);

        var denied = false;
        try
        {
            await state.GetAsync("s", "k", Ct);
        }
        catch (RpcException ex)
        {
            // 错误必须原样交给插件：重注册是后台的补救，不该掩盖这一次失败
            Assert.Equal(StatusCode.Unauthenticated, ex.StatusCode);
            denied = true;
        }

        Assert.True(denied, "假通道应当抛出 Unauthenticated");

        var reRegistered = await TestWait.Until(() => registrar.RegisterCount >= 2, TimeSpan.FromSeconds(10));
        await cts.CancelAsync();
        await loop;

        Assert.True(reRegistered, $"401 之后应重走注册流程，实际只注册了 {registrar.RegisterCount} 次");
        Assert.Contains("状态凭证被拒，重新注册以换取新凭证", sink.ToString());
    }

    /// <summary>
    /// 冷却窗口内的 401 必须被丢掉。没有这条下限，一次成功注册之后紧接着排空信号就是
    /// **零延迟**回到注册——速率等于 Register RPC 的延迟，同时还在反复探测插件自己的地址。
    ///
    /// 窗口取得比任何测试停顿都长（2s），所以「注册之后马上敲一次」在窗口内是确定的。
    /// </summary>
    [Fact]
    public async Task 冷却窗口内的凭证被拒会被忽略()
    {
        var channel = new FakeRegistryChannel();
        var sink = new StringWriter();
        var config = Config(sink);
        config.RetryInterval = TimeSpan.FromSeconds(2);
        var stateChannel = new FakeStateChannel
        {
            Fail = () => new RpcException(new Status(StatusCode.Unauthenticated, "状态凭证无效或已失效")),
        };
        var state = State(stateChannel);
        var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, state, Gateway(state: state));

        using var cts = new CancellationTokenSource();
        var loop = registrar.RunAsync(cts.Token);

        await TestWait.Until(() => registrar.RegisterCount >= 1, TimeSpan.FromSeconds(10));

        // 注册之后立刻敲（此刻距上次注册只有毫秒级，远在冷却窗口内）
        await Assert.ThrowsAsync<RpcException>(() => state.GetAsync("s", "k", CancellationToken.None));

        // 给「没有冷却」的实现足够的时间把第 2 次注册发出来（Register 是本地 RPC，毫秒级）
        await Task.Delay(TimeSpan.FromMilliseconds(300), Ct);

        await cts.CancelAsync();
        await loop;

        Assert.Equal(1, channel.RegisterCalls);
        Assert.Contains("距上次注册不足冷却窗口，忽略本次", sink.ToString());
    }

    /// <summary>
    /// 401 之外的失败**不能**触发重注册：网络抖动、参数非法、超时都跟凭证无关，
    /// 重注册解决不了，反而会让注册循环空转（中台一慢就变成重注册风暴）。
    /// </summary>
    [Fact]
    public async Task 超时与参数错误不会触发重新注册()
    {
        foreach (var code in new[] { StatusCode.DeadlineExceeded, StatusCode.InvalidArgument })
        {
            var channel = new FakeRegistryChannel();
            var sink = new StringWriter();
            var config = Config(sink);
            var stateChannel = new FakeStateChannel
            {
                Fail = () => new RpcException(new Status(code, "与凭证无关")),
            };
            var state = State(stateChannel);
            var registrar = new Registrar(channel, new FakePlugin(), config, config.Logger!, state, Gateway(state: state));

            using var cts = new CancellationTokenSource();
            var loop = registrar.RunAsync(cts.Token);

            await TestWait.Until(() => registrar.RegisterCount >= 1, TimeSpan.FromSeconds(10));
            await Task.Delay(TimeSpan.FromMilliseconds(50), Ct);

            await Assert.ThrowsAsync<RpcException>(() => state.GetAsync("s", "k", CancellationToken.None));
            await Task.Delay(TimeSpan.FromMilliseconds(200), Ct);

            await cts.CancelAsync();
            await loop;

            Assert.Equal(1, channel.RegisterCalls);
            Assert.DoesNotContain("状态凭证被拒", sink.ToString());
        }
    }
}
