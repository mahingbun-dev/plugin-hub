using System.Net;
using Grpc.Core;
using Grpc.Net.Client;
using Hub.V1;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Server.Kestrel.Core;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging;

namespace HubKit;

/// <summary>
/// 插件骨架的入口：起 gRPC 服务、向中台自注册、维持心跳、优雅退出时注销。
///
/// 插件作者只需要实现 <see cref="IPlugin"/>，其余全部由它处理：
/// <code>
/// return await PluginHost.RunAsync(new MyPlugin(), HubConfig.FromEnv());
/// </code>
/// </summary>
public static class PluginHost
{
    /// <summary>
    /// 启动插件，直到收到 SIGINT / SIGTERM（由 <c>IHostApplicationLifetime</c> 转达）。
    ///
    /// 收到终止信号时先向中台 <c>Unregister</c>，再停 gRPC 服务——
    /// 中台据此立刻摘掉实例，不必等心跳超时。
    /// </summary>
    /// <returns>进程退出码。</returns>
    public static async Task<int> RunAsync(IPlugin plugin, HubConfig config, CancellationToken cancellationToken = default)
    {
        config.Validate();
        config = config.WithDefaults();
        var log = config.Logger!;

        CheckManifest(plugin);

        var (address, port) = config.ParseListenAddr();
        var listenDisplay = FormatListen(address, port);

        // GrpcChannel 只实现 IDisposable（同步），不能 await using
        using var channel = HubChannel.Create(config);
        var registry = new GrpcRegistryChannel(new PluginRegistry.PluginRegistryClient(channel));

        // 状态客户端与注册共用这条连接：凭证由注册下发、由注册循环注入给插件，
        // 三条线的交汇处就在 Registrar（见 Registrar.RegisterOnceAsync）
        var state = new StateClient(
            new GrpcStateChannel(new HubState.HubStateClient(channel)),
            config.StateCallTimeout);

        // 网关客户端也挂在这条连接上，与状态客户端共享「凭证被拒」信号：
        // 两面用的是同一张凭证，中台重启时两面一起失效、一起换
        var gateway = new GatewayClient(
            new GrpcGatewayChannel(new PluginGateway.PluginGatewayClient(channel)),
            state.Denied,
            config.StateCallTimeout);

        var registrar = new Registrar(registry, plugin, config, log, state, gateway);

        var builder = WebApplication.CreateSlimBuilder();

        // 日志**全部**走 HubLogger 的 JSON 形状。清掉缺省的 provider 是必须的：
        // Kestrel 与 Hosting 会往 stderr 打自己的文本行（"Now listening on: ..."），
        // 混进来就破坏了「一行一条 JSON」这条采集前提。
        builder.Logging.ClearProviders();

        builder.WebHost.ConfigureKestrel(options =>
        {
            // 纯 h2c（HTTP/2 无 TLS），与中台的插件面一致。
            // 这里**不能**写 Http1AndHttp2：无 TLS 时 Kestrel 会退回 HTTP/1.1，gRPC 就谈不起来了。
            options.Listen(address, port, listen => listen.Protocols = HttpProtocols.Http2);
        });

        builder.Services.AddGrpc();
        builder.Services.AddSingleton(plugin);
        builder.Services.AddSingleton(config);
        // 日志也进容器：注册循环、gRPC 服务、宿主生命周期三处都要它，
        // 各自从 HubConfig 里掏既啰嗦又容易漏一个（漏了就是一个启动期 DI 报错）
        builder.Services.AddSingleton(log);
        builder.Services.AddSingleton(registrar);
        builder.Services.AddSingleton(sp => new PluginRuntimeService(sp.GetRequiredService<IPlugin>()));
        builder.Services.AddHostedService<RegistrarService>();

        await using var app = builder.Build();
        app.MapGrpcService<PluginRuntimeService>();

        try
        {
            await app.StartAsync(cancellationToken);
        }
        catch (Exception ex)
        {
            // 绑不上端口是最常见的一种，且原始异常（"Address already in use"）
            // 不说明是谁占的、也不说明该怎么办
            log.Error("插件 gRPC 启动失败", ("listen", listenDisplay), ("err", ex.Message));
            return 1;
        }

        log.Info("插件 gRPC 已监听", ("listen", listenDisplay), ("advertise", config.AdvertiseAddr));
        log.Info("插件已启动", ("hub", config.HubAddr), ("instance", config.InstanceId));

        try
        {
            await app.WaitForShutdownAsync(cancellationToken);
        }
        catch (OperationCanceledException)
        {
            // 调用方主动取消：走与收到信号同一条收尾路径
            await app.StopAsync(CancellationToken.None);
        }

        log.Info("插件已退出");
        return 0;
    }

