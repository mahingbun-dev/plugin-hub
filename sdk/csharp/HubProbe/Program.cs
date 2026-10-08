using HubKit;

// hubprobe —— SDK 里的插件调试工具。与 Go 侧同名同用途。
//
//   hubprobe conform <插件地址>   跑一遍运行时契约自检（不需要中台在线）
//   hubprobe health  <插件地址>   只探活
//
// 它验的是「插件作为 gRPC 进程是否健康」，**证明不了**中台能不能拨通你上报的地址
// ——那是网络视角的问题，只有中台自己拨一次才知道（L3）。

if (args.Length == 0 || args[0] is "-h" or "--help" or "help")
{
    Usage();
    return 2;
}

var command = args[0];
var target = args.Length > 1 ? args[1] : null;

if (string.IsNullOrEmpty(target))
{
    Console.Error.WriteLine($"{command} 需要一个插件地址，例如 http://127.0.0.1:9000");
    return 2;
}

switch (command)
{
    case "conform":
    {
        var report = await Conformance.RuntimeAsync(target);
        // 报告走 stdout：它是要被贴进 issue、被管道读的产物。
        // 人话（比如失败时的下一步）走 stderr，管道里拿到的仍是干净的一份。
        Console.WriteLine(report.ToString().TrimEnd());
        if (!report.Passed)
        {
            Console.Error.WriteLine();
            Console.Error.WriteLine("没过的项该怎么查：");
            foreach (var failure in report.Failures)
            {
                Console.Error.WriteLine($"  - {failure.Name}：{Hints.For(failure.Name)}");
            }
        }

        return report.Passed ? 0 : 1;
    }

    case "health":
    {
        // 只探活：拿它区分「中台没起来 / 地址写错 / 前缀不对」三种情况时最省事
        var report = await Conformance.RuntimeAsync(target);
        var health = report.Checks.FirstOrDefault(c => c.Name == "Health 可应答");
        if (health is null)
        {
            Console.Error.WriteLine($"连不上 {target}");
            return 1;
        }

        Console.WriteLine($"{target} 可应答");
        return 0;
    }

    default:
        Console.Error.WriteLine($"不认识的命令：{command}");
        Usage();
        return 2;
}

static void Usage()
{
    Console.Error.WriteLine("""
        用法：
          hubprobe conform <插件地址>   运行时契约自检（Health / Describe / 校验器 / 插件体）
          hubprobe health  <插件地址>   只探活

        地址形如 http://127.0.0.1:9000（插件自己的监听端口，不是中台的）。
        """);
}

/// <summary>
/// 「没过怎么办」的提示。
///
/// 放在这里而不是报告里：报告是要被脚本解析的，塞进排查建议会让它变成一坨散文；
/// 而排查建议又确实有用，不能只有一句 ✗。
/// </summary>
internal static class Hints
{
    public static string For(string check) => check switch
    {
        "Health 可应答" =>
            "这一档最常见的成因**不是**健康检查，是地址/端口：地址写错、端口不对" +
            "（缺省监听 :9000）、插件没起来——三种都会落在这一行。",
        "Describe 可应答" => "manifest 有问题，先跑本工程的 L1（契约一致性测试）。",
        "校验器对空信封不崩" => "Validate 在空信封上抛异常了；中台探测时就会送它进来，必须能处理。",
        "校验器可处理 JSON 载荷" =>
            "Validate 在这个载荷上**超时或抛异常**了（被业务规则拒绝**不算**失败）。",
        "插件体返回信封" => "Handle 返回了空信封，中台会把它当成插件异常。",
        _ => "见 README 的「接入四道关」。",
    };
}
