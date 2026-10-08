using Google.Protobuf;
using Grpc.Core;
using Hub.V1;

namespace HubKit;

/// <summary>
/// 插件 → PluginGateway 那条链路（插件间发现与互调）。
///
/// 抽成接口的理由与 <see cref="IStateChannel"/> 同一个：让「凭证怎么带、撞上 401
/// 会怎样」这些**行为**能被单测证明，而不是只能靠一个真中台去验。
/// </summary>
public interface IGatewayChannel
{
    Task<ListPluginsResponse> ListPluginsAsync(ListPluginsRequest request, CallOptions options);

    Task<DescribeMessageResponse> DescribeMessageAsync(DescribeMessageRequest request, CallOptions options);

    Task<GetContractResponse> GetContractAsync(GetContractRequest request, CallOptions options);

    Task<InvokeResponse> InvokeAsync(InvokeRequest request, CallOptions options);
}

/// <summary>真的连中台。与 <see cref="GrpcStateChannel"/> 一样只做转发。</summary>
public sealed class GrpcGatewayChannel(PluginGateway.PluginGatewayClient client) : IGatewayChannel
{
    public Task<ListPluginsResponse> ListPluginsAsync(ListPluginsRequest request, CallOptions options) =>
        client.ListPluginsAsync(request, options).ResponseAsync;

    public Task<DescribeMessageResponse> DescribeMessageAsync(DescribeMessageRequest request, CallOptions options) =>
        client.DescribeMessageAsync(request, options).ResponseAsync;

    public Task<GetContractResponse> GetContractAsync(GetContractRequest request, CallOptions options) =>
        client.GetContractAsync(request, options).ResponseAsync;

    public Task<InvokeResponse> InvokeAsync(InvokeRequest request, CallOptions options) =>
        client.InvokeAsync(request, options).ResponseAsync;
}

/// <summary>
/// 由需要发现/互调能力的插件实现。
///
/// 用可选接口而不是往 <see cref="IPlugin"/> 里加方法：老插件一行不用改，新插件想用才实现。
/// </summary>
public interface IGatewayAware
{
    /// <summary>
    /// 在**每次**注册成功后由骨架调用（首次注册、以及自愈重注册）——凭证随每次注册轮换，
    /// 注入的永远是同一个客户端对象、最新的凭证。实现方**不要**把这个引用长期拷进别处，
    /// 也不该缓存凭证。
    ///
    /// 与 <see cref="IStateAware"/> 同一条提醒：骨架从注册循环那个任务里调用它，而插件的
    /// handler 通常跑在别的任务上，所以**同步是实现方的责任**。
    /// </summary>
    void SetGateway(GatewayClient gateway);
}

/// <summary>
/// 互调业务失败的共同基类（<c>outcome=REJECTED</c> / <c>ERROR</c>）。
///
/// 想对两类失败做同一件事（记日志、退避）就抓它；想区分处理就抓子类。
/// 原因放在 <see cref="Reason"/> 属性上，不塞进异常消息——消息只剩给人看的份，
/// 进了属性才能被程序分支（按 reason 决定「改数据重试」还是「改逻辑」）。
/// </summary>
public abstract class InvokeFailureException : Exception
{
    /// <summary>
    /// <paramref name="kind"/> 是 proto 的 <c>InvokeOutcome</c> 枚举名（REJECTED / ERROR）。
    /// 日志与文档里用的是 proto 名而不是 C# 枚举名，与 <see cref="RejectCodes"/> 同一个道理。
    /// </summary>
    protected InvokeFailureException(string reason, string kind)
        : base(reason.Length == 0
            ? $"hubkit: 插件互调失败（{kind}，未给出原因）"
            : $"hubkit: 插件互调失败（{kind}）：{reason}")
    {
        Reason = reason;
    }

    /// <summary>中台给的人类可读原因；可能为空串。</summary>
    public string Reason { get; }
}

/// <summary>
/// 目标插件的校验器拒绝了这次调用（<c>outcome=REJECTED</c>）。
///
/// <see cref="Issues"/> 是结构化的校验问题，path 能定位到具体字段——调用方照着改数据
/// 即可，不必去翻下游日志。数据不变的重发只会得到同一批 issues。
/// </summary>
public sealed class InvokeRejectedException(string reason, IEnumerable<ValidationIssue> issues)
    : InvokeFailureException(reason, "REJECTED")
{
    /// <summary>目标插件 Validate 的结构化校验问题。</summary>
    public IReadOnlyList<ValidationIssue> Issues { get; } = [.. issues];

    /// <summary>issues 摊进消息里：日志里能直接看到是哪个字段没过，不必先翻属性。</summary>
    public override string Message
    {
        get
        {
            var head = base.Message;
            if (Issues.Count == 0)
            {
                return head;
            }

            var detail = string.Join("；", Issues.Select(i => $"{i.Path}: {i.Message}"));
            return $"{head}（issues: {detail}）";
        }
    }
}