    /// <summary>
    /// 在启动时就把 manifest 的明显问题挡住。
    ///
    /// 这些问题中台也会拒，但那是网络往返之后的事——本地先炸能省一轮排查。
    /// </summary>
    public static void CheckManifest(IPlugin plugin)
    {
        var manifest = plugin.Manifest;
        if (manifest is null)
        {
            throw new HubConfigException("hubkit: Manifest 返回了 null");
        }

        if (string.IsNullOrWhiteSpace(manifest.Name))
        {
            throw new HubConfigException("hubkit: manifest 缺少插件名（name）");
        }

        if (string.IsNullOrWhiteSpace(manifest.Version))
        {
            throw new HubConfigException("hubkit: manifest 缺少版本号（version）——flow 靠它锁定实例");
        }

        // 空 descriptor 本身合法：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 proto。
        // 但声明了自有类型的就必须提供，否则中台会拒（声明的类型找不到出处）。
        var descriptor = plugin.Descriptor;
        if (descriptor is null || descriptor.Length == 0)
        {
            var declared = manifest.Produces.Concat(manifest.Consumes);
            foreach (var contract in declared)
            {
                if (!Envelopes.IsWellKnownFqName(contract.FqName))
                {
                    throw new HubConfigException(
                        $"hubkit: manifest 声明了自有类型 {contract.FqName}，但 Descriptor 返回空——" +
                        "要么把它从 produces/consumes 里去掉，要么提供它的 proto");
                }
            }
        }
    }

    /// <summary>把监听地址渲染成 Go 侧那种形状（<c>[::]:9000</c> / <c>127.0.0.1:9000</c>）。</summary>
    private static string FormatListen(IPAddress address, int port)
    {
        var host = address.AddressFamily == System.Net.Sockets.AddressFamily.InterNetworkV6
            ? $"[{address}]"
            : address.ToString();
        return $"{host}:{port}";
    }
}

/// <summary>与中台之间的 gRPC 通道。抽出来是为了让 TLS 逃生口的设置只有一处。</summary>
public static class HubChannel
{
    /// <summary>按配置建通道。地址没写 scheme 时补 <c>http://</c>。</summary>
    public static GrpcChannel Create(HubConfig config) =>
        Create(config.HubAddr, config.TlsMaxProtocols());

    /// <summary>按裸地址建通道（调试工具直连插件时用，那条链路不套 TLS 逃生口）。</summary>
    public static GrpcChannel Create(string address) => Create(address, null);

    private static GrpcChannel Create(string address, System.Security.Authentication.SslProtocols? protocols)
    {
        var endpoint = address.Trim();
        if (!endpoint.Contains("://", StringComparison.Ordinal))
        {
            // Go 的 grpc.NewClient 接受不带 scheme 的 target（隐含明文），
            // 这里补上，让「照 Go 的写法粘过来」不至于在 Uri 解析上炸掉
            endpoint = "http://" + endpoint;
        }

        var handler = new SocketsHttpHandler();
        if (protocols is { } enabled)
        {
            handler.SslOptions = new System.Net.Security.SslClientAuthenticationOptions
            {
                EnabledSslProtocols = enabled,
            };
        }

        return GrpcChannel.ForAddress(endpoint, new GrpcChannelOptions { HttpHandler = handler });
    }
}

