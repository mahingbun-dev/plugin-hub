using System.Net;
using System.Net.Sockets;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 宿主整体能不能起来、能不能干净地停下来。
///
/// 这条测试的由来是实测抓到的一个 DI 漏配：注册循环要一个 <c>HubLogger</c>，
/// 而那时候只有 <c>HubConfig</c> 进了容器，插件启动直接报
/// 「Unable to resolve service for type 'HubKit.HubLogger'」——所有单元测试全绿，
/// 插件一个都起不来。单元测试测不到容器的装配，只有把宿主真的拉起来才测得到。
/// </summary>
public class HostTests
{
    private static int FreePort()
    {
        // 绑 0 拿一个系统分配的空闲端口再放掉。拿到与用上之间有窗口期，
        // 但本机测试里这个窗口足够小；真要撞上了会表现为「端口被占」的启动失败，
        // 而不是一条难懂的假绿
        using var probe = new TcpListener(IPAddress.Loopback, 0);
        probe.Start();
        var port = ((IPEndPoint)probe.LocalEndpoint).Port;
        probe.Stop();
        return port;
    }

    [Fact]
    public async Task 宿主起得来也能在取消时干净退出()
    {
        var port = FreePort();
        var sink = new StringWriter();
        var config = new HubConfig
        {
            // 指向一个没人听的端口：注册会一直失败并重试。这是**刻意允许**的行为
            // （中台可能比插件晚起来），顺带把「注册失败不阻断启动」也验了
            HubAddr = "http://127.0.0.1:1",
            AdvertiseAddr = $"http://127.0.0.1:{port}",
            ListenAddr = $":{port}",
            InstanceId = "host-test-1",
            RetryInterval = TimeSpan.FromMilliseconds(20),
            Logger = new HubLogger(HubLogLevel.Debug, sink),
        };

        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var run = PluginHost.RunAsync(new FakePlugin(), config, cts.Token);

        var started = await TestWait.Until(
            () => sink.ToString().Contains("插件 gRPC 已监听"), TimeSpan.FromSeconds(15));

        Assert.True(started, $"宿主没起来：\n{sink}");
        Assert.Contains("插件已启动", sink.ToString());

        await cts.CancelAsync();
        var code = await run;

        Assert.Equal(0, code);
        Assert.Contains("插件已退出", sink.ToString());
    }

    [Fact]
    public async Task 运行时自检能对着真在跑的插件跑完()
    {
        var port = FreePort();
        var sink = new StringWriter();
        var config = new HubConfig
        {
            HubAddr = "http://127.0.0.1:1",
            AdvertiseAddr = $"http://127.0.0.1:{port}",
            ListenAddr = $":{port}",
            InstanceId = "host-test-3",
            RetryInterval = TimeSpan.FromMilliseconds(50),
            Logger = new HubLogger(HubLogLevel.Debug, sink),
        };

        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(60));
        var run = PluginHost.RunAsync(new FakePlugin(), config, cts.Token);

        var started = await TestWait.Until(
            () => sink.ToString().Contains("插件 gRPC 已监听"), TimeSpan.FromSeconds(15));
        Assert.True(started, $"宿主没起来：\n{sink}");

        // 走真 gRPC：这一条把「服务端注册对了没有」也一并验了——
        // 少注册一个方法、方法名拼错，在这里都会露出来
        var report = await Conformance.RuntimeAsync(
            $"http://127.0.0.1:{port}", TestContext.Current.CancellationToken);

        Assert.True(report.Passed, report.ToString());
        Assert.Equal(
            ["Health 可应答", "Describe 可应答", "校验器对空信封不崩", "校验器可处理 JSON 载荷", "插件体返回信封"],
            report.Checks.Select(c => c.Name));

        await cts.CancelAsync();
        await run;
    }

    [Fact]
    public async Task 端口被占时给出可照做的失败而不是崩()
    {
        var port = FreePort();
        using var squatter = new TcpListener(IPAddress.IPv6Any, port);
        squatter.Start();

        var sink = new StringWriter();
        var config = new HubConfig
        {
            HubAddr = "http://127.0.0.1:1",
            AdvertiseAddr = $"http://127.0.0.1:{port}",
            ListenAddr = $":{port}",
            InstanceId = "host-test-2",
            Logger = new HubLogger(HubLogLevel.Debug, sink),
        };

        var code = await PluginHost.RunAsync(new FakePlugin(), config, CancellationToken.None);

        Assert.Equal(1, code);
        Assert.Contains("插件 gRPC 启动失败", sink.ToString());
    }
}