/// <summary>
/// 这次互调被中台拦下或下游出错（<c>outcome=ERROR</c>）。
///
/// 覆盖的情形（<see cref="InvokeFailureException.Reason"/> 里会说明是哪种）：未声明调用
/// 授权、分钟级配额打满、互调成环、链深超限、目标插件不存在 / 没有健康实例 / 超时。
/// 「没有健康实例」值得退避重试，「未声明授权」重试没有意义——两类靠 reason 区分，
/// 这正是它们不走 gRPC 错误通道的原因。
/// </summary>
public sealed class InvokeFailedException(string reason) : InvokeFailureException(reason, "ERROR");

/// <summary>
/// <see cref="GatewayClient.InvokePluginAsync"/> 的入参。
/// </summary>
public sealed class InvokeOptions
{
    /// <summary>目标插件的版本，空 = 最新已注册版本。</summary>
    public string? Version { get; set; }

    /// <summary>
    /// 本次调用的超时预算，<see cref="TimeSpan.Zero"/> 表示不填——由中台给默认预算兜底。
    ///
    /// 它会同时落到两处：信封的 deadline（与 <see cref="CurrentEnvelope"/> 的 deadline
    /// 取较早者，中台侧还会再夹一次）与请求的 <c>timeout_ms</c>。
    /// </summary>
    public TimeSpan Timeout { get; set; }

    /// <summary>
    /// 插件当前正在处理的信封（可空）。
    ///
    /// 传入它，trace 才能从上游贯通到下游：其 trace_id / run_id / node_id 会被复制进
    /// 新信封，meta 里的调用链（<see cref="GatewayClient.CallChainMeta"/>）原样带上——
    /// **不追加自己**，中台负责把 caller 记进链。不传则生成全新 trace_id、不带链：
    /// 那是一次顶层发起的调用，不是当前处理的延续。
    /// </summary>
    public Envelope? CurrentEnvelope { get; set; }

    /// <summary>
    /// JSON 对象载荷（直接调用场景），与 <see cref="Envelopes.PayloadJson"/> 同读法。
    /// 与 <see cref="Payload"/> 二选一，都空则发不带载荷的信封。
    /// </summary>
    public IDictionary<string, object?>? PayloadJson { get; set; }

    /// <summary>业务 proto 消息载荷（flow 内部传递语义），type_url 由消息的全限定名拼出。</summary>
    public IMessage? Payload { get; set; }
}

/// <summary>
/// 插件间发现与互调（PluginGateway 服务）的客户端。
///
/// 与 <see cref="StateClient"/> 同款：由宿主在**注册成功后**注入给实现了
/// <see cref="IGatewayAware"/> 的插件——凭证就是状态凭证（中台用同一张
/// <c>x-hub-state-token</c> 认两件事：状态面的「这条键值是谁写的」与网关面的
/// 「这次互调是谁发起的」），随每次重新注册轮换，所以 token 的读写都过锁；
/// 插件作者不该自己构造它。
///
/// 与 <see cref="StateClient"/> 的一点刻意差别：**Invoke 不吃 callTimeout**。状态调用
/// 是毫秒级的管面操作，卡 2 秒就该放弃；互调是业务调用，预算就是信封的 deadline——
/// 下游真的处理 20 秒时，客户端先超时等于让下游白干。所以发现三件套用 callTimeout
/// 截断，Invoke 的超时完全由信封预算决定。
/// </summary>
public sealed class GatewayClient
{
    /// <summary>
    /// 互调链的 meta 键：链上是「已处理过该消息的插件名」，逗号分隔。
    ///
    /// 信封的 meta 是唯一的自由携带通道，链只能随它走。事实源是
    /// <c>testdata/hub-rules.json</c> 的 <c>gateway.callChainMeta</c>（有测试钉住一致），
    /// 与中台 <c>crates/hub-grpc/src/gateway.rs</c> 的 <c>CALL_CHAIN_META</c> 同名。
    ///
    /// 经 <see cref="InvokePluginAsync"/> 发起调用时**不要**自己往链里追加自己：
    /// 链的语义由中台维护，SDK 只负责把调用方收到的链原样带下去。
    /// </summary>
    public const string CallChainMeta = "hub.call_chain";

