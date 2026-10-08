using Grpc.Core;
using Hub.V1;

namespace HubKit;

/// <summary>扫描返回的一项。</summary>
public sealed record StateEntry(string Key, byte[] Value);

/// <summary>
/// 状态调用在**本地**就被拦下（参数非法、超过中台的上限）。
///
/// 存在的理由：这些错中台也会回 <c>InvalidArgument</c>，但那要等一次网络往返还只是一句
/// 泛泛的「参数非法」。本地先拦一道，错误里直接写清是哪个参数、上限多少、收到了多少。
///
/// 它与 <see cref="RpcException"/> 的分工是明确的：**本地能判定的**抛它，
/// **中台判定的**（凭证无效、后端故障）原样抛 <see cref="RpcException"/>。
/// </summary>
public sealed class HubStateException(string message) : Exception(message);

/// <summary>
/// 中台没有受理一次 Publish（<c>accepted=false</c>）。
///
/// 这是**业务结果**而不是网络故障：防环、超限、目标 flow 不存在都属于此类，
/// <see cref="Reason"/> 会说明原因。调用方该据此改逻辑或换目标，而不是退避重试——
/// 重试是给「中台/总线坏了」（<see cref="RpcException"/>）准备的，两者被中台刻意
/// 分在两个通道里。
/// </summary>
public sealed class PublishRejectedException(string? reason)
    : Exception(reason is null || reason.Length == 0
        ? "中台未受理 Publish（未给出原因）"
        : $"中台未受理 Publish：{reason}")
{
    /// <summary>中台给的人类可读原因；可能为空串。</summary>
    public string Reason { get; } = reason ?? string.Empty;
}

/// <summary>
/// 插件 → HubState 那条链路。
///
/// 抽成接口的理由与 <see cref="IRegistryChannel"/> 同一个：让「凭证怎么带、什么时候换、
/// 撞上 401 会怎样」这些**行为**能被单测证明，而不是只能靠一个真中台去验。
/// </summary>
public interface IStateChannel
{
    Task<KvGetResponse> KvGetAsync(KvGetRequest request, CallOptions options);

    Task<KvPutResponse> KvPutAsync(KvPutRequest request, CallOptions options);

    Task<KvDeleteResponse> KvDeleteAsync(KvDeleteRequest request, CallOptions options);

    Task<KvScanResponse> KvScanAsync(KvScanRequest request, CallOptions options);

    Task<PublishResponse> PublishAsync(PublishRequest request, CallOptions options);
}

/// <summary>
/// 真的连中台。凭证与超时都在 <see cref="CallOptions"/> 里，这里只做转发——
/// 不在这里另拼 metadata，就是为了让「凭证是 <see cref="StateClient"/> 放进去的」
/// 这件事只有一处。
/// </summary>
public sealed class GrpcStateChannel(HubState.HubStateClient client) : IStateChannel
{
    public Task<KvGetResponse> KvGetAsync(KvGetRequest request, CallOptions options) =>
        client.KvGetAsync(request, options).ResponseAsync;

    public Task<KvPutResponse> KvPutAsync(KvPutRequest request, CallOptions options) =>
        client.KvPutAsync(request, options).ResponseAsync;

    public Task<KvDeleteResponse> KvDeleteAsync(KvDeleteRequest request, CallOptions options) =>
        client.KvDeleteAsync(request, options).ResponseAsync;

    public Task<KvScanResponse> KvScanAsync(KvScanRequest request, CallOptions options) =>
        client.KvScanAsync(request, options).ResponseAsync;

    public Task<PublishResponse> PublishAsync(PublishRequest request, CallOptions options) =>
        client.PublishAsync(request, options).ResponseAsync;
}

