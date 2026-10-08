using Google.Protobuf;
using Hub.V1;

namespace HubKit;

/// <summary>
/// 插件 → 中台那条链路。
///
/// 抽成接口是为了让「注册 → 心跳 → 被摘除则重新注册」这套编排**可被单测证明**：
/// 拿一个真中台去测重注册，得先把实例从库里摘掉，而那是中台的内部状态。
/// 假实现能让三个响应各出现一次，把整条路径在一次测试里走完。
/// </summary>
public interface IRegistryChannel
{
    Task<RegisterResponse> RegisterAsync(RegisterRequest request, CancellationToken cancellationToken);

    Task<HeartbeatResponse> HeartbeatAsync(HeartbeatRequest request, CancellationToken cancellationToken);

    Task<UnregisterResponse> UnregisterAsync(UnregisterRequest request, CancellationToken cancellationToken);
}

/// <summary>真的连中台。<see cref="PluginHost"/> 缺省用它。</summary>
public sealed class GrpcRegistryChannel(PluginRegistry.PluginRegistryClient client) : IRegistryChannel
{
    public Task<RegisterResponse> RegisterAsync(RegisterRequest request, CancellationToken cancellationToken) =>
        client.RegisterAsync(request, cancellationToken: cancellationToken).ResponseAsync;

    public Task<HeartbeatResponse> HeartbeatAsync(HeartbeatRequest request, CancellationToken cancellationToken) =>
        client.HeartbeatAsync(request, cancellationToken: cancellationToken).ResponseAsync;

    public Task<UnregisterResponse> UnregisterAsync(UnregisterRequest request, CancellationToken cancellationToken) =>
        client.UnregisterAsync(request, cancellationToken: cancellationToken).ResponseAsync;
}

