using System.Net;
using System.Security.Authentication;

namespace HubKit;

/// <summary>
/// 骨架的运行参数。与 Go 侧的 <c>hubkit.Config</c> 逐字段对应——
/// 同一份环境变量契约要在六门语言里表现一致，字段名与缺省值都不能各写各的。
/// </summary>
public sealed class HubConfig
{
    /// <summary>本插件 gRPC 的监听地址，缺省 <see cref="DefaultListenAddr"/>。</summary>
    public const string DefaultListenAddr = ":9000";

    /// <summary>中台没告诉我们心跳周期时的兜底值。</summary>
    public static readonly TimeSpan HeartbeatFallbackInterval = TimeSpan.FromSeconds(10);

    /// <summary>注册失败后的重试间隔。</summary>
    public static readonly TimeSpan RegisterRetryInterval = TimeSpan.FromSeconds(5);

    /// <summary>
    /// 中台插件面地址。生产形如 <c>https://hub.example.com:8094</c>（经 nginx 的 TLS 终结）。
    ///
    /// 中台与插件**不要求同机**——插件可以部署在任何能连上这个地址的地方。
    /// </summary>
    public string HubAddr { get; set; } = string.Empty;

    /// <summary>
    /// 中台可达的本插件地址，例如 <c>http://10.0.0.5:9000</c>。
    ///
    /// 中台在注册时会连它做**可达性探测**，所以必须是「从中台那边拨得通」的地址，
    /// 而不是本机视角的 localhost——这是插件接入时最容易踩的坑。
    /// </summary>
    public string AdvertiseAddr { get; set; } = string.Empty;

    /// <summary>本插件 gRPC 的监听地址，缺省 <see cref="DefaultListenAddr"/>（<c>:9000</c>）。</summary>
    public string ListenAddr { get; set; } = string.Empty;

    /// <summary>
    /// 实例标识，缺省「主机名-PID」。
    ///
    /// 同一 ID 重复注册视为进程重启（中台会刷新地址与心跳），不产生重复实例。
    /// </summary>
    public string InstanceId { get; set; } = string.Empty;

    /// <summary>
    /// 限制与中台之间 TLS 的**最高**版本：<c>"1.2"</c> 或 <c>"1.3"</c>，
    /// 留空则跟随 .NET 的默认（当前会协商到 1.3）。
    ///
    /// 存在的理由与 Go 侧同一个：某些网络路径上的中间设备会重置 TLS 1.3 握手，
    /// 表现为 <c>connection reset by peer</c>，而错误信息里完全看不出是网络设备干的。
    /// 把上限压到 <c>"1.2"</c> 即可绕开。
    ///
    /// 它是**逃生口，不是默认配置**：压到 1.2 是有代价的（1.3 的握手更短、前向保密更强）。
    /// </summary>
    public string TlsMaxVersion { get; set; } = string.Empty;

    /// <summary>
    /// 注册失败后的重试间隔，缺省 <see cref="RegisterRetryInterval"/>。
    ///
    /// 测试里会调到毫秒级；生产一般不必改。
    /// </summary>
    public TimeSpan RetryInterval { get; set; } = TimeSpan.Zero;

    /// <summary>
    /// 心跳周期的换算覆盖值，**只有测试会设它**。
    ///
    /// 中台回执里的周期单位是**整数秒**，而「等 1 秒才发下一拍」对测试来说太慢：
    /// 自愈那条路径（心跳被判要求重注册 → 重走注册）要等好几拍才走得到。
    /// 设成毫秒级就能把整条路径压进一次测试里。
    /// </summary>
    public TimeSpan? HeartbeatIntervalOverride { get; set; }

    /// <summary>
    /// 单次 HubState 调用的时间上限，缺省 <see cref="StateClient.DefaultCallTimeout"/>。
    ///
    /// 远小于信封预算（HTTP 面缺省 30s），让状态调用先于业务调用放弃：中台一次卡顿
    /// 最坏能吃掉十几秒（PG 取连接 + 查询 + Redis 响应超时），不设上限时这些时间全部
    /// 从调用方的预算里扣，随后的业务调用会拿到一份已经用光的预算。
    ///
    /// 它是**上限而非承诺**：调用方自己的 <see cref="CancellationToken"/> 取消得更早就按调用方的来。
    /// </summary>
    public TimeSpan StateCallTimeout { get; set; } = TimeSpan.Zero;

    /// <summary>结构化日志，缺省按 <c>HUB_LOG_LEVEL</c> 打到 stderr。</summary>
    public HubLogger? Logger { get; set; }

    /// <summary>检查必填项，并给出能直接照做的提示。</summary>
    public void Validate()
    {
        var missing = new List<string>();
        if (string.IsNullOrWhiteSpace(HubAddr))
        {
            missing.Add("HUB_ADDR（中台插件面地址）");
        }

        if (string.IsNullOrWhiteSpace(AdvertiseAddr))
        {
            missing.Add("HUB_ADVERTISE_ADDR（中台可达的本插件地址）");
        }

        if (missing.Count > 0)
        {
            throw new HubConfigException($"hubkit: 缺少必填配置 {string.Join("、", missing)}");
        }

        var tls = TlsMaxVersion.Trim();
        if (tls is not ("" or "1.2" or "1.3"))
        {
            throw new HubConfigException($"hubkit: HUB_TLS_MAX_VERSION 只接受 \"1.2\" 或 \"1.3\"，收到 \"{tls}\"");
        }
    }