    /// <summary>
    /// 互调链的长度上限（含本次 caller）。
    ///
    /// SDK 侧不主动校验它——链的校验在中台（超了会以 outcome=ERROR 拒绝）——
    /// 导出它只为插件自测能对齐同一个数。事实源同 <see cref="CallChainMeta"/>。
    /// </summary>
    public const int MaxInvokeDepth = 8;

    private readonly IGatewayChannel _channel;
    private readonly TimeSpan _callTimeout;
    private readonly DeniedSignal _denied;

    private readonly object _gate = new();
    private string _token = string.Empty;

    /// <param name="channel">与中台之间那条链路。</param>
    /// <param name="callTimeout">
    /// **发现类**调用的时间上限，缺省与 <see cref="StateClient.DefaultCallTimeout"/> 同值。
    /// 互调不走它——互调的预算是调用方每次给的信封 deadline。
    /// </param>
    public GatewayClient(IGatewayChannel channel, TimeSpan callTimeout = default)
        : this(channel, denied: null, callTimeout)
    {
    }

    /// <summary>
    /// 宿主装配用：互调撞上 401 与状态调用撞上 401 是**同一件事**（同一张凭证），
    /// 所以共享 <see cref="StateClient"/> 那个「凭证被拒」信号，注册循环不用开第二个等待口。
    /// </summary>
    internal GatewayClient(IGatewayChannel channel, DeniedSignal? denied, TimeSpan callTimeout)
    {
        _channel = channel;
        _denied = denied ?? new DeniedSignal();
        _callTimeout = callTimeout > TimeSpan.Zero ? callTimeout : StateClient.DefaultCallTimeout;
    }

    /// <summary>「凭证被拒」的信号口。与 <see cref="StateClient"/> 共享同一个（见内部构造函数）。</summary>
    internal DeniedSignal Denied => _denied;

    /// <summary>记下中台本次下发的凭证。由注册流程调用，**不对外**——插件作者不该自己管凭证。</summary>
    internal void SetToken(string? token)
    {
        lock (_gate)
        {
            _token = token ?? string.Empty;
        }
    }

    /// <summary>当前凭证。空串表示这次注册没拿到凭证。语义见 <see cref="StateClient"/> 的同名约定。</summary>
    internal string CurrentToken()
    {
        lock (_gate)
        {
            return _token;
        }
    }

    /// <summary>
    /// 在线插件清单。缺省只列有健康实例的；<paramref name="includeOffline"/> 置 true 时附上离线插件。
    ///
    /// 插件用它回答「能调谁」，不要旁路维护硬编码名单——那会跟注册表漂移。
    /// </summary>
    public async Task<IReadOnlyList<PluginSummary>> ListPluginsAsync(
        bool includeOffline = false,
        CancellationToken cancellationToken = default)
    {
        var response = await CallAsync(
            DiscoveryOptions(cancellationToken),
            options => _channel.ListPluginsAsync(
                new ListPluginsRequest { IncludeOffline = includeOffline },
                options)).ConfigureAwait(false);

        return response.Plugins.ToList();
    }

    /// <summary>
    /// 查一个消息类型由谁生产、由谁消费。
    ///
    /// <paramref name="fqName"/> 的口径与 manifest 契约里的 fq_name 一致（如 <c>wms.v1.OrderCreated</c>）。
    /// 两边都查无时返回空响应而不是报错：「没人生产/消费这个消息」本身就是有效答案。
    /// </summary>
    public Task<DescribeMessageResponse> DescribeMessageAsync(
        string fqName,
        CancellationToken cancellationToken = default)
    {
        return CallAsync(
            DiscoveryOptions(cancellationToken),
            options => _channel.DescribeMessageAsync(
                new DescribeMessageRequest { FqName = fqName },
                options));
    }

    /// <summary>
    /// 查一个插件的契约 + 字段级 schema。
    ///
    /// <paramref name="version"/> 传空取最新已注册版本；<paramref name="fqName"/> 传空
    /// 不展开单个消息的字段级 schema（<c>schema_json</c> 为空串），要看某个消息吃什么
    /// 就把它传进来。查无插件或版本是 <see cref="StatusCode.NotFound"/>——与发现不同，
    /// 「要调的对象不存在」对调用方是异常而不是空答案。
    /// </summary>
    public Task<GetContractResponse> GetContractAsync(
        string plugin,
        string? version = null,
        string? fqName = null,
        CancellationToken cancellationToken = default)
    {
        return CallAsync(
            DiscoveryOptions(cancellationToken),
            options => _channel.GetContractAsync(
                new GetContractRequest { Plugin = plugin, Version = version ?? string.Empty, FqName = fqName ?? string.Empty },
                options));
    }