/// <summary>
/// 需要外置状态的插件实现它。
///
/// 用可选接口而不是往 <see cref="IPlugin"/> 里加方法：老插件一行不用改，新插件想用才实现。
/// </summary>
public interface IStateAware
{
    /// <summary>
    /// 在**每次**注册成功后由骨架调用（首次注册、以及被摘除 / 中台重启后的自愈重注册）。
    ///
    /// 凭证随每次注册轮换，所以注入的永远是<strong>同一个</strong>客户端对象、最新的凭证——
    /// 实现方**不要**把这个引用长期拷进别处，也不该缓存凭证。
    ///
    /// 骨架从注册循环那个任务里调用它，而插件的 handler 通常跑在别的任务上，
    /// 所以**同步是实现方的责任**：用 <c>lock</c> 或 <c>volatile</c> 护住那个字段。
    /// 裸赋值给一个会被其它任务读取的字段就是数据竞争。
    /// </summary>
    void SetState(StateClient state);
}

/// <summary>
/// 中台外置状态（HubState）的客户端。
///
/// 插件被强制无状态：实例内存不保证跨调用保留，需要跨调用保留的东西走这里。
/// 客户端由宿主在**注册成功后**注入给实现了 <see cref="IStateAware"/> 的插件——
/// 插件作者不该自己构造它，凭证只有中台知道（见 <c>RegisterResponse.state_token</c>）。
///
/// **键前缀不含版本号**：中台把键拼成 <c>hub:state:{插件名}:{namespace}:{key}</c>，
/// 同一插件的**所有版本共用一个状态空间**。升版本不会清空状态（对登录缓存这类状态
/// 正是要的），但两个版本往同一个 namespace 写就是**互相覆盖**。要按版本隔离，
/// 请自己把版本写进 namespace。
///
/// 凭证会被重新注册轮换（中台重启、实例被摘除后自愈），而注册循环跑在另一个任务上，
/// 所以凭证的读写都要过锁。
/// </summary>
public sealed class StateClient
{
    /// <summary>
    /// 状态凭证的 metadata 键，必须与中台侧一致（<c>crates/hub-core/src/state.rs</c> 的
    /// <c>STATE_TOKEN_METADATA</c>）。事实来源是 <c>sdk/go/hubkit/testdata/hub-rules.json</c>
    /// 的 <c>stateTokenMetadata</c>，有测试钉住这三处一致。
    ///
    /// 导出它而不是让各处各写一份字面量：写错的话中台会判成「无凭证」，
    /// 而插件侧只会看到一个 401，很难查。
    /// </summary>
    public const string TokenMetadata = "x-hub-state-token";

    /// <summary>
    /// 单个值的字节上限，取自中台的实现常量（<c>crates/hub-core/src/state.rs</c> 的
    /// <c>MAX_VALUE_BYTES</c>）——**不要在这里自己编一个数**：放宽了会被中台拒，
    /// 收紧了会让本来合法的写入在本地失败。有契约测试盯着这两处相等
    /// （<c>ContractSyncTests.本地上限常量与中台实现一致</c>）。
    /// </summary>
    public const int MaxValueBytes = 1024 * 1024;

    /// <summary>单次扫描的条数上限，取自中台的 <c>MAX_SCAN_LIMIT</c>。</summary>
    public const uint MaxScanLimit = 1000;

    /// <summary>单次调用的默认时间上限，与 Go 侧的 <c>DefaultStateCallTimeout</c> 一致。</summary>
    public static readonly TimeSpan DefaultCallTimeout = TimeSpan.FromSeconds(2);

    private readonly IStateChannel _channel;
    private readonly TimeSpan _callTimeout;

    private readonly object _gate = new();
    private string _token = string.Empty;

    /// <param name="channel">与中台之间那条链路。</param>
    /// <param name="callTimeout">
    /// 单次调用的时间上限，缺省 <see cref="DefaultCallTimeout"/>。
    /// 它是**上限而非承诺**：调用方自己的 <see cref="CancellationToken"/> 取消得更早就按调用方的来。
    /// </param>
    public StateClient(IStateChannel channel, TimeSpan callTimeout = default)
    {
        _channel = channel;
        _callTimeout = callTimeout > TimeSpan.Zero ? callTimeout : DefaultCallTimeout;
    }

    /// <summary>
    /// 「凭证被中台拒了，去换一张」的信号，注册循环据此重走注册流程。
    ///
    /// 只有注册循环关心它，插件作者不必知道——所以是 internal。
    /// </summary>
    internal DeniedSignal Denied { get; } = new();

