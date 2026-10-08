using System.Text.Json;
using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 跨语言规则的一致性测试。
///
/// 输入是 <c>testdata/hub-rules.json</c>——Rust、Go、C# 三侧读的是**同一份**文件。
/// 它不是从任何一侧的实现生成的，是手写的规格：由实现生成等于自证，
/// 生成时用了错的实现，固件也是错的。
///
/// 本文件的价值在于「改了规则文件，C# 侧没跟上就会红」——而不是「我把实现抄了一遍」。
/// </summary>
public class RulesTests
{
    private static string RulesPath =>
        Path.Combine(AppContext.BaseDirectory, "testdata", "hub-rules.json");

    [Fact]
    public void 规则文件的每一条用例都通过()
    {
        using var document = JsonDocument.Parse(File.ReadAllText(RulesPath));
        var cases = document.RootElement.GetProperty("cases");

        var failures = new List<string>();
        var total = 0;

        foreach (var item in cases.EnumerateArray())
        {
            total++;
            var rule = item.GetProperty("rule").GetString()!;
            // JsonDocument 会把 \u0000 转义还原成真正的 NUL 字符——规则文件里那条
            // 用例就是靠它表达「控制字符必须被拒」，而不是一个看起来像空格的替身
            var input = item.GetProperty("input").GetString()!;
            var want = item.GetProperty("valid").GetBoolean();

            var got = rule switch
            {
                "stateSegment" => Rules.ValidStateSegment(input),
                "pluginName" => Rules.ValidPluginName(input),
                _ => throw new InvalidOperationException($"规则文件里有 C# 侧不认识的规则：{rule}"),
            };

            if (got != want)
            {
                failures.Add($"{rule}({Describe(input)}) 期望 {want}，实际 {got}");
            }
        }

        Assert.True(failures.Count == 0,
            $"{failures.Count}/{total} 条用例不通过：\n  " + string.Join("\n  ", failures));
    }

    [Fact]
    public void 插件名首字符不允许下划线与连字符()
    {
        // 这两条在规则文件里各占一行，单独再钉一次是因为「首字符」这条最容易写漏
        Assert.False(Rules.ValidPluginName("_lead"));
        Assert.False(Rules.ValidPluginName("-lead"));
        Assert.True(Rules.ValidPluginName("a-"));
        Assert.True(Rules.ValidPluginName("a_b-1"));
    }

    private static string Describe(string s)
    {
        var escaped = s.Replace("\0", "\\0");
        if (escaped.Length <= 24)
        {
            return $"\"{escaped}\"";
        }

        return $"\"{escaped[..24]}…\"（{escaped.Length} 字符）";
    }
}