    /// <summary>
    /// 发起一次同步互调的底层形态：信封由调用方完整构造，原样返回中台的应答。
    ///
    /// 业务结果（HANDLED/REJECTED/ERROR）全在应答字段里，本方法不做映射——要
    /// 「HANDLED 给信封、其余给类型化异常」的便捷语义请用 <see cref="InvokePluginAsync"/>。
    ///
    /// 与发现三件套不同，这里**不套 callTimeout**（理由见类注释）：预算由信封的
    /// deadline 表达，中台会把它夹到 <c>min(传入 deadline, now + timeout_ms)</c>。
    /// </summary>
    public Task<InvokeResponse> InvokeAsync(InvokeRequest request, CancellationToken cancellationToken = default)
    {
        return CallAsync(Options(deadline: null, cancellationToken), options => _channel.InvokeAsync(request, options));
    }

    /// <summary>
    /// 发起一次同步互调的便捷入口：装配信封（幂等键、trace 上下文、调用链、deadline）、
    /// 打包载荷、调用中台、把业务结果映射成类型化异常。
    ///
    /// 结果语义：
    /// <list type="bullet">
    /// <item>HANDLED → 返回下游的信封（载荷用 <see cref="Envelopes.PayloadJson"/> 取）；</item>
    /// <item>REJECTED → <see cref="InvokeRejectedException"/>（reason + issues）；</item>
    /// <item>ERROR → <see cref="InvokeFailedException"/>（reason）；</item>
    /// <item>基础设施故障（未鉴权、中台不可达）→ 原样抛 <see cref="RpcException"/>。</item>
    /// </list>
    /// </summary>
    public async Task<Envelope> InvokePluginAsync(
        string plugin,
        InvokeOptions? options = null,
        CancellationToken cancellationToken = default)
    {
        options ??= new InvokeOptions();

        // 参数问题在本地就讲清楚，省一趟注定失败的往返；这些不是「中台判定的错」，
        // 所以不走 RpcException，也不必新造一个异常类型
        if (string.IsNullOrWhiteSpace(plugin))
        {
            throw new ArgumentException("缺少目标插件名（plugin）", nameof(plugin));
        }

        if (options.PayloadJson is not null && options.Payload is not null)
        {
            throw new ArgumentException("PayloadJson 与 Payload 只能二选一", nameof(options));
        }

        if (options.Timeout < TimeSpan.Zero)
        {
            throw new ArgumentException("Timeout 不能为负", nameof(options));
        }

        // 线上协议的 timeout_ms 是 uint32 毫秒：塞不下的预算在这里就讲清楚，
        // 别等 gRPC 序列化时变成一个莫名其妙的回绕值
        if (options.Timeout > TimeSpan.FromMilliseconds(uint.MaxValue))
        {
            throw new ArgumentException(
                $"Timeout 超出线上协议上限（uint32 毫秒）：{options.Timeout}", nameof(options));
        }

        var envelope = new Envelope
        {
            // message_id 是幂等键，每次调用都要是新的——哪怕复用同一个 payload
            MessageId = Envelopes.NewUlid(),
            Type = PayloadType.Request,
        };

        var deadlineMs = 0L;
        if (options.CurrentEnvelope is { } current)
        {
            envelope.TraceId = current.TraceId;
            envelope.RunId = current.RunId;
            envelope.NodeId = current.NodeId;

            // 链原样复制，不追加自己：链上记的是「已处理过该消息的插件」，
            // 追加 caller 是中台的事，SDK 抢着做会让链上出现重复节点
            if (current.Meta.TryGetValue(CallChainMeta, out var chain) && chain.Length > 0)
            {
                envelope.Meta.Add(CallChainMeta, chain);
            }

            // 整体预算比本次预算更早时以它为准——调用不该活得比触发它的那次处理更久
            deadlineMs = current.DeadlineMs;
        }

        if (envelope.TraceId.Length == 0)
        {
            envelope.TraceId = Envelopes.NewUlid();
        }

        if (options.Timeout > TimeSpan.Zero)
        {
            // 中台会取 min(传入 deadline, now+timeout)，客户端先夹一次能更早放弃
            var ceiling = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() + (long)options.Timeout.TotalMilliseconds;
            if (deadlineMs <= 0 || ceiling < deadlineMs)
            {
                deadlineMs = ceiling;
            }
        }

        envelope.DeadlineMs = deadlineMs;

        if (options.PayloadJson is not null)
        {
            envelope = Envelopes.WithPayloadJson(envelope, options.PayloadJson);
        }
        else if (options.Payload is not null)
        {
            envelope = Envelopes.WithPayload(envelope, options.Payload);
        }

        // 信封预算就是本次 gRPC 调用的预算：到点客户端先放弃，而不是等中台或下游超时。
        // 已经过期的预算就不再设 deadline 了——让「必然被中台拒」的应答能带 reason 回来，
        // 比一句干巴巴的 DeadlineExceeded 多一条排查线索
        DateTime? rpcDeadline = null;
        if (envelope.DeadlineMs > 0)
        {
            var budget = DateTimeOffset.FromUnixTimeMilliseconds(envelope.DeadlineMs);
            if (budget > DateTimeOffset.UtcNow)
            {
                rpcDeadline = budget.UtcDateTime;
            }
        }

        var response = await CallAsync(
            Options(rpcDeadline, cancellationToken),
            callOptions => _channel.InvokeAsync(
                new InvokeRequest
                {
                    Plugin = plugin,
                    Version = options.Version ?? string.Empty,
                    Envelope = envelope,
                    TimeoutMs = (uint)options.Timeout.TotalMilliseconds,
                },
                callOptions)).ConfigureAwait(false);

        return response.Outcome switch
        {
            // 中台违约（协议上 HANDLED 必带信封）：返回一个空信封会让调用方把
            // 「没有结果」当成「结果是空的」用下去，宁可在这里显式炸出来
            InvokeOutcome.Handled when response.Envelope is not null => response.Envelope,
            InvokeOutcome.Handled => throw new InvokeFailedException("中台返回 HANDLED 但未携带下游信封"),
            InvokeOutcome.Rejected => throw new InvokeRejectedException(response.Reason, response.Issues),
            InvokeOutcome.Error => throw new InvokeFailedException(response.Reason),
            // UNSPECIFIED 只可能来自不按契约实现的假中台；按未知错误处理而不是猜
            _ => throw new InvokeFailedException($"中台返回了未知的结果分类 {(int)response.Outcome}"),
        };
    }