    /// <summary>
    /// 记下中台本次下发的凭证。由注册流程调用，**不对外**——插件作者不该自己管凭证。
    ///
    /// 注入给状态调用的和注销要用的都是这一份（见 <see cref="CurrentToken"/>）。
    /// </summary>
    internal void SetToken(string? token)
    {
        lock (_gate)
        {
            _token = token ?? string.Empty;
        }
    }

    /// <summary>
    /// 当前凭证。空串表示这次注册没拿到凭证（迁移前登记的旧行）。
    ///
    /// **状态凭证与注销凭证是同一个东西**：中台注册时只下发这一张，它既是 HubState 的
    /// 认证依据（<see cref="TokenMetadata"/> 那个头），也是注销时「你是这一行的主人」的
    /// 证明（<c>UnregisterRequest.state_token</c>）。所以 <see cref="Registrar"/> 注销时
    /// 直接读这一份，不另存——同一个凭证存两处，迟早有一处忘了跟着轮换。
    ///
    /// 注销那个用途还多一层含义：**空串意味着本实例没进过注册表**，没有行可摘除
    /// （见 <see cref="Registrar.UnregisterAsync"/>）。
    /// </summary>
    internal string CurrentToken()
    {
        lock (_gate)
        {
            return _token;
        }
    }

    /// <summary>
    /// 读取一个键。<c>Found</c> 区分「键不存在」与「值是空字节」。
    /// </summary>
    public async Task<(byte[] Value, bool Found)> GetAsync(
        string @namespace,
        string key,
        CancellationToken cancellationToken = default)
    {
        var response = await CallAsync(
            options => _channel.KvGetAsync(
                new KvGetRequest { Key = new KvKey { Namespace = @namespace, Key = key } },
                options),
            cancellationToken).ConfigureAwait(false);

        return (response.Value.ToByteArray(), response.Found);
    }

    /// <summary>
    /// 写入一个键。<paramref name="ttl"/> 为 <see cref="TimeSpan.Zero"/> 表示不过期。
    ///
    /// 注意：TTL 的粒度是**秒**（线协议是 <c>ttl_seconds</c>），不足 1 秒的 ttl 会被**截断为 0**，
    /// 也就是**永不过期**——与 Go 侧同一处截断，两侧的插件不该在这里有不同的行为。
    /// 想让键很快消失，请传 &gt;= 1s 的值。
    /// </summary>
    public async Task PutAsync(
        string @namespace,
        string key,
        byte[]? value,
        TimeSpan ttl = default,
        CancellationToken cancellationToken = default)
    {
        // 与 Go 侧同语义：nil 切片就是**空值**，不是错误
        value ??= [];

        if (value.Length > MaxValueBytes)
        {
            // 中台也会拒（InvalidArgument），但本地这道错把「上限多少、收到多少」直接写出来了，
            // 而且省掉一次注定失败的往返
            throw new HubStateException(
                $"value 超过上限 {MaxValueBytes} 字节，收到 {value.Length} 字节（HubState 不是对象存储）");
        }

        if (ttl < TimeSpan.Zero)
        {
            throw new HubStateException($"ttl 不能为负，收到 {ttl}（0 表示不过期）");
        }

        // 按秒截断，与 Go 侧 `ttl / time.Second` 同一语义：不足 1 秒即 0（不过期）
        var ttlSeconds = (long)ttl.TotalSeconds;

        await CallAsync(
            options => _channel.KvPutAsync(
                new KvPutRequest
                {
                    Key = new KvKey { Namespace = @namespace, Key = key },
                    Value = Google.Protobuf.ByteString.CopyFrom(value),
                    TtlSeconds = ttlSeconds,
                },
                options),
            cancellationToken).ConfigureAwait(false);
    }

