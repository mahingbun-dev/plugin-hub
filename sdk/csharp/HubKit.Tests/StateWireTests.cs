using System.Net;
using System.Net.Sockets;
using Grpc.Core;
using Grpc.Net.Client;
using Hub.V1;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 走**真连接**的那部分行为：<see cref="GrpcStateChannel"/> 到底把什么放上了线路。
///
/// 假通道能证明「客户端把凭证放进了 <see cref="CallOptions"/>」，证明不了
/// 「gRPC 真的按 deadline 结束、取消时给的是 Cancelled」——那是 grpc-dotnet 的事，
/// 而插件作者看得到的就是这些错误码。中台那侧的行为（身份反查、前缀拼接、TTL）
/// 由 <see cref="StateIntegrationTests"/> 对着真中台跑。
/// </summary>
public class StateWireTests
{
    private static CancellationToken Ct => TestContext.Current.CancellationToken;

    /// <summary>
    /// 上限到点必须表现为 <c>DeadlineExceeded</c>，而不是「连接错误」之类。
    ///
    /// 这正是把超时放进 <c>CallOptions.Deadline</c>（而不是自己 <c>CancelAfter</c>）的理由：
    /// 前者由 gRPC 自己判、错误码是 DeadlineExceeded；后者会变成 Cancelled，
    /// 调用方就分不清「中台慢」和「我自己取消了」。
    /// </summary>
    [Fact]
    public async Task 对端不响应时按上限以DeadlineExceeded结束()
    {
        using var silent = new SilentPeer();
        using var channel = GrpcChannel.ForAddress($"http://127.0.0.1:{silent.Port}");
        var state = new StateClient(
            new GrpcStateChannel(new HubState.HubStateClient(channel)),
            TimeSpan.FromMilliseconds(300));

        var start = DateTime.UtcNow;
        var ex = await Assert.ThrowsAsync<RpcException>(() => state.GetAsync("ns", "k", Ct));
        var elapsed = DateTime.UtcNow - start;

        Assert.Equal(StatusCode.DeadlineExceeded, ex.StatusCode);
        Assert.True(elapsed < TimeSpan.FromSeconds(5), $"应在 300ms 附近结束，实际 {elapsed}");
    }

    /// <summary>调用方取消与「超上限」是两种错误，真连接上也不能混。</summary>
    [Fact]
    public async Task 调用方取消在真连接上表现为Cancelled()
    {
        using var silent = new SilentPeer();
        using var channel = GrpcChannel.ForAddress($"http://127.0.0.1:{silent.Port}");
        // 上限设得足够大，保证先到的是调用方自己的取消
        var state = new StateClient(
            new GrpcStateChannel(new HubState.HubStateClient(channel)),
            TimeSpan.FromSeconds(30));

        using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(100));

        var ex = await Assert.ThrowsAsync<RpcException>(() => state.GetAsync("ns", "k", cts.Token));

        Assert.Equal(StatusCode.Cancelled, ex.StatusCode);
    }

    /// <summary>
    /// 预算已经用光（令牌早已取消）时**根本不发请求**：没有理由再把一次注定死掉的调用
    /// 放上线路。判据是 peer 上有没有建立过连接——连上了就等于发过。
    ///
    /// 顺带钉住「已取消的令牌也走 <see cref="RpcException"/>」：插件 catch 一种异常类型
    /// 就够，不必在业务代码里额外接一个 <c>OperationCanceledException</c>。
    /// </summary>
    [Fact]
    public async Task 令牌已取消时不发请求()
    {
        using var silent = new SilentPeer();
        using var channel = GrpcChannel.ForAddress($"http://127.0.0.1:{silent.Port}");
        var state = new StateClient(
            new GrpcStateChannel(new HubState.HubStateClient(channel)),
            TimeSpan.FromSeconds(30));

        using var cts = new CancellationTokenSource();
        cts.Cancel();

        var start = DateTime.UtcNow;
        var ex = await Assert.ThrowsAsync<RpcException>(() => state.GetAsync("ns", "k", cts.Token));
        var elapsed = DateTime.UtcNow - start;

        Assert.Equal(StatusCode.Cancelled, ex.StatusCode);
        Assert.True(elapsed < TimeSpan.FromSeconds(1), $"应当立刻失败，实际 {elapsed}");
        Assert.Equal(0, silent.Connections);
    }

    /// <summary>
    /// 一个只接受 TCP 连接、从不回一个字节的 peer。
    ///
    /// 用它而不是一个「实现 HubState 的假服务端」：中台与 SDK 用的是同一份
    /// <c>state.proto</c>，而 SDK 只生成了**客户端**那一半
    /// （<c>HubKit.csproj</c> 里 <c>GrpcServices="Client"</c>），测试里为了托一个假服务端
    /// 去改 SDK 的生成配置，等于让被测物为测试让路。
    ///
    /// 「连上就挂着」正好是验 deadline 需要的：调用会一直等到 deadline 到点。
    /// </summary>
    private sealed class SilentPeer : IDisposable
    {
        private readonly TcpListener _listener;
        private readonly List<TcpClient> _accepted = [];
        private readonly CancellationTokenSource _cts = new();

        public SilentPeer()
        {
            _listener = new TcpListener(IPAddress.Loopback, 0);
            _listener.Start();
            Port = ((IPEndPoint)_listener.LocalEndpoint).Port;

            _ = Task.Run(async () =>
            {
                try
                {
                    while (!_cts.IsCancellationRequested)
                    {
                        var client = await _listener.AcceptTcpClientAsync(_cts.Token);
                        lock (_accepted)
                        {
                            // 收下就不管，也**不关**：关掉会让 gRPC 看到连接断开，
                            // 那验的就变成「连接错误」而不是「超时」了
                            _accepted.Add(client);
                        }
                    }
                }
                catch (Exception)
                {
                    // 停机时的取消，正常
                }
            });
        }

        public int Port { get; }

        /// <summary>peer 上建立过几条连接（= gRPC 真的拨过来了）。</summary>
        public int Connections
        {
            get
            {
                lock (_accepted)
                {
                    return _accepted.Count;
                }
            }
        }

        public void Dispose()
        {
            _cts.Cancel();
            _listener.Stop();
            lock (_accepted)
            {
                foreach (var client in _accepted)
                {
                    client.Dispose();
                }

                _accepted.Clear();
            }

            _cts.Dispose();
        }
    }
}