/// <summary>
/// 维持「注册 → 心跳 → 被摘除则重新注册」的循环。
///
/// 四个刻意的行为：
/// <list type="bullet">
/// <item>**注册会一直重试**：中台可能比插件晚起来，插件先启动是常态。</item>
/// <item>**被摘除后自动重新注册**：心跳响应里带 <c>reregister_required</c> 时
/// 重走注册流程，这是实例掉线后能自愈的关键。</item>
/// <item>**心跳失败不停止心跳**：网络抖一下就让插件哑掉，比不心跳更糟。</item>
/// <item>**状态凭证失效后自动重新注册**：HubState 调用收到 <c>Unauthenticated</c> 时
/// 重走注册流程换一张新凭证（中台重启会轮换凭证、实例被摘除后旧凭证即失效）。
/// 这是防御层而不是主恢复路径——凭证落了库，中台重启对插件本来是透明的。</item>
/// </list>
/// </summary>
public sealed class Registrar(
    IRegistryChannel channel,
    IPlugin plugin,
    HubConfig config,
    HubLogger log,
    StateClient state,
    GatewayClient gateway)
{
    private readonly object _gate = new();
    private TimeSpan _interval = HubConfig.HeartbeatFallbackInterval;
    private DateTime _lastRegisterAt = DateTime.MinValue;

    /// <summary>中台指定的心跳周期；中台没说时用兜底值。</summary>
    public TimeSpan Interval
    {
        get
        {
            lock (_gate)
            {
                return _interval;
            }
        }
    }

    /// <summary>已经成功注册过几次。测试用它判断「重注册真的发生了」。</summary>
    public int RegisterCount { get; private set; }

    /// <summary>循环直到 <paramref name="cancellationToken"/> 结束。</summary>
    public async Task RunAsync(CancellationToken cancellationToken)
    {
        while (!cancellationToken.IsCancellationRequested)
        {
            try
            {
                await RegisterOnceAsync(cancellationToken);
            }
            catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
            {
                return;
            }
            catch (Exception ex)
            {
                LogRegisterFailure(ex);
                if (!await SleepAsync(config.RetryInterval, cancellationToken))
                {
                    return;
                }

                continue;
            }

            await HeartbeatLoopAsync(cancellationToken);
        }
    }

    /// <summary>
    /// 注册一次。失败抛异常：拒绝是 <see cref="RegistrationRejectedException"/>，
    /// 连不上中台之类是底层异常。
    /// </summary>
    public async Task RegisterOnceAsync(CancellationToken cancellationToken)
    {
        var manifest = plugin.Manifest;

        var response = await channel.RegisterAsync(
            new RegisterRequest
            {
                PluginName = manifest.Name,
                Version = manifest.Version,
                InstanceId = config.InstanceId,
                AdvertiseAddr = config.AdvertiseAddr,
                Manifest = manifest,
                DescriptorSet = ByteString.CopyFrom(plugin.Descriptor ?? []),
            },
            cancellationToken);

        if (!response.Accepted)
        {
            throw new RegistrationRejectedException(response.Rejections);
        }

        RegisterCount++;

        // 记下成功注册的时刻：「状态凭证被拒」触发的强制重注册靠它做速率下限（见 HeartbeatLoopAsync）
        lock (_gate)
        {
            _lastRegisterAt = DateTime.UtcNow;
        }

        // 凭证随每次注册轮换，这里覆盖旧的。
        //
        // **这一份同时就是注销时的属主证明**——`RegisterResponse.state_token` 与
        // `UnregisterRequest.state_token` 是同一个东西：中台靠它认出「你要摘的是不是
        // 自己那一行」（`instance_id` 是插件自报的、可以跨插件相撞）。所以注销直接读
        // <see cref="StateClient"/> 里这一份，`Registrar` 不另存字段：同一个凭证存两处，
        // 迟早有一处忘了跟着轮换，而那种错只在「注销没摘掉行」时才露出来。
        state.SetToken(response.StateToken);
        gateway.SetToken(response.StateToken);
        if (response.StateToken.Length == 0)
        {
            log.Warn("中台未下发状态凭证，HubState 将不可用");
        }

        // 每次注册后都注入一次：插件可能还持有上一个凭证时期的客户端引用
        if (plugin is IStateAware aware)
        {
            aware.SetState(state);
        }

        if (plugin is IGatewayAware gatewayAware)
        {
            gatewayAware.SetGateway(gateway);
        }

        if (response.HeartbeatIntervalSeconds > 0)
        {
            lock (_gate)
            {
                _interval = TimeSpan.FromSeconds(response.HeartbeatIntervalSeconds);
            }
        }

        foreach (var warning in response.Warnings)
        {
            log.Warn("中台提示", ("warning", warning));
        }

        log.Info(
            "已注册到中台",
            ("plugin", manifest.Name),
            ("version", manifest.Version),
            ("instance", response.InstanceId));
    }

    /// <summary>
    /// 按中台指定的周期续期。**返回即表示需要重新注册**（或 token 已取消）。
    ///
    /// 触发返回的有三条：中台要求重注册、心跳被拒、以及插件侧的状态调用撞上 401。
    /// 最后一条受 <see cref="HubConfig.RetryInterval"/> 这个冷却窗口约束——窗口内到达的
    /// 401 被忽略，免得持续被拒时把注册循环打成无限自旋。
    /// </summary>
    private async Task HeartbeatLoopAsync(CancellationToken cancellationToken)
    {
        var interval = config.HeartbeatIntervalOverride ?? Interval;

        // 这一轮心跳的取消尺：出口处统一取消，放掉还挂在「凭证被拒」上的那次等待
        using var waits = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);

        // 挂着的等待**跨迭代保留**。心跳那一拍赢的时候不能把它扔掉：被扔掉的等待一旦完成
        // 就会把那个 401 吃掉（DeniedSignal 的取消路径才不消费信号），于是「该重注册却没重注册」。
        // 只在方法出口取消它——那时要么信号已经没用（正要重注册），要么留给下一轮。
        Task<bool>? denial = null;

        try
        {
            while (true)
            {
                var tick = Task.Delay(interval, waits.Token);
                denial ??= state.Denied.WaitAsync(waits.Token);

                var winner = await Task.WhenAny(tick, denial);

                if (winner == denial)
                {
                    var refused = await denial;
                    denial = null;

                    if (!refused)
                    {
                        // 等待被取消：只有整个循环结束（token 结束）时才会走到这里
                        return;
                    }

                    // 插件侧的状态调用被中台判了 401：凭证多半已被吊销或轮换。
                    // 心跳本身可能一切正常（实例还在库里），不重注册就会一直哑下去。
                    //
                    // 但必须有速率下限：401 可能连续不断（中台校验滞后、撤销尚未传播，
                    // 或非凭证原因也回 401）。没有它，一次成功注册之后紧接着排空信号
                    // 就是零延迟，注册速率等于 Register RPC 的延迟——无限自旋，
                    // 同时还在反复探测插件自己的地址。窗口内到达的直接丢掉。
                    var since = SinceLastRegister();
                    if (since < config.RetryInterval)
                    {
                        log.Warn(
                            "状态凭证被拒，但距上次注册不足冷却窗口，忽略本次",
                            ("since", Duration(since)),
                            ("cooldown", Duration(config.RetryInterval)));
                        continue;
                    }

                    log.Warn("状态凭证被拒，重新注册以换取新凭证");
                    return;
                }

                try
                {
                    await tick;
                }
                catch (OperationCanceledException)
                {
                    return;
                }

                HeartbeatResponse response;
                try
                {
                    response = await channel.HeartbeatAsync(
                        new HeartbeatRequest { InstanceId = config.InstanceId },
                        cancellationToken);
                }
                catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
                {
                    return;
                }
                catch (Exception ex)
                {
                    // 网络抖动不该让插件停止心跳，下一拍继续
                    log.Warn("心跳失败", ("err", ex.Message));
                    continue;
                }

                if (!response.Accepted || response.ReregisterRequired)
                {
                    // 中台认不出这个实例了：可能是被摘除（心跳超时、人工摘除），
                    // 也可能是中台重启过。两条都靠重走注册流程收敛。
                    log.Warn("中台要求重新注册（实例可能已被摘除）");
                    return;
                }
            }
        }
        finally
        {
            // 取消（而不是丢弃）挂着的等待：取消路径不消费信号，
            // 下一个心跳循环再等时还能把它拿到
            waits.Cancel();
        }
    }

    private TimeSpan SinceLastRegister()
    {
        lock (_gate)
        {
            return DateTime.UtcNow - _lastRegisterAt;
        }
    }

    /// <summary>
    /// 主动注销。中台据此立刻摘掉实例，不必等心跳超时。
    ///
    /// **手里没有凭证就整个跳过**：凭证只在注册成功时下发，空串说明本实例压根没进过注册表
    /// （注册被拒、或还没注册上就退出了），没有实例行可摘除。而 <c>instance_id</c> 是插件
    /// 自己生成的、可以跟别的插件撞——缺省「主机名-PID」，同一 host 网络下容器 PID 又都是 1，
    /// 撞是必然。这一发不带身份的注销若被中台按 <c>instance_id</c> 删行，删掉的正是**对方**
    /// 那一行，而对方的心跳仍按 <c>instance_id</c> 命中、返回 accepted，**完全察觉不到自己
    /// 从注册表里消失了**（实测：auth 重启一次，sql-executor 的工具从 MCP 工具面上全部消失，
    /// 而它自己的日志停在「已注册到中台」之后再无输出）。中台侧现在会拒
    /// （见 <c>UnregisterRequest.state_token</c>），插件侧这一道是别发这个注定被拒的请求。
    ///
    /// 这也决定了**插件先于中台升级**时的行为：旧中台的 <c>RegisterResponse</c> 没有这个
    /// 字段，这里的凭证就是空串，于是注销整个跳过——对旧中台不再有优雅退出，只能等它心跳
    /// 超时摘除。这个窗口是有界的（中台升级完就恢复），而反过来放行的代价是可能删掉别人的
    /// 实例行。
    /// </summary>
    public async Task UnregisterAsync(string reason = "插件优雅退出")
    {
        var token = state.CurrentToken();
        if (token.Length == 0)
        {
            log.Info("本实例没有注册凭证，跳过主动注销（没注册成功就没有可摘除的实例行）");
            return;
        }

        try
        {
            using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(3));
            await channel.UnregisterAsync(
                new UnregisterRequest
                {
                    InstanceId = config.InstanceId,
                    Reason = reason,
                    StateToken = token,
                },
                timeout.Token);
        }
        catch (Exception ex)
        {
            // 注销失败不是错误路径：中台会在心跳超时后自行摘除
            log.Warn("注销失败（中台会在心跳超时后自行摘除）", ("err", ex.Message));
        }
    }

    /// <summary>
    /// 把一次注册失败讲成「看一眼就懂」的样子。
    ///
    /// 中台拒绝时给的是结构化原因，这里让**每条原因各占一行**。日志是 JSON、一条记录
    /// 一行，一整段多行文本会被转义成 <c>\n</c> 塞进单个字段——一行里糊着 N 条原因，
    /// 得靠人脑反解析才看得出哪条是哪条。拆成 code / message / detail 三列之后，
    /// 每行本身就是完整的一条，读日志不需要 jq，也不需要任何工具。
    ///
    /// 重试间隔单独收尾一行，而不是跟在每条原因后面：它是「接下来会怎样」，
    /// 与「错在哪」不是一回事，逐条重复只会把原因行淹掉。reasons 是原因条数，
    /// 用来兜底——日志被截断时，一眼能看出还有几条没打出来。
    /// </summary>
    private void LogRegisterFailure(Exception error)
    {
        if (error is RegistrationRejectedException rejected && rejected.Rejections.Count > 0)
        {
            foreach (var rejection in rejected.Rejections)
            {
                log.Error(
                    "中台拒绝了注册",
                    ("code", RejectCodes.Name(rejection.Code)),
                    ("message", rejection.Message),
                    ("detail", rejection.Detail));
            }

            log.Error(
                "注册未通过，稍后重试",
                ("reasons", rejected.Rejections.Count),
                ("retry_in", Duration(config.RetryInterval)));
            return;
        }

        // 连不上中台、网络抖动这类错误本身就是单行的，和重试间隔打在同一行里正好，
        // 别为了跟上面的格式统一把它拆开——拆开只会多出一行没有信息量的收尾
        log.Error(
            "注册未通过，稍后重试",
            ("err", error.Message),
            ("retry_in", Duration(config.RetryInterval)));
    }

    /// <summary>
    /// 时长的**人话**形式。
    ///
    /// Go 侧踩过这个：直接把 Duration 落进 JSON 会得到纳秒整数 5000000000，
    /// 没人认得出那是 5 秒。这里统一成 <c>"5s"</c> / <c>"20ms"</c>。
    /// </summary>
    internal static string Duration(TimeSpan d) =>
        d.TotalSeconds >= 1 ? $"{d.TotalSeconds:0.###}s" : $"{d.TotalMilliseconds:0.###}ms";

    private static async Task<bool> SleepAsync(TimeSpan delay, CancellationToken cancellationToken)
    {
        try
        {
            await Task.Delay(delay, cancellationToken);
            return true;
        }
        catch (OperationCanceledException)
        {
            return false;
        }
    }
}
