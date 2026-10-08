using Grpc.Core;
using Hub.V1;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 状态客户端的行为测试。
///
/// 用假通道而不是真中台：这里要验的是**客户端自己的规矩**——凭证放哪、什么时候换、
/// 撞上 401 会怎样、本地拦住哪些参数。真中台那侧的行为（身份反查、前缀拼接、
/// TTL 真的生效）由 <see cref="StateIntegrationTests"/> 对着中台跑。
/// </summary>
public class StateTests
{
    private static CancellationToken Ct => TestContext.Current.CancellationToken;

    private static StateClient Client(FakeStateChannel channel, int timeoutMs = 1000) =>
        new(channel, TimeSpan.FromMilliseconds(timeoutMs));

    [Fact]
    public async Task 键值读写删扫都走同一条通道且参数原样落到底层()
    {
        var channel = new FakeStateChannel { Value = "v1"u8.ToArray(), Found = true, Deleted = true };
        channel.Entries.Add(new KvEntry
        {
            Key = "p1",
            Value = Google.Protobuf.ByteString.CopyFrom("v1"u8.ToArray()),
        });
        var state = Client(channel);

        var (value, found) = await state.GetAsync("ns", "k1", Ct);
        Assert.True(found);
        Assert.Equal("v1"u8.ToArray(), value);
        Assert.Equal(("ns", "k1"), channel.Gets[^1]);

        await state.PutAsync("ns", "k1", "v1"u8.ToArray(), TimeSpan.Zero, Ct);
        var put = channel.Puts[^1];
        Assert.Equal("ns", put.Namespace);
        Assert.Equal("k1", put.Key);
        Assert.Equal("v1"u8.ToArray(), put.Value);
        Assert.Equal(0L, put.TtlSeconds);

        Assert.True(await state.DeleteAsync("ns", "k1", Ct));
        Assert.Equal(("ns", "k1"), channel.Deletes[^1]);

        var entries = await state.ScanAsync("ns", "p", 10, Ct);
        Assert.Equal(("ns", "p", 10u), channel.Scans[^1]);
        Assert.Equal("p1", Assert.Single(entries).Key);
        Assert.Equal("v1"u8.ToArray(), entries[0].Value);
    }

    /// <summary>
    /// 键不存在时 <c>Found=false</c>，而不是「值为空字节」——这两件事在登录缓存那类场景里
    /// 是两回事：一个是没缓存过，一个是缓存了一个空值。
    /// </summary>
    [Fact]
    public async Task 键不存在与值为空字节是两回事()
    {
        var channel = new FakeStateChannel { Found = false, Value = [] };
        var state = Client(channel);

        var (value, found) = await state.GetAsync("ns", "k1", Ct);

        Assert.False(found);
        Assert.Empty(value);
    }