    /// <summary>
    /// 一次调用：带上凭证与时间上限，并把「凭证被拒」讲给注册循环（共享
    /// <see cref="StateClient"/> 那个信号）。失败**原样抛出**——业务拒绝该改逻辑、
    /// 网络故障该重试或上报，都不该被客户端掩盖。
    /// </summary>
    private async Task<TResponse> CallAsync<TResponse>(
        CallOptions callOptions,
        Func<CallOptions, Task<TResponse>> call)
    {
        try
        {
            return await call(callOptions).ConfigureAwait(false);
        }
        catch (RpcException ex)
        {
            NoteDenied(ex);
            throw;
        }
    }

    /// <summary>
    /// 发现类调用的 metadata 与时间上限，语义与 <see cref="StateClient"/> 的同名约定一致：
    /// 没有凭证时**不发**这个头；超时走 <c>Deadline</c> 而不是自己 <c>CancelAfter</c>；
    /// 调用方的 <see cref="CancellationToken"/> 原样透传。
    /// </summary>
    private CallOptions DiscoveryOptions(CancellationToken cancellationToken) =>
        Options(
            DateTime.MaxValue - _callTimeout > DateTime.UtcNow
                ? DateTime.UtcNow.Add(_callTimeout)
                : null,
            cancellationToken);

    private CallOptions Options(DateTime? deadline, CancellationToken cancellationToken)
    {
        var token = CurrentToken();

        var headers = new Metadata();
        if (token.Length > 0)
        {
            headers.Add(StateClient.TokenMetadata, token);
        }

        return new CallOptions(headers: headers, deadline: deadline, cancellationToken: cancellationToken);
    }

    /// <summary>
    /// 只认 <see cref="StatusCode.Unauthenticated"/>：其它错误与凭证无关，重注册解决不了，
    /// 反而会让循环空转（与 <see cref="StateClient"/> 同一套处置——那是同一个凭证）。
    /// </summary>
    private void NoteDenied(RpcException error)
    {
        if (error.StatusCode == StatusCode.Unauthenticated)
        {
            _denied.Notify();
        }
    }
}
