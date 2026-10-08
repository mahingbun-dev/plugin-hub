using Hub.V1;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// L1 契约自洽的检查本身。
///
/// 这些检查是「中台注册期校验」的本地复现，它一旦漏判，插件会带着问题推上去、
/// 到中台才被拒——所以检查器的**每一条分支**都得有测试钉着，
/// 包括「该红的时候必须红」。
/// </summary>
public class ConformanceTests
{
    [Fact]
    public void 合法插件六项全过()
    {
        var report = Conformance.Local(new FakePlugin());

        Assert.True(report.Passed, report.ToString());
        Assert.Equal("test-plugin@0.1.0", report.Subject);

        // 行名与行序是与 Go 侧对齐的约定：README 里贴的期望输出六门语言同一份
        Assert.Equal(
            [
                "manifest 存在",
                "插件名合法",
                "版本号存在",
                "descriptor 可用",
                "声明的类型都在 descriptor 中",
                "工具声明合法",
            ],
            report.Checks.Select(c => c.Name));
    }

    [Fact]
    public void 插件名非法会红()
    {
        var report = Conformance.Local(new FakePlugin { Name = "_lead" });

        Assert.False(report.Passed);
        Assert.Contains(report.Failures, c => c.Name == "插件名合法" && c.Detail.Contains("字母数字"));
    }

    [Fact]
    public void 缺版本号会红()
    {
        var report = Conformance.Local(new FakePlugin { Version = "" });

        Assert.False(report.Passed);
        Assert.Contains(report.Failures, c => c.Name == "版本号存在");
    }

    [Fact]
    public void 一条契约都没声明会红()
    {
        // 中台**会接受**一份 produces / consumes 都为空的 manifest，
        // 所以这条是本地比中台更严的唯一一条——正因如此才要单独钉住，
        // 免得有人拿「中台能收」当借口把它删掉
        var report = Conformance.Local(new FakePlugin { Consumes = [], Produces = [] });

        Assert.False(report.Passed);
        Assert.Contains(report.Failures, c => c.Name == "声明了契约" && c.Detail.Contains("google.protobuf.Struct"));
    }

    [Fact]
    public void 声明了自有类型却交不出descriptor会红()
    {
        var report = Conformance.Local(new FakePlugin { Consumes = ["wms.v1.OrderCreated"] });

        Assert.False(report.Passed);
        var failure = Assert.Single(report.Failures, c => c.Name == "声明的类型都在 descriptor 中");
        Assert.Contains("consumes 里的 wms.v1.OrderCreated", failure.Detail);
    }

    [Fact]
    public void 工具名含非法字符或重名都会红()
    {
        var badChar = Conformance.Local(new FakePlugin
        {
            Tools = [new ToolDecl { Name = "a.b" }],
        });
        Assert.False(badChar.Passed);
        Assert.Contains(badChar.Failures, c => c.Name == "工具声明合法");

        var duplicated = Conformance.Local(new FakePlugin
        {
            Tools = [new ToolDecl { Name = "echo" }, new ToolDecl { Name = "echo" }],
        });
        Assert.False(duplicated.Passed);
        Assert.Contains(duplicated.Failures, c => c.Detail.Contains("重复声明"));
    }

    [Fact]
    public void 空descriptor是合法的而不是错误()
    {
        // 只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 proto，
        // 把它判红会让「最小插件」根本没法通过 L1
        var report = Conformance.Local(new FakePlugin { Descriptor = [] });

        Assert.True(report.Passed, report.ToString());
        Assert.Contains(report.Checks, c => c.Name == "descriptor 可用" && c.Detail.Contains("well-known"));
    }

    [Fact]
    public void descriptor能解析出自有类型()
    {
        // 拿 SDK 自己的 proto 当素材：它就在包里，不必另造一个 .proto 再生成一遍
        var raw = Descriptors.Of(Hub.V1.EnvelopeReflection.Descriptor);
        var names = Descriptors.MessageNames(raw);

        Assert.Contains("hub.v1.Envelope", names);
        Assert.Contains("hub.v1.Subject", names);
        // map 字段的合成 Entry 消息是实现细节，不该被算成契约类型
        Assert.DoesNotContain("hub.v1.Envelope.MetaEntry", names);
    }

    [Fact]
    public void descriptor不是合法protobuf时报错而不是崩()
    {
        var report = Conformance.Local(new FakePlugin { Descriptor = [0xff, 0xff, 0xff] });

        Assert.False(report.Passed);
        Assert.Contains(report.Failures, c => c.Name == "descriptor 可用");
    }

    [Theory]
    [InlineData(RejectCode.Unreachable, "UNREACHABLE")]
    [InlineData(RejectCode.DescriptorInvalid, "DESCRIPTOR_INVALID")]
    [InlineData(RejectCode.BreakingChange, "BREAKING_CHANGE")]
    [InlineData(RejectCode.ToolConflict, "TOOL_CONFLICT")]
    [InlineData(RejectCode.VersionConflict, "VERSION_CONFLICT")]
    [InlineData(RejectCode.ManifestInvalid, "MANIFEST_INVALID")]
    [InlineData(RejectCode.Internal, "INTERNAL")]
    [InlineData(RejectCode.InstanceConflict, "INSTANCE_CONFLICT")]
    public void 拒绝码用的是proto名而不是C的枚举名(RejectCode code, string want)
    {
        // 中台、Go 侧与文档里的表格用的都是 proto 名。打 C# 的驼峰名（BreakingChange）
        // 会让「照着文档的码表对号入座」这件事做不成。
        Assert.Equal(want, RejectCodes.Name(code));
    }

    [Fact]
    public void 认不出的拒绝码不静默留空()
    {
        Assert.Equal("未知拒绝码(999)", RejectCodes.Name((RejectCode)999));
    }
}