    /// <summary>
    /// 删除一个键。删不存在的键返回 <c>false</c>，**不是错误**——
    /// 调用方的意图（这键没了）已经达成。
    /// </summary>
    public async Task<bool> DeleteAsync(
        string @namespace,
        string key,
        CancellationToken cancellationToken = default)
    {
        var response = await CallAsync(
            options => _channel.KvDeleteAsync(
                new KvDeleteRequest { Key = new KvKey { Namespace = @namespace, Key = key } },
                options),
            cancellationToken).ConfigureAwait(false);

        return response.Deleted;
    }

    /// <summary>
    /// 扫描一个命名空间下前缀匹配的键。
    ///
    /// <paramref name="prefix"/> 可以为空（扫整个命名空间）。
    /// <paramref name="limit"/> 必须在 <c>1..=<see cref="MaxScanLimit"/></c> 之间：
    /// 中台另有硬上限，防止插件一次把整个命名空间拉走。
    ///
    /// 刻意**不给缺省值**：一次拉多少条是调用方该想清楚的事（与 Go 侧的签名一致）。
    /// </summary>
    public async Task<IReadOnlyList<StateEntry>> ScanAsync(
        string @namespace,
        string prefix,
        uint limit,
        CancellationToken cancellationToken = default)
    {
        if (limit == 0 || limit > MaxScanLimit)
        {
            throw new HubStateException($"limit 必须在 1..={MaxScanLimit} 之间，收到 {limit}");
        }

        var response = await CallAsync(
            options => _channel.KvScanAsync(
                new KvScanRequest { Namespace = @namespace, Prefix = prefix, Limit = limit },
                options),
            cancellationToken).ConfigureAwait(false);

        return response.Entries.Select(e => new StateEntry(e.Key, e.Value.ToByteArray())).ToList();
    }

    /// <summary>
    /// 把一条信封投递到目标 flow / topic，**异步**触发、拿不到业务结果。
    ///
    /// 这是插件间异步协作的通道；需要下游处理结果的同步场景走
    /// <see cref="GatewayClient.InvokePluginAsync"/>。信封用 <see cref="Envelopes.NewEnvelope"/>
    /// 起步、载荷用 <see cref="Envelopes.WithPayloadJson"/> / <see cref="Envelopes.WithPayload"/>
    /// 装；subject 由中台覆盖为调用方身份，自己填了也没用。
    ///
    /// 受理成功返回中台分配的 run_id（本次触发的 flow 执行标识）；未受理抛
    /// <see cref="PublishRejectedException"/>，reason 随异常携带。
    /// </summary>
    public async Task<string> PublishAsync(
        string target,
        Envelope envelope,
        CancellationToken cancellationToken = default)
    {
        var response = await CallAsync(
            options => _channel.PublishAsync(
                new PublishRequest { Target = target, Envelope = envelope },
                options),
            cancellationToken).ConfigureAwait(false);

        if (!response.Accepted)
        {
            throw new PublishRejectedException(response.Reason);
        }

        return response.RunId;
    }

    /// <summary>
    /// 一次调用：带上凭证与时间上限，并把「凭证被拒」讲给注册循环。
    ///
    /// 失败**原样抛出**给调用方：重注册是后台的补救，不该掩盖这一次失败——
    /// 插件很可能按 fail-open 处理（比如拿不到登录缓存就回源查一次）。
    /// </summary>
    private async Task<TResponse> CallAsync<TResponse>(
        Func<CallOptions, Task<TResponse>> call,
        CancellationToken cancellationToken)
    {
        try
        {
            return await call(Options(cancellationToken)).ConfigureAwait(false);
        }
        catch (RpcException ex)
        {
            NoteDenied(ex);
            throw;
        }
    }