    /// <summary>
    /// 凭证放在 gRPC metadata 的 <see cref="StateClient.TokenMetadata"/> 上，而不是请求体里
    /// ——键名写错的话中台判「无凭证」，插件只看到 401，很难倒推回去。
    ///
    /// 这里钉住它是跨语言契约文件里的 <c>stateTokenMetadata</c>：三处（proto 注释、
    /// hub-rules.json、本常量）必须是同一个字符串。
    /// </summary>
    [Fact]
    public void 凭证键名与跨语言契约文件一致()
    {
        using var document = System.Text.Json.JsonDocument.Parse(
            File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "testdata", "hub-rules.json")));

        Assert.Equal(
            StateClient.TokenMetadata,
            document.RootElement.GetProperty("stateTokenMetadata").GetString());
        Assert.Equal("x-hub-state-token", StateClient.TokenMetadata);
    }

    [Fact]
    public async Task 凭证随注册轮换后用的是新凭证()
    {
        var channel = new FakeStateChannel();
        var state = Client(channel);

        // 还没注册（凭证为空）时**不带上**这个头：中台对「头缺失」与「头为空」都判无凭证，
        // 但发一个空值要先赌 gRPC 允许空 metadata 值
        await state.GetAsync("ns", "k1", Ct);
        Assert.Null(Assert.Single(channel.Tokens));

        state.SetToken("tok-1");
        await state.GetAsync("ns", "k1", Ct);
        Assert.Equal("tok-1", channel.Tokens[^1]);

        state.SetToken("tok-2");
        await state.GetAsync("ns", "k1", Ct);
        Assert.Equal("tok-2", channel.Tokens[^1]);
    }

    /// <summary>
    /// 调用方的 <see cref="CancellationToken"/> **原样透传**（不派生新令牌），
    /// 时间上限走 <c>Deadline</c>。于是「调用方取消」永远是 Cancelled、
    /// 「超过上限」永远是 DeadlineExceeded，两者不会互相化装。
    /// </summary>
    [Fact]
    public async Task 调用方的取消令牌原样透传且上限走deadline()
    {
        var channel = new FakeStateChannel();
        var state = Client(channel, 30_000);

        using var cts = new CancellationTokenSource();
        await state.GetAsync("ns", "k1", cts.Token);

        var options = channel.LastOptions!.Value;
        Assert.Equal(cts.Token, options.CancellationToken);
        Assert.Equal(30d, (options.Deadline!.Value - DateTime.UtcNow).TotalSeconds, precision: 1);
    }

    [Fact]
    public async Task 超过一兆的值在本地就被拦住且不发请求()
    {
        var channel = new FakeStateChannel();
        var state = Client(channel);

        var tooBig = new byte[StateClient.MaxValueBytes + 1];
        var ex = await Assert.ThrowsAsync<HubStateException>(
            () => state.PutAsync("ns", "k1", tooBig, cancellationToken: Ct));

        // 错误里直接写清上限与实收：中台那句 InvalidArgument 不会告诉你这些
        Assert.Contains("1048576", ex.Message);
        Assert.Contains("1048577", ex.Message);
        Assert.Equal(0, channel.Calls);
    }

    [Fact]
    public async Task 刚好等于上限的值可以写入()
    {
        var channel = new FakeStateChannel();
        var state = Client(channel);

        await state.PutAsync("ns", "k1", new byte[StateClient.MaxValueBytes], cancellationToken: Ct);

        Assert.Equal(StateClient.MaxValueBytes, channel.Puts[^1].Value.Length);
    }

    [Theory]
    [InlineData(0u)]
    [InlineData(StateClient.MaxScanLimit + 1)]
    public async Task 扫描条数超出范围在本地就被拦住且不发请求(uint limit)
    {
        var channel = new FakeStateChannel();
        var state = Client(channel);

        var ex = await Assert.ThrowsAsync<HubStateException>(() => state.ScanAsync("ns", "p", limit, Ct));

        Assert.Contains($"1..={StateClient.MaxScanLimit}", ex.Message);
        Assert.Equal(0, channel.Calls);
    }

    [Fact]
    public async Task 上限条数刚好可以扫()
    {
        var channel = new FakeStateChannel();
        var state = Client(channel);

        await state.ScanAsync("ns", "p", StateClient.MaxScanLimit, Ct);

        Assert.Equal(StateClient.MaxScanLimit, channel.Scans[^1].Limit);
    }

    /// <summary>
    /// TTL 的粒度是秒：不足 1 秒的 ttl 被**截断为 0**，也就是**永不过期**。
    /// 与 Go 侧同一处截断——真拿 500ms 当「半秒后过期」用的话，键会一直留着。
    /// </summary>
    [Fact]
    public async Task 不足一秒的ttl被截断为零而不是向上取整()
    {
        var channel = new FakeStateChannel();
        var state = Client(channel);

        await state.PutAsync("ns", "sub", [], TimeSpan.FromMilliseconds(999), Ct);
        Assert.Equal(0L, channel.Puts[^1].TtlSeconds);

        await state.PutAsync("ns", "exact", [], TimeSpan.FromSeconds(1), Ct);
        Assert.Equal(1L, channel.Puts[^1].TtlSeconds);

        await state.PutAsync("ns", "frac", [], TimeSpan.FromMilliseconds(1500), Ct);
        Assert.Equal(1L, channel.Puts[^1].TtlSeconds);

        await state.PutAsync("ns", "zero", [], TimeSpan.Zero, Ct);
        Assert.Equal(0L, channel.Puts[^1].TtlSeconds);
    }

    [Fact]
    public async Task 负的ttl在本地就被拦住()
    {
        var channel = new FakeStateChannel();
        var state = Client(channel);

        await Assert.ThrowsAsync<HubStateException>(
            () => state.PutAsync("ns", "k1", [], TimeSpan.FromSeconds(-1), Ct));

        Assert.Equal(0, channel.Calls);
    }

    /// <summary>
    /// Publish 是插件间**异步**协作的通道：受理成功返回中台分配的 run_id，
    /// 凭证与上限和四个状态方法走同一条路（同一个 <see cref="StateClient.TokenMetadata"/> 头）。
    /// </summary>
    [Fact]
    public async Task publish受理时返回runId且参数原样落到底层()
    {
        var channel = new FakeStateChannel { Accepted = true, RunId = "run-42" };
        var state = Client(channel);
        var envelope = new Envelope { MessageId = "m-1" };

        state.SetToken("tok-1");
        var runId = await state.PublishAsync("order-flow", envelope, Ct);

        Assert.Equal("run-42", runId);
        Assert.Equal("order-flow", Assert.Single(channel.Publishes));
        Assert.Equal("tok-1", Assert.Single(channel.Tokens));
    }

    /// <summary>
    /// accepted=false 是**业务结果**而不是网络故障：reason 要原样暴露给调用方——
    /// 吞进一个泛泛的异常消息里，调用方就分不清「该改逻辑」（防环、flow 不存在）
    /// 和「该退避重试」（总线故障）了。
    /// </summary>
    [Fact]
    public async Task publish未受理时reason随类型化异常暴露()
    {
        var channel = new FakeStateChannel { Accepted = false, RejectReason = "检测到互调环: a->b->a" };
        var state = Client(channel);

        var ex = await Assert.ThrowsAsync<PublishRejectedException>(
            () => state.PublishAsync("order-flow", new Envelope(), Ct));

        Assert.Equal("检测到互调环: a->b->a", ex.Reason);
        Assert.Contains("检测到互调环", ex.Message);
    }

    [Fact]
    public async Task publish未给出原因时异常也不含糊其辞()
    {
        var channel = new FakeStateChannel { Accepted = false, RejectReason = "" };
        var state = Client(channel);

        var ex = await Assert.ThrowsAsync<PublishRejectedException>(
            () => state.PublishAsync("order-flow", new Envelope(), Ct));

        Assert.Empty(ex.Reason);
        Assert.Contains("未给出原因", ex.Message);
    }

    /// <summary>
    /// 每个方法都受单次调用上限约束。
    ///
    /// 逐个列出来而不是抽一个循环：漏掉任何一个入口的包装都不会有编译期提示，
    /// 而漏掉的那个会在中台卡顿时把调用方的整段预算吃掉。
    /// </summary>
    [Fact]
    public async Task 每个方法都受单次调用上限约束()
    {
        var calls = new (string Name, Func<StateClient, CancellationToken, Task> Call)[]
        {
            ("Get", async (c, ct) => await c.GetAsync("ns", "k", ct)),
            ("Put", (c, ct) => c.PutAsync("ns", "k", [], cancellationToken: ct)),
            ("Delete", async (c, ct) => await c.DeleteAsync("ns", "k", ct)),
            ("Scan", async (c, ct) => await c.ScanAsync("ns", "", 10, ct)),
            ("Publish", (c, ct) => c.PublishAsync("flow", new Envelope(), ct)),
        };

        foreach (var (name, call) in calls)
        {
            var channel = new FakeStateChannel { BlockUntilDeadline = true };
            var state = Client(channel, 200);

            var start = DateTime.UtcNow;
            var ex = await Assert.ThrowsAsync<RpcException>(() => call(state, Ct));
            var elapsed = DateTime.UtcNow - start;

            Assert.Equal(StatusCode.DeadlineExceeded, ex.StatusCode);
            Assert.True(elapsed < TimeSpan.FromSeconds(2), $"{name} 没有在上限附近返回，实际 {elapsed}");
        }
    }

    /// <summary>上限是**上限而非承诺**：调用方自己的预算更早就按调用方的来。</summary>
    [Fact]
    public async Task 调用方的预算更早时按调用方的结束()
    {
        var channel = new FakeStateChannel { BlockUntilDeadline = true };
        // 上限设得足够大，保证先撞到的一定是调用方自己的取消
        var state = Client(channel, 30_000);

        using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(50));

        var start = DateTime.UtcNow;
        var ex = await Assert.ThrowsAsync<RpcException>(() => state.GetAsync("ns", "k", cts.Token));
        var elapsed = DateTime.UtcNow - start;

        Assert.Equal(StatusCode.Cancelled, ex.StatusCode);
        Assert.True(elapsed < TimeSpan.FromSeconds(2), $"不该被延长到 callTimeout，实际 {elapsed}");
    }

    /// <summary>
    /// 凭证被拒时：错误**原样**交给插件，同时叫醒注册循环。
    ///
    /// 两件事都要做——吞掉错误会让插件把一次失败的读当成「键不存在」；
    /// 只抛错误不叫醒注册循环，插件就会一直拿着废凭证哑下去。
    /// </summary>
    [Fact]
    public async Task 凭证被拒时原样抛出并且叫醒注册循环()
    {
        var channel = new FakeStateChannel
        {
            Fail = () => new RpcException(new Status(StatusCode.Unauthenticated, "状态凭证无效或已失效")),
        };
        var state = Client(channel);

        var ex = await Assert.ThrowsAsync<RpcException>(() => state.GetAsync("ns", "k", Ct));
        Assert.Equal(StatusCode.Unauthenticated, ex.StatusCode);

        Assert.True(await Raised(state), "凭证被拒应当叫醒注册循环");
    }

    /// <summary>401 之外的失败一律不叫醒注册循环：它们跟凭证无关，重注册解决不了。</summary>
    [Theory]
    [InlineData(StatusCode.DeadlineExceeded)]
    [InlineData(StatusCode.Cancelled)]
    [InlineData(StatusCode.InvalidArgument)]
    [InlineData(StatusCode.Internal)]
    [InlineData(StatusCode.Unavailable)]
    public async Task 其它错误不会叫醒注册循环(StatusCode code)
    {
        var channel = new FakeStateChannel
        {
            Fail = () => new RpcException(new Status(code, "与凭证无关")),
        };
        var state = Client(channel);

        var ex = await Assert.ThrowsAsync<RpcException>(() => state.GetAsync("ns", "k", Ct));
        Assert.Equal(code, ex.StatusCode);

        Assert.False(await Raised(state, waitMs: 200), "与凭证无关的错误不该触发重新注册");
    }

    /// <summary>
    /// 并发一起撞上 401 时只留一个信号：不这样做的话，一次凭证轮换会把注册循环叫成风暴
    /// （每次注册都要探测一遍插件自己的地址）。
    /// </summary>
    [Fact]
    public async Task 并发撞上401只留一个信号()
    {
        var channel = new FakeStateChannel
        {
            Fail = () => new RpcException(new Status(StatusCode.Unauthenticated, "状态凭证无效或已失效")),
        };
        var state = Client(channel);

        var storm = Enumerable.Range(0, 16)
            .Select(_ => Task.Run(async () =>
            {
                try
                {
                    await state.GetAsync("ns", "k", CancellationToken.None);
                }
                catch (RpcException)
                {
                    // 预期内
                }
            }))
            .ToArray();

        await Task.WhenAll(storm);

        Assert.True(await Raised(state), "16 次 401 至少该留下一个信号");
        // 第二个信号不该存在：信号已经在上一次等待里被取走
        Assert.False(await Raised(state, waitMs: 200), "并发的 401 不该攒成一堆信号");
    }

    /// <summary>
    /// 等待被取消时**不消费**信号——这是心跳循环与状态客户端之间的那根细线。
    ///
    /// 心跳那一拍赢了的时候，挂在后面的 401 等待会被取消；如果取消也算「收到」并把它吃掉，
    /// 那次 401 就永远丢了，表现为偶发的「该重注册却没重注册」，极难查。
    /// </summary>
    [Fact]
    public async Task 等待被取消不会吃掉信号()
    {
        var channel = new FakeStateChannel
        {
            Fail = () => new RpcException(new Status(StatusCode.Unauthenticated, "状态凭证无效或已失效")),
        };
        var state = Client(channel);

        using var abandoned = new CancellationTokenSource();
        var loser = state.Denied.WaitAsync(abandoned.Token);
        abandoned.Cancel();

        Assert.False(await loser, "被取消的等待不该算作收到了信号");

        // 信号在取消**之后**才来：它必须还在
        await Assert.ThrowsAsync<RpcException>(() => state.GetAsync("ns", "k", Ct));
        Assert.True(await Raised(state), "被取消的等待不该把后来的信号吃掉");
    }

    [Fact]
    public async Task 缺省构造也带上限()
    {
        // 与 Go 侧的 DefaultStateCallTimeout 一致
        Assert.Equal(TimeSpan.FromSeconds(2), StateClient.DefaultCallTimeout);

        var channel = new FakeStateChannel();
        var state = new StateClient(channel);

        await state.GetAsync("ns", "k", Ct);

        var remaining = channel.LastOptions!.Value.Deadline!.Value - DateTime.UtcNow;
        Assert.InRange(remaining, TimeSpan.FromSeconds(1), TimeSpan.FromSeconds(2));
    }

    /// <summary>等一次「凭证被拒」信号；<paramref name="waitMs"/> 内没等到就是 false。</summary>
    private static async Task<bool> Raised(StateClient state, int waitMs = 500)
    {
        using var wait = new CancellationTokenSource(TimeSpan.FromMilliseconds(waitMs));
        return await state.Denied.WaitAsync(wait.Token);
    }
}