    /// <summary>补齐缺省值。返回新对象，不改原对象。</summary>
    public HubConfig WithDefaults()
    {
        var copy = (HubConfig)MemberwiseClone();

        if (string.IsNullOrEmpty(copy.ListenAddr))
        {
            copy.ListenAddr = DefaultListenAddr;
        }

        if (string.IsNullOrEmpty(copy.InstanceId))
        {
            // 用 Dns.GetHostName() 而不是 Environment.MachineName：后者在 macOS 上
            // 给的是 NetBIOS 那种短名，而 Go 侧 os.Hostname() 给的是完整主机名。
            // 两侧缺省值形状不一致的话，「这个实例是谁」在多语言混布时就对不上。
            string host;
            try
            {
                host = Dns.GetHostName();
            }
            catch (Exception)
            {
                host = "unknown-host";
            }

            copy.InstanceId = $"{host}-{Environment.ProcessId}";
        }

        if (copy.RetryInterval <= TimeSpan.Zero)
        {
            copy.RetryInterval = RegisterRetryInterval;
        }

        if (copy.StateCallTimeout <= TimeSpan.Zero)
        {
            copy.StateCallTimeout = StateClient.DefaultCallTimeout;
        }

        copy.Logger ??= HubLogger.FromEnv();
        return copy;
    }

    /// <summary>
    /// 从环境变量读取配置；缺省值由 <see cref="WithDefaults"/> 补齐。
    ///
    /// <code>
    /// HUB_ADDR            中台插件面地址（必填）
    /// HUB_ADVERTISE_ADDR  本插件对外可达地址（必填）
    /// HUB_LISTEN_ADDR     本插件监听地址（缺省 :9000）
    /// HUB_INSTANCE_ID     实例标识（缺省 主机名-PID）
    /// HUB_TLS_MAX_VERSION TLS 最高版本，1.2 或 1.3（缺省跟随 .NET）
    /// HUB_LOG_LEVEL       debug / info / warn / error（缺省 info）
    /// </code>
    /// </summary>
    public static HubConfig FromEnv()
    {
        return new HubConfig
        {
            HubAddr = Environment.GetEnvironmentVariable("HUB_ADDR") ?? string.Empty,
            AdvertiseAddr = Environment.GetEnvironmentVariable("HUB_ADVERTISE_ADDR") ?? string.Empty,
            ListenAddr = Environment.GetEnvironmentVariable("HUB_LISTEN_ADDR") ?? string.Empty,
            InstanceId = Environment.GetEnvironmentVariable("HUB_INSTANCE_ID") ?? string.Empty,
            TlsMaxVersion = Environment.GetEnvironmentVariable("HUB_TLS_MAX_VERSION") ?? string.Empty,
            Logger = HubLogger.FromEnv(),
        };
    }

    /// <summary>把 <see cref="TlsMaxVersion"/> 翻成 .NET 的枚举；留空时返回 null（跟随默认）。</summary>
    public SslProtocols? TlsMaxProtocols() => TlsMaxVersion.Trim() switch
    {
        "1.2" => SslProtocols.Tls12,
        "1.3" => SslProtocols.Tls13,
        _ => null,
    };

    /// <summary>
    /// 解析监听地址。
    ///
    /// 认 <c>:9000</c>（Go 的写法，也是缺省值）、<c>0.0.0.0:9000</c>、<c>127.0.0.1:9000</c>、
    /// <c>[::]:9000</c>、<c>localhost:9000</c>。
    ///
    /// <c>:9000</c> 映射到 IPv6Any 而不是 IPv4Any：Kestrel 的 IPv6Any 在双栈系统上
    /// 同时收 v4 与 v6，语义与 Go 的 <c>:9000</c> 一致；映射成 IPv4Any 的话，
    /// 中台从 IPv6 侧拨过来就连不上了。
    /// </summary>
    public (IPAddress Address, int Port) ParseListenAddr()
    {
        var raw = ListenAddr.Trim();
        if (raw.Length == 0)
        {
            raw = DefaultListenAddr;
        }

        string host;
        string portText;

        if (raw.StartsWith('['))
        {
            var close = raw.IndexOf(']');
            if (close < 0)
            {
                throw new HubConfigException($"hubkit: HUB_LISTEN_ADDR 里的 IPv6 地址缺少 ']'：{raw}");
            }

            host = raw[1..close];
            var rest = raw[(close + 1)..];
            if (!rest.StartsWith(':'))
            {
                throw new HubConfigException($"hubkit: HUB_LISTEN_ADDR 缺少端口：{raw}");
            }

            portText = rest[1..];
        }
        else
        {
            var colon = raw.LastIndexOf(':');
            if (colon < 0)
            {
                throw new HubConfigException($"hubkit: HUB_LISTEN_ADDR 缺少端口（形如 :9000）：{raw}");
            }

            host = raw[..colon];
            portText = raw[(colon + 1)..];
        }

        if (!int.TryParse(portText, out var port) || port is < 0 or > 65535)
        {
            throw new HubConfigException($"hubkit: HUB_LISTEN_ADDR 的端口非法：{portText}");
        }

        IPAddress address;
        if (host.Length == 0 || host == "*")
        {
            address = IPAddress.IPv6Any;
        }
        else if (host is "localhost")
        {
            address = IPAddress.Loopback;
        }
        else if (!IPAddress.TryParse(host, out var parsed))
        {
            throw new HubConfigException($"hubkit: HUB_LISTEN_ADDR 里的主机名无法解析（请写 IP，别写域名）：{host}");
        }
        else
        {
            address = parsed;
        }

        return (address, port);
    }
}

/// <summary>配置不合法。与 Go 侧 <c>Config.Validate</c> 返回的错误对应。</summary>
public sealed class HubConfigException(string message) : Exception(message);
