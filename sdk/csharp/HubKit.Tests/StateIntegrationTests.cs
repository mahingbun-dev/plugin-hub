using System.Net;
using System.Net.Sockets;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 对着**真中台**跑通 HubState。
///
/// 假的通道能证明客户端自己的规矩，证明不了中台那侧的三件事：
/// <list type="number">
/// <item>凭证真的被接受（<c>x-hub-state-token</c> 放错地方就是 401）</item>
/// <item>身份真的按凭证反查出来、前缀真的由中台拼上（插件自报的 namespace 只是子空间）</item>
/// <item>TTL 真的会过期、Scan 真的按前缀过滤、Delete 真的删掉</item>
/// </list>
///
/// 中台没跑时**跳过**而不是判红：这条测试依赖一个外部进程，红了也说明不了 SDK 有问题。
/// 地址可用 <c>HUB_STATE_IT_ADDR</c> 覆盖。
/// </summary>
public class StateIntegrationTests(ITestOutputHelper output)
{
    private const string DefaultHub = "http://127.0.0.1:8093";

    /// <summary>插件名（会注册进真中台）。固定它，重复跑就是「同版本重注册=进程重启」。</summary>
    private const string PluginName = "csharp-state-it";

    private static CancellationToken Ct => TestContext.Current.CancellationToken;

    [Fact]
    public async Task 状态读写删扫对着真中台跑通()
    {
        var hub = Environment.GetEnvironmentVariable("HUB_STATE_IT_ADDR") ?? DefaultHub;
        if (!Reachable(hub))
        {
            Assert.Skip($"中台插件面 {hub} 连不上——这条测试要一个真中台（它得有 HubState 与后端存储）");
        }

        var port = FreePort();
        var sink = new StringWriter();
        var config = new HubConfig
        {
            HubAddr = hub,
            // 中台注册时会**从它那边**拨这个地址做可达性探测，所以必须是中台视角拨得通的地址
            AdvertiseAddr = $"http://127.0.0.1:{port}",
            ListenAddr = $":{port}",
            InstanceId = $"csharp-state-it-{port}",
            RetryInterval = TimeSpan.FromMilliseconds(200),
            Logger = new HubLogger(HubLogLevel.Debug, sink),
        };

        var plugin = new StateAwarePlugin { Name = PluginName };
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(120));
        var host = PluginHost.RunAsync(plugin, config, cts.Token);