    /// <summary>
    /// 组装这一次调用的 metadata 与时间上限。
    ///
    /// 超时走 <c>Deadline</c> 而不是自己起一个 <c>CancellationTokenSource.CancelAfter</c>：
    /// 前者在 .NET 里表现为 <c>DeadlineExceeded</c>，与 Go 侧的语义一致；
    /// 后者会变成 <c>Cancelled</c>，调用方就分不清「中台慢」和「我自己取消了」。
    ///
    /// 调用方的 <see cref="CancellationToken"/> **原样透传**、不再派生新的：
    /// 于是「调用方取消」永远是 <c>Cancelled</c>，「超上限」永远是 <c>DeadlineExceeded</c>，
    /// 两者不会互相化装。
    /// </summary>
    private CallOptions Options(CancellationToken cancellationToken)
    {
        var token = CurrentToken();

        // 没有凭证时**不发**这个头，而不是发一个空值：中台对「头缺失」与「头为空」
        // 的处理虽然一样（都判无凭证），但发空值要先赌 gRPC 允许空 metadata 值
        var headers = new Metadata();
        if (token.Length > 0)
        {
            headers.Add(TokenMetadata, token);
        }

        // 上限大到 DateTime 表达不了时退化成**不设 deadline**：让调用方自己的 token 兜底，
        // 也比在这里抛一个 ArgumentOutOfRangeException 好
        var now = DateTime.UtcNow;
        DateTime? deadline = DateTime.MaxValue - _callTimeout > now ? now.Add(_callTimeout) : null;

        return new CallOptions(headers: headers, deadline: deadline, cancellationToken: cancellationToken);
    }

    /// <summary>
    /// 凭证被中台拒绝时叫醒注册循环去换一张新凭证。
    ///
    /// **为什么会有这个错**：凭证不是常驻的。中台重启会轮换凭证、实例被摘除后旧凭证即失效，
    /// 而这两种情况下心跳本身可能一切正常（注册循环毫不知情），插件就要一直哑下去——
    /// 直到这里撞上 401 把它叫醒。
    ///
    /// 只认 <see cref="StatusCode.Unauthenticated"/>：其它错误（网络抖动、参数非法、
    /// 超时）与凭证无关，重注册解决不了，反而会让循环空转。
    /// </summary>
    private void NoteDenied(RpcException error)
    {
        if (error.StatusCode == StatusCode.Unauthenticated)
        {
            Denied.Notify();
        }
    }
}

/// <summary>
/// 「凭证被拒」的一次性信号，缓冲 1 + 非阻塞发送：并发调用一起撞上 401 时只留一个信号，
/// 不会把注册循环叫成风暴。
///
/// 用 TCS 而不是 <c>SemaphoreSlim</c>：心跳循环每次只等一拍，输给心跳那一拍的那次等待会被
/// 取消掉——而 <c>SemaphoreSlim.WaitAsync</c> 一旦有人等过就会**吃掉**那个计数，
/// 被遗弃的等待会把一次 401 吞掉（表现为偶发的「该重注册却没重注册」，极难查）。
/// 这里的取消路径**不消费**信号，留给下一次等待。
/// </summary>
internal sealed class DeniedSignal
{
    private readonly object _gate = new();
    private TaskCompletionSource _pending = NewSource();
    private bool _raised;

    /// <summary>登记一次「凭证被拒」。已经有待处理信号时直接丢弃。</summary>
    public void Notify()
    {
        lock (_gate)
        {
            if (_raised)
            {
                return;
            }

            _raised = true;
            _pending.TrySetResult();
        }
    }

    /// <summary>
    /// 等到一次「凭证被拒」。<paramref name="cancellationToken"/> 结束则返回 false，
    /// 且**不消费**信号。同一个信号只会被等到的那个调用取走一次。
    /// </summary>
    public async Task<bool> WaitAsync(CancellationToken cancellationToken)
    {
        Task pending;
        lock (_gate)
        {
            pending = _pending.Task;
        }

        try
        {
            await pending.WaitAsync(cancellationToken).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            // 没等到就被取消了：这一次等待作废，但信号还在，留给下一次
            return false;
        }

        lock (_gate)
        {
            // 消费掉这个信号（本实现只会有一个等待者；_raised 已是 false 说明它刚被
            // 前一个等待取走，那也照样算「等到了」——醒来总比把一次 401 丢掉强）
            if (_raised)
            {
                _raised = false;
                _pending = NewSource();
            }
        }

        return true;
    }

    private static TaskCompletionSource NewSource() =>
        new(TaskCreationOptions.RunContinuationsAsynchronously);
}
