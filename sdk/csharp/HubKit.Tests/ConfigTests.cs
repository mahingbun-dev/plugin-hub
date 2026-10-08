using System.Net;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 配置来自环境变量——这是六门语言共同的接入契约，字段名与缺省值都不能各写各的。
///
/// 动环境变量的测试都放在**这一个类**里：xunit 按类并行，多几个类一起改环境变量，
/// 就会变成「偶尔红一次、重跑又绿了」那种最难查的失败。
/// </summary>
public class ConfigTests
{
    /// <summary>临时设定一组环境变量，<c>Dispose</c> 时按原值还原。</summary>
    private sealed class EnvScope(params (string Name, string? Value)[] entries) : IDisposable
    {
        private readonly List<(string Name, string? Old)> _saved =
            entries.Select(e => (e.Name, Environment.GetEnvironmentVariable(e.Name))).ToList();

        public EnvScope Apply()
        {
            foreach (var (name, value) in entries)
            {
                Environment.SetEnvironmentVariable(name, value);
            }

            return this;
        }

        public void Dispose()
        {
            foreach (var (name, old) in _saved)
            {
                Environment.SetEnvironmentVariable(name, old);
            }
        }
    }

    [Fact]
    public void 缺必填项时报出两个名字()
    {
        var ex = Assert.Throws<HubConfigException>(() => new HubConfig().Validate());

        // 一次把缺的都说出来：只报第一个的话，补完一个再跑一次才发现还缺一个
        Assert.Contains("HUB_ADDR", ex.Message);
        Assert.Contains("HUB_ADVERTISE_ADDR", ex.Message);
    }

    [Fact]
    public void 隧道口只接受两个取值()
    {
        var config = new HubConfig { HubAddr = "http://h", AdvertiseAddr = "http://a", TlsMaxVersion = "1.1" };
        Assert.Throws<HubConfigException>(() => config.Validate());

        foreach (var ok in new[] { "", "1.2", "1.3" })
        {
            new HubConfig { HubAddr = "http://h", AdvertiseAddr = "http://a", TlsMaxVersion = ok }.Validate();
        }
    }

    [Fact]
    public void 缺省值补齐()
    {
        var config = new HubConfig { HubAddr = "http://h", AdvertiseAddr = "http://a" }.WithDefaults();

        Assert.Equal(":9000", config.ListenAddr);
        Assert.Equal(HubConfig.RegisterRetryInterval, config.RetryInterval);
        Assert.NotNull(config.Logger);

        // 缺省实例标识是「主机名-PID」，与 Go 侧同形
        Assert.Matches(@"^.+-[0-9]+$", config.InstanceId);
        Assert.EndsWith($"-{Environment.ProcessId}", config.InstanceId);
    }

    [Fact]
    public void 补齐缺省值不会改动原对象()
    {
        var original = new HubConfig { HubAddr = "http://h", AdvertiseAddr = "http://a" };
        _ = original.WithDefaults();

        Assert.Equal("", original.ListenAddr);
    }

    [Theory]
    [InlineData(":9000", "::", 9000)]
    [InlineData("0.0.0.0:9211", "0.0.0.0", 9211)]
    [InlineData("127.0.0.1:19211", "127.0.0.1", 19211)]
    [InlineData("[::]:9211", "::", 9211)]
    [InlineData("[::1]:9211", "::1", 9211)]
    [InlineData("localhost:9211", "127.0.0.1", 9211)]
    [InlineData("", "::", 9000)]
    public void 监听地址的解析(string raw, string wantHost, int wantPort)
    {
        var config = new HubConfig { ListenAddr = raw, HubAddr = "http://h", AdvertiseAddr = "http://a" };
        var (address, port) = config.ParseListenAddr();

        Assert.Equal(IPAddress.Parse(wantHost), address);
        Assert.Equal(wantPort, port);
    }

    [Theory]
    [InlineData("9000")]      // 少了冒号
    [InlineData(":not-a-port")]
    [InlineData(":99999")]
    [InlineData("example.com:9000")] // 域名解析不了：这里刻意不支持，免得缺省行为随 DNS 变
    public void 监听地址写错时报出可照做的错(string raw)
    {
        var config = new HubConfig { ListenAddr = raw, HubAddr = "http://h", AdvertiseAddr = "http://a" };
        Assert.Throws<HubConfigException>(() => config.ParseListenAddr());
    }

    [Fact]
    public void 环境变量是配置的唯一来源()
    {
        using var _ = new EnvScope(
            ("HUB_ADDR", "http://hub:8093"),
            ("HUB_ADVERTISE_ADDR", "http://me:19211"),
            ("HUB_LISTEN_ADDR", ":19211"),
            ("HUB_INSTANCE_ID", "explicit-1"),
            ("HUB_TLS_MAX_VERSION", "1.2"),
            ("HUB_LOG_LEVEL", "warn")).Apply();

        var config = HubConfig.FromEnv();

        Assert.Equal("http://hub:8093", config.HubAddr);
        Assert.Equal("http://me:19211", config.AdvertiseAddr);
        Assert.Equal(":19211", config.ListenAddr);
        Assert.Equal("explicit-1", config.InstanceId);
        Assert.Equal("1.2", config.TlsMaxVersion);
        Assert.Equal(HubLogLevel.Warn, config.Logger!.MinLevel);
        Assert.Equal(System.Security.Authentication.SslProtocols.Tls12, config.TlsMaxProtocols());
    }

    [Fact]
    public void 认不出的日志级别落到info()
    {
        using var _ = new EnvScope(("HUB_LOG_LEVEL", "verbose")).Apply();
        Assert.Equal(HubLogLevel.Info, HubConfig.FromEnv().Logger!.MinLevel);
    }

    [Fact]
    public void 留空时隧道跟随默认()
    {
        Assert.Null(new HubConfig { TlsMaxVersion = "" }.TlsMaxProtocols());
        Assert.Null(new HubConfig { TlsMaxVersion = "  " }.TlsMaxProtocols());
        Assert.Equal(
            System.Security.Authentication.SslProtocols.Tls13,
            new HubConfig { TlsMaxVersion = "1.3" }.TlsMaxProtocols());
    }
}