        try
        {
            var injected = await TestWait.Until(() => plugin.State() is not null, TimeSpan.FromSeconds(30));
            Assert.True(injected, $"注册成功后应当把状态客户端注入给插件。日志：\n{sink}");
            output.WriteLine($"已注册并注入状态客户端：{Registration(sink)}");

            var state = plugin.State()!;

            // 每次跑用不同的命名空间，避免与上一轮（或别的 agent）的数据撞上。
            // 字符集受限（[A-Za-z0-9_.-]），所以端口号直接拼进来正好
            var ns = $"it{port}";

            await RoundTripAsync(state, ns);
            await TtlExpiresAsync(state, ns);
            await ScanIsIsolatedAsync(state, ns);
        }
        finally
        {
            cts.Cancel();
            await host;
        }
    }

    /// <summary>Put → Get → Delete → Get：一次完整的读写删往返。</summary>
    private async Task RoundTripAsync(StateClient state, string ns)
    {
        await state.PutAsync(ns, "k1", "v1"u8.ToArray(), cancellationToken: Ct);
        output.WriteLine($"Put {ns}/k1 = v1");

        var (value, found) = await state.GetAsync(ns, "k1", Ct);
        Assert.True(found, "刚写进去的键应当读得到");
        Assert.Equal("v1", System.Text.Encoding.UTF8.GetString(value));
        output.WriteLine($"Get {ns}/k1 → found={found} value={System.Text.Encoding.UTF8.GetString(value)}");

        // 空字节的值与「键不存在」在服务器上是两回事，真中台也得区分得开
        await state.PutAsync(ns, "empty", [], cancellationToken: Ct);
        var (emptyValue, emptyFound) = await state.GetAsync(ns, "empty", Ct);
        Assert.True(emptyFound, "写进去一个空值也应当算「键存在」");
        Assert.Empty(emptyValue);
        output.WriteLine($"Get {ns}/empty → found={emptyFound} value=（空）");

        Assert.True(await state.DeleteAsync(ns, "k1", Ct), "删一个存在的键应当返回 true");
        var (_, afterDelete) = await state.GetAsync(ns, "k1", Ct);
        Assert.False(afterDelete, "删掉之后不该还读得到");
        output.WriteLine($"Delete {ns}/k1 → deleted=True；再 Get → found={afterDelete}");

        // 删不存在的键不是错误：调用方的意图（这键没了）已经达成
        Assert.False(await state.DeleteAsync(ns, "k1", Ct), "删一个不存在的键应当返回 false 而不是报错");
        output.WriteLine($"Delete {ns}/k1（第二次）→ deleted=False（不是错误）");
    }

    /// <summary>ttl_seconds 真的会过期。不验这条的话，一个把 ttl 丢掉的实现也能全绿。</summary>
    private async Task TtlExpiresAsync(StateClient state, string ns)
    {
        await state.PutAsync(ns, "ttl", "v"u8.ToArray(), TimeSpan.FromSeconds(2), Ct);
        var (_, found) = await state.GetAsync(ns, "ttl", Ct);
        Assert.True(found, "刚写进去（ttl=2s）应当读得到");

        var gone = await TestWait.UntilAsync(
            async () => !(await state.GetAsync(ns, "ttl", Ct)).Found,
            TimeSpan.FromSeconds(15));

        Assert.True(gone, "ttl=2s 的键应当在十几秒内过期");
        output.WriteLine($"Put {ns}/ttl ttl=2s → 立刻 Get found=True → 过期后 Get found=False");
    }

    /// <summary>
    /// Scan 按前缀过滤，并且**只**返回本命名空间里那些键。
    ///
    /// 顺带验了「中台按凭证反查插件名、强制加前缀」这件事的另一面：一套命名空间里
    /// 不该混进别处的键（虽然没法从这里看到中台拼出来的完整 Redis 键）。
    /// </summary>
    private async Task ScanIsIsolatedAsync(StateClient state, string ns)
    {
        await state.PutAsync(ns, "p1", "v1"u8.ToArray(), cancellationToken: Ct);
        await state.PutAsync(ns, "p2", "v2"u8.ToArray(), cancellationToken: Ct);
        await state.PutAsync(ns, "q1", "v3"u8.ToArray(), cancellationToken: Ct);

        var entries = await state.ScanAsync(ns, "p", 10, Ct);

        Assert.Equal(2, entries.Count);
        Assert.Equal(new[] { "p1", "p2" }, entries.Select(e => e.Key).Order().ToArray());
        Assert.Equal("v1", System.Text.Encoding.UTF8.GetString(entries.Single(e => e.Key == "p1").Value));
        output.WriteLine($"Scan {ns} prefix=p limit=10 → {string.Join(", ", entries.Select(e => $"{e.Key}={System.Text.Encoding.UTF8.GetString(e.Value)}"))}");

        // 空 prefix = 扫整个命名空间；limit 就是条数上限
        var all = await state.ScanAsync(ns, string.Empty, 1, Ct);
        Assert.Single(all);
        output.WriteLine($"Scan {ns} prefix=（空）limit=1 → {string.Join(", ", all.Select(e => e.Key))}（上限生效）");

        // 收尾：把自己的键删干净，别留一坨没有 TTL 的数据在真中台里
        var leftovers = await state.ScanAsync(ns, string.Empty, StateClient.MaxScanLimit, Ct);
        foreach (var entry in leftovers)
        {
            await state.DeleteAsync(ns, entry.Key, Ct);
        }

        var cleaned = await state.ScanAsync(ns, string.Empty, StateClient.MaxScanLimit, Ct);
        Assert.Empty(cleaned);
        output.WriteLine($"清理 {leftovers.Count} 个键 → 再 Scan 得到 0 条");
    }

    /// <summary>从日志里挑出注册那一行，作为「真的注册上了」的凭据。</summary>
    private static string Registration(StringWriter sink) =>
        sink.ToString()
            .Split('\n')
            .FirstOrDefault(line => line.Contains("\"msg\":\"已注册到中台\""))
            ?.Trim() ?? "（没找到注册日志）";

    private static bool Reachable(string address)
    {
        var url = address.Contains("://", StringComparison.Ordinal) ? address : "http://" + address;
        var uri = new Uri(url);
        try
        {
            using var probe = new TcpClient();
            return probe.ConnectAsync(uri.Host, uri.Port).Wait(TimeSpan.FromMilliseconds(500));
        }
        catch (Exception)
        {
            return false;
        }
    }

    private static int FreePort()
    {
        using var probe = new TcpListener(IPAddress.Loopback, 0);
        probe.Start();
        var port = ((IPEndPoint)probe.LocalEndpoint).Port;
        probe.Stop();
        return port;
    }
}
