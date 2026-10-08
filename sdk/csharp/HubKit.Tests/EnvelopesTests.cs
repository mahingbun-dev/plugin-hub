using Google.Protobuf.WellKnownTypes;
using Hub.V1;
using HubKit;
using Xunit;

namespace HubKit.Tests;

public class EnvelopesTests
{
    [Fact]
    public void JSON载荷往返()
    {
        var envelope = new Envelope { MessageId = "t-1" };
        var outbound = Envelopes.WithPayloadJson(envelope, new Dictionary<string, object?>
        {
            ["text"] = "你好",
            ["count"] = 3,
            ["ok"] = true,
            ["nothing"] = null,
            ["nested"] = new Dictionary<string, object?> { ["a"] = "b" },
            ["list"] = new List<object?> { 1, "x" },
        });

        Assert.Equal(Envelopes.StructTypeUrl, outbound.Payload.TypeUrl);

        var inbound = Envelopes.PayloadJson(outbound)!;
        Assert.Equal("你好", inbound["text"]);
        Assert.Equal(3d, inbound["count"]);
        Assert.Equal(true, inbound["ok"]);
        Assert.Null(inbound["nothing"]);
        Assert.Equal("b", ((Dictionary<string, object?>)inbound["nested"]!)["a"]);
    }

    [Fact]
    public void 原信封不会被改动()
    {
        // 链路里可能有别的持有者，就地改会把别人的信封一起改掉
        var envelope = new Envelope { MessageId = "t-2" };
        var outbound = Envelopes.WithPayloadJson(envelope, new Dictionary<string, object?> { ["a"] = 1 });

        Assert.Null(envelope.Payload);
        Assert.NotNull(outbound.Payload);
        Assert.Equal("t-2", outbound.MessageId);
    }

    [Fact]
    public void 非Struct载荷会返回空而不是硬解()
    {
        // flow 内部传的可能是业务类型，插件那时该按自己的类型去解析
        var envelope = new Envelope
        {
            Payload = Any.Pack(new HealthRequest()),
        };

        Assert.Null(Envelopes.PayloadJson(envelope));
        Assert.Null(Envelopes.PayloadJson(new Envelope()));
    }

    [Fact]
    public void 数值一律是double()
    {
        // Struct 只有一种数值类型。大单号这类超出 2^53 的整数请用字符串承载——
        // 这条测试把这个「坑」写进代码里，而不是只写在注释里
        var envelope = Envelopes.WithPayloadJson(new Envelope(), new Dictionary<string, object?> { ["id"] = 9007199254740993L });
        var back = Envelopes.PayloadJson(envelope)!;

        Assert.IsType<double>(back["id"]);
    }

    [Fact]
    public void 预算按信封的绝对截止时间算()
    {
        var future = new Envelope { DeadlineMs = DateTimeOffset.UtcNow.AddSeconds(30).ToUnixTimeMilliseconds() };
        var budget = Envelopes.Budget(future);
        Assert.NotNull(budget);
        Assert.InRange(budget!.Value, TimeSpan.FromSeconds(25), TimeSpan.FromSeconds(31));
        Assert.False(Envelopes.Expired(future));

        var past = new Envelope { DeadlineMs = 1 };
        Assert.Equal(TimeSpan.Zero, Envelopes.Budget(past));
        Assert.True(Envelopes.Expired(past));

        // 没设 deadline 时不算过期——否则每个不带预算的调用都会被判死
        Assert.Null(Envelopes.Budget(new Envelope()));
        Assert.False(Envelopes.Expired(new Envelope()));
    }

    [Fact]
    public void 校验响应的构造()
    {
        Assert.True(Envelopes.Valid().Valid);
        Assert.Empty(Envelopes.Valid().Issues);

        var bad = Envelopes.Invalid(
            Envelopes.Issue("payload.text", "缺少必填字段 text"),
            Envelopes.Warn("payload.count", "计数偏大"));

        Assert.False(bad.Valid);
        Assert.Equal(2, bad.Issues.Count);
        Assert.Equal(Severity.Error, bad.Issues[0].Severity);
        Assert.Equal("payload.text", bad.Issues[0].Path);
        // 警告级不该让调用方以为整条被拒了
        Assert.Equal(Severity.Warning, bad.Issues[1].Severity);
    }

    [Fact]
    public void wellknown类型判定与中台一致()
    {
        Assert.True(Envelopes.IsWellKnownFqName("google.protobuf.Struct"));
        Assert.True(Envelopes.IsWellKnownFqName("google.protobuf.Any"));
        Assert.False(Envelopes.IsWellKnownFqName("wms.v1.OrderCreated"));
    }

    /// <summary>
    /// ULID 的形状：26 字符、Crockford Base32 字母表、首字符 ≤ '7'
    /// （130 位编码的高 2 位是补零，首字符只含最高 3 位）。
    ///
    /// 字母表在这里**故意再写一遍**而不是引用 <c>Envelopes</c> 的私有常量：
    /// 被测物与断言共用一份写错的常量，测了等于没测。
    /// </summary>
    [Fact]
    public void ulid的形状符合规范()
    {
        const string alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

        for (var i = 0; i < 100; i++)
        {
            var id = Envelopes.NewUlid();

            Assert.Equal(26, id.Length);
            Assert.True(id[0] <= '7', $"首字符应只含 3 位（≤7），实际 {id[0]}");
            Assert.All(id, c => Assert.Contains(c, alphabet));
        }
    }

    /// <summary>时间部分在前：同一毫秒内生成的多个 id 仍不重复（随机位扛住碰撞）。</summary>
    [Fact]
    public void ulid同一毫秒内也不重复()
    {
        var ids = new HashSet<string>(Enumerable.Range(0, 256).Select(_ => Envelopes.NewUlid()));

        Assert.Equal(256, ids.Count);
    }

    /// <summary>ULID 按时间有序：后生成的 id 不该排在早生成的前面。</summary>
    [Fact]
    public async Task ulid按时间有序()
    {
        var first = Envelopes.NewUlid();
        // 跨过毫秒界，让时间戳部分必然前进（同毫秒内只由随机位决定次序）
        await Task.Delay(5, TestContext.Current.CancellationToken);
        var second = Envelopes.NewUlid();

        Assert.True(string.CompareOrdinal(first, second) < 0, $"{first} 应排在 {second} 之前");
    }

    /// <summary>NewEnvelope 起步就带全新幂等键与 trace，type 留空不替调用方决定语义。</summary>
    [Fact]
    public void newEnvelope带全新的幂等键与trace()
    {
        var a = Envelopes.NewEnvelope();
        var b = Envelopes.NewEnvelope();

        Assert.NotEqual(a.MessageId, b.MessageId);
        Assert.NotEqual(a.TraceId, b.TraceId);
        Assert.NotEqual(a.MessageId, a.TraceId);
        Assert.Equal(PayloadType.Unspecified, a.Type);
        Assert.Equal(0, a.DeadlineMs);
    }
}
