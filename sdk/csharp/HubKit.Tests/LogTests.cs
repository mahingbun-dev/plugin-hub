using System.Text.Json;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 日志形状的测试。
///
/// 形状是与 Go 侧 slog 的**约定**，不是实现细节：六门语言的插件跑在同一个中台旁边，
/// 采集规则、告警规则、jq 配方只该有一套。所以这里逐字段钉住。
/// </summary>
public class LogTests
{
    private static (string Raw, JsonElement Parsed) Capture(Action<HubLogger> emit, HubLogLevel level = HubLogLevel.Info)
    {
        var sink = new StringWriter();
        var log = new HubLogger(level, sink);
        emit(log);

        var raw = sink.ToString();
        var lines = raw.Split('\n', StringSplitOptions.RemoveEmptyEntries);
        Assert.Single(lines);
        return (lines[0], JsonDocument.Parse(lines[0]).RootElement);
    }

    [Fact]
    public void 一条记录一行JSON且字段齐备()
    {
        var (_, parsed) = Capture(log => log.Info(
            "已注册到中台",
            ("plugin", "order-reader"),
            ("version", "0.1.0"),
            ("instance", "host-1234")));

        Assert.Equal("INFO", parsed.GetProperty("level").GetString());
        Assert.Equal("已注册到中台", parsed.GetProperty("msg").GetString());
        Assert.Equal("order-reader", parsed.GetProperty("plugin").GetString());
        Assert.Equal("0.1.0", parsed.GetProperty("version").GetString());
        Assert.Equal("host-1234", parsed.GetProperty("instance").GetString());

        // time 是 RFC3339 带偏移，且能解析回来
        var time = parsed.GetProperty("time").GetString()!;
        Assert.Contains('T', time);
        Assert.True(DateTimeOffset.TryParse(time, out _), time);
    }

    [Fact]
    public void 多行内容会被转义在单个字段里()
    {
        // 采集侧是按行解析的：一条记录里混进真换行，多出来的行会变成没有 msg 的孤儿
        var (raw, parsed) = Capture(log => log.Warn("中台提示", ("detail", "第一行\n第二行")));

        Assert.DoesNotContain("\n", raw);
        // 字段里读回来仍是两行——转义不该把内容也吃掉
        Assert.Equal("第一行\n第二行", parsed.GetProperty("detail").GetString());
    }

    [Fact]
    public void 中文与时区里的加号不被转义成unicode转义()
    {
        // 这条是实测抓出来的：.NET 缺省的 JSON 编码器会把中文写成 \\u5FC3\\u8DF3、
        // 把时区里的加号写成 \\u002B，而 Go 侧 slog 是原样写 UTF-8 的。
        // 形状一散，"grep 心跳失败" 这种最朴素的排查动作会什么都搜不到——
        // 而且它不会报错，只会静默地搜不到。
        var (raw, _) = Capture(log => log.Info(
            "已注册到中台",
            ("plugin", "order-reader")));

        Assert.Contains("\"msg\":\"已注册到中台\"", raw);
        Assert.Contains("\"plugin\":\"order-reader\"", raw);
        Assert.DoesNotContain("\\u", raw);
        // 时区偏移就是 +08:00 这个样子，不是被转义过的写法
        Assert.Matches(@"\d{2}:\d{2}:\d{2}(\.\d+)?[+Z]", raw);
    }

    [Theory]
    [InlineData(HubLogLevel.Debug, "debug", true)]
    [InlineData(HubLogLevel.Info, "debug", false)]
    [InlineData(HubLogLevel.Info, "info", true)]
    [InlineData(HubLogLevel.Warn, "info", false)]
    [InlineData(HubLogLevel.Warn, "warn", true)]
    [InlineData(HubLogLevel.Error, "warn", false)]
    [InlineData(HubLogLevel.Error, "error", true)]
    public void 低于配置级别的记录被丢掉(HubLogLevel min, string emit, bool expected)
    {
        var sink = new StringWriter();
        var log = new HubLogger(min, sink);

        switch (emit)
        {
            case "debug": log.Debug("d"); break;
            case "info": log.Info("i"); break;
            case "warn": log.Warn("w"); break;
            case "error": log.Error("e"); break;
            default: throw new InvalidOperationException($"未知的级别：{emit}");
        }

        Assert.Equal(expected, sink.ToString().Length > 0);
    }

    [Fact]
    public void 十进制的秒级时长不落成纳秒整数()
    {
        // Go 侧踩过这个：直接把 Duration 落进 JSON 会得到 5000000000，
        // 没人认得出那是 5 秒
        Assert.Equal("5s", Registrar.Duration(TimeSpan.FromSeconds(5)));
        Assert.Equal("10ms", Registrar.Duration(TimeSpan.FromMilliseconds(10)));
        Assert.Equal("1.5s", Registrar.Duration(TimeSpan.FromMilliseconds(1500)));
    }

    [Fact]
    public void 异常作为一列而不是展开()
    {
        var (raw, parsed) = Capture(log => log.Error("注册未通过，稍后重试", ("err", new InvalidOperationException("连接被拒绝"))));

        Assert.DoesNotContain("\n", raw);
        Assert.Equal("连接被拒绝", parsed.GetProperty("err").GetString());
    }
}