/// <summary>
/// <c>hub.v1.PluginRuntime</c> 的服务端实现。中台 → 插件那条链路。
///
/// 插件作者不实现它——它只是把 <see cref="IPlugin"/> 翻译成 gRPC 语义：
/// 哪些失败是「业务结果」、哪些是「插件异常」。
/// </summary>
public sealed class PluginRuntimeService(IPlugin plugin) : PluginRuntime.PluginRuntimeBase
{
    public override Task<PluginManifest> Describe(DescribeRequest request, ServerCallContext context) =>
        Task.FromResult(plugin.Manifest);

    public override async Task<ValidateResponse> Validate(ValidateRequest request, ServerCallContext context)
    {
        if (request.Envelope is null)
        {
            return Envelopes.Invalid(Envelopes.Issue("envelope", "缺少信封"));
        }

        try
        {
            return await plugin.ValidateAsync(request.Envelope, context.CancellationToken);
        }
        catch (Exception ex)
        {
            throw new RpcException(new Status(StatusCode.Internal, $"校验器执行失败: {ex.Message}"));
        }
    }

    public override async Task<HandleResponse> Handle(HandleRequest request, ServerCallContext context)
    {
        if (request.Envelope is null)
        {
            throw new RpcException(new Status(StatusCode.InvalidArgument, "缺少信封"));
        }

        try
        {
            var output = await plugin.HandleAsync(request.Envelope, context.CancellationToken);
            if (output is null)
            {
                // 空信封中台会当成插件异常——在这里就说清楚，别让它到中台才变成一句难懂的话
                throw new RpcException(new Status(StatusCode.Internal, "插件体返回了空信封"));
            }

            return new HandleResponse { Envelope = output };
        }
        catch (RpcException)
        {
            throw;
        }
        catch (Exception ex)
        {
            // 插件自己的错误原样上报：中台会把它归到「插件调用失败」，调用方据此重试
            throw new RpcException(new Status(StatusCode.Internal, $"插件处理失败: {ex.Message}"));
        }
    }

    public override Task HandleStream(
        HandleRequest request,
        IServerStreamWriter<HandleResponse> responseStream,
        ServerCallContext context) =>
        throw new RpcException(new Status(
            StatusCode.Unimplemented,
            "流式处理尚未实现（见 docs/design.md 的载荷边界，随 M3 落地）"));

    public override Task<HealthResponse> Health(HealthRequest request, ServerCallContext context) =>
        Task.FromResult(new HealthResponse { Healthy = true, Message = "ok" });
}

/// <summary>
/// 把 <see cref="Registrar"/> 挂进宿主生命周期。
///
/// 用宿主而不是自己 new 一个线程，是为了拿到 <c>IHostApplicationLifetime</c>：
/// SIGTERM / SIGINT 会转成一次优雅停机，<see cref="StopAsync"/> 就是那个钩子——
/// 插件主动注销的机会只有这一次。
/// </summary>
internal sealed class RegistrarService(Registrar registrar, HubLogger log) : IHostedService
{
    private CancellationTokenSource? _cts;
    private Task? _loop;

    public Task StartAsync(CancellationToken cancellationToken)
    {
        _cts = new CancellationTokenSource();
        _loop = Task.Run(() => registrar.RunAsync(_cts.Token), CancellationToken.None);
        return Task.CompletedTask;
    }

    public async Task StopAsync(CancellationToken cancellationToken)
    {
        // 先注销再停心跳：反过来的话，最后一次心跳可能把注销掉的实例又"救"回来
        await registrar.UnregisterAsync();

        if (_cts is not null)
        {
            await _cts.CancelAsync();
        }

        if (_loop is not null)
        {
            // 给收尾一个有上限的等待，别让一个卡住的注册请求把停机拖死
            await Task.WhenAny(_loop, Task.Delay(TimeSpan.FromSeconds(3), cancellationToken));
        }

        _cts?.Dispose();
        log.Debug("注册循环已停止");
    }
}
