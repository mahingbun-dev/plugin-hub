using Hub.V1;

namespace HubKit;

/// <summary>一项检查的结果。</summary>
public sealed record CheckResult(string Name, bool Passed, string Detail)
{
    public override string ToString()
    {
        var mark = Passed ? "✓" : "✗";
        return Detail.Length == 0 ? $"  {mark} {Name}" : $"  {mark} {Name} —— {Detail}";
    }
}

/// <summary>
/// 一套检查的结果。
///
/// 行名与行序**刻意与 Go 侧 conformance.Report 一致**：README 里贴的期望输出
/// 六门语言是同一份，顺序一变，文档就得按语言分支。
/// </summary>
public sealed class Report
{
    private readonly List<CheckResult> _checks = [];

    public Report(string subject) => Subject = subject;

    /// <summary>被检查的对象：本地检查是「插件名@版本」。</summary>
    public string Subject { get; private set; }

    public IReadOnlyList<CheckResult> Checks => _checks;

    public bool Passed => _checks.All(c => c.Passed);

    public IEnumerable<CheckResult> Failures => _checks.Where(c => !c.Passed);

    internal void Add(string name, bool passed, string detail = "") => _checks.Add(new CheckResult(name, passed, detail));

    public override string ToString()
    {
        var lines = _checks.Select(c => c.ToString());
        return $"契约一致性检查 {Subject}\n" + string.Join("\n", lines) + "\n";
    }
}

/// <summary>
/// 插件的契约一致性自测套件。
///
/// 插件接入中台之前必须跑通它。检查的都是「中台在真实调用时依赖、但等生产才发现
/// 代价太大」的约定：契约自洽、校验器不崩、插件体真的返回信封。
///
/// <see cref="Local"/> 只看插件对象，不需要把它跑起来——它复现的正是中台注册期
/// 的校验，本地先跑能省一轮「推上去才发现被拒」。
///
/// 典型用法（插件工程里放一个测试）：
/// <code>
/// [Fact]
/// public void 契约一致性()
/// {
///     var report = Conformance.Local(new MyPlugin());
///     Assert.True(report.Passed, report.ToString());
/// }
/// </code>
/// </summary>
public static class Conformance
{
    /// <summary>工具名要拼进 MCP 的工具标识，字符集更窄。</summary>
    private static bool IsValidToolName(string name) =>
        name.Length > 0 && name.All(c => c is (>= 'a' and <= 'z') or (>= 'A' and <= 'Z') or (>= '0' and <= '9') or '_' or '-');

    /// <summary>
    /// 检查插件对象自身的自洽性，不需要把它跑起来。
    ///
    /// 复现的是中台注册期的那几条校验，因此它能挡住绝大多数「推上去才发现被拒」的问题。
    /// </summary>
    public static Report Local(IPlugin plugin)
    {
        var manifest = plugin.Manifest;
        if (manifest is null)
        {
            var broken = new Report("（未命名插件）");
            broken.Add("manifest 存在", false, "Manifest 返回了 null");
            return broken;
        }

        var report = new Report($"{manifest.Name}@{manifest.Version}");
        report.Add("manifest 存在", true);

        var name = manifest.Name;
        if (string.IsNullOrWhiteSpace(name))
        {
            report.Add("插件名合法", false, "缺少插件名 name");
        }
        else if (!Rules.ValidPluginName(name))
        {
            report.Add("插件名合法", false, "只允许字母数字与 -_，最长 64 字符，且以字母数字开头");
        }
        else
        {
            report.Add("插件名合法", true, name);
        }

        var version = manifest.Version;
        if (string.IsNullOrWhiteSpace(version))
        {
            report.Add("版本号存在", false, "缺少版本号 —— flow 靠它锁定实例");
        }
        else
        {
            report.Add("版本号存在", true, version);
        }

        // descriptor 是中台做字段级兼容检查的依据。
        // 空的是合法的：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 proto。
        var raw = plugin.Descriptor;
        var messages = new HashSet<string>(StringComparer.Ordinal);
        if (raw is null || raw.Length == 0)
        {
            report.Add("descriptor 可用", true, "无自有 proto（只用 well-known 载荷）");
        }
        else
        {
            try
            {
                messages = Descriptors.MessageNames(raw);
            }
            catch (Exception ex)
            {
                report.Add("descriptor 可用", false, ex.Message);
                return report;
            }

            report.Add("descriptor 可用", true, $"{raw.Length} 字节、{messages.Count} 个消息类型");
        }

        CheckDeclaredMessages(report, manifest, messages);
        CheckTools(report, manifest);
        return report;
    }

    /// <summary>
    /// 验证「自述与编译产物一致」。
    ///
    /// 这是最有价值的一条：改了 proto 忘了重新生成、或者消息改名后忘了同步 manifest，
    /// 都在这里被抓住，而不是等注册时被中台拒。
    /// </summary>
    private static void CheckDeclaredMessages(Report report, PluginManifest manifest, HashSet<string> messages)
    {
        var missing = new List<string>();

        void Check(string direction, IEnumerable<MessageContract> contracts)
        {
            foreach (var contract in contracts)
            {
                var fq = contract.FqName;
                if (Envelopes.IsWellKnownFqName(fq))
                {
                    continue;
                }

                if (!messages.Contains(fq))
                {
                    missing.Add($"{direction} 里的 {fq}");
                }
            }
        }

        Check("produces", manifest.Produces);
        Check("consumes", manifest.Consumes);

        if (missing.Count > 0)
        {
            report.Add("声明的类型都在 descriptor 中", false,
                string.Join("；", missing) + " —— manifest 的自述必须与提交的 proto 一致");
            return;
        }

        if (manifest.Produces.Count == 0 && manifest.Consumes.Count == 0)
        {
            report.Add("声明了契约", false,
                $"既没有 produces 也没有 consumes —— 至少声明一个，接受直接调用的插件请声明 {Envelopes.StructFqName}");
            return;
        }

        report.Add("声明的类型都在 descriptor 中", true,
            $"produces {manifest.Produces.Count} 个、consumes {manifest.Consumes.Count} 个");
    }

    private static void CheckTools(Report report, PluginManifest manifest)
    {
        if (manifest.Tools.Count == 0)
        {
            // 不暴露工具是合法的：插件可能只参与 flow
            report.Add("工具声明合法", true, "未声明工具（仅参与 flow）");
            return;
        }

        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var tool in manifest.Tools)
        {
            if (!IsValidToolName(tool.Name))
            {
                report.Add("工具声明合法", false,
                    $"工具名 \"{tool.Name}\" 含非法字符 —— 它要拼进 MCP 的工具标识");
                return;
            }

            if (!seen.Add(tool.Name))
            {
                report.Add("工具声明合法", false,
                    $"工具 {tool.Name} 重复声明 —— 同一插件内工具名必须唯一");
                return;
            }
        }

        report.Add("工具声明合法", true, $"{seen.Count} 个工具");
    }

    /// <summary>
    /// 对**运行中的插件**检查运行时行为。
    ///
    /// <paramref name="pluginAddr"/> 是插件的 gRPC 地址（如 <c>http://127.0.0.1:9000</c>）。
    ///
    /// 这一关不需要中台在线（直连插件），所以它证明不了网络可达性——
    /// 也就是证明不了中台拨不拨得到你。别拿它当交付判据。
    /// </summary>
    public static async Task<Report> RuntimeAsync(string pluginAddr, CancellationToken cancellationToken = default)
    {
        var report = new Report(pluginAddr);

        using var channel = HubChannel.Create(pluginAddr);
        var client = new PluginRuntime.PluginRuntimeClient(channel);

        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(CheckTimeout);

        HealthResponse health;
        try
        {
            health = await client.HealthAsync(new HealthRequest(), cancellationToken: timeout.Token);
        }
        catch (Exception ex)
        {
            // 「Health 可应答」✗ 是这一关最常见的失败，而且多半不是健康检查的问题，
            // 是**地址/端口**：地址写错、端口不对（缺省监听 :9000）、插件没起来
            report.Add("Health 可应答", false, ex.Message);
            return report;
        }

        if (!health.Healthy)
        {
            report.Add("Health 可应答", false, "插件自报不健康: " + health.Message);
            return report;
        }

        report.Add("Health 可应答", true);

        try
        {
            var manifest = await client.DescribeAsync(new DescribeRequest(), cancellationToken: timeout.Token);
            report.Add("Describe 可应答", true, $"{manifest.Name}@{manifest.Version}");
        }
        catch (Exception ex)
        {
            report.Add("Describe 可应答", false, ex.Message);
        }

        // 空信封：插件必须能处理，而不是 panic 或挂住
        try
        {
            await client.ValidateAsync(new ValidateRequest { Envelope = new Envelope() }, cancellationToken: timeout.Token);
            report.Add("校验器对空信封不崩", true);
        }
        catch (Exception ex)
        {
            report.Add("校验器对空信封不崩", false, ex.Message);
        }

        var probe = new ValidateRequest
        {
            Envelope = Envelopes.WithPayloadJson(
                new Envelope { MessageId = "conformance-1" },
                new Dictionary<string, object?> { ["conformance"] = true }),
        };

        try
        {
            var response = await client.ValidateAsync(probe, cancellationToken: timeout.Token);
            report.Add("校验器可处理 JSON 载荷", true, response.Valid
                ? "通过"
                // 探针载荷本来就可能不满足业务规则，**拒绝是合法结果**；
                // 只有超时或抛异常才算 ✗
                : $"拒绝（{response.Issues.Count} 条问题）—— 探针载荷不满足业务规则属正常");
        }
        catch (Exception ex)
        {
            report.Add("校验器可处理 JSON 载荷", false, ex.Message);
        }

        var handle = new HandleRequest { Envelope = probe.Envelope };
        try
        {
            var output = await client.HandleAsync(handle, cancellationToken: timeout.Token);
            if (output.Envelope is null)
            {
                // 空信封中台会当成插件异常
                report.Add("插件体返回信封", false, "返回了空信封 —— 中台会把它当成插件异常");
            }
            else
            {
                report.Add("插件体返回信封", true,
                    Envelopes.PayloadJson(output.Envelope) is not null ? "已返回 JSON 载荷" : "已返回信封");
            }
        }
        catch (Exception ex)
        {
            // 被业务逻辑拒掉是正常的，但必须是「明确报错」而不是超时或 panic
            report.Add("插件体返回信封", true, $"拒绝处理（{ex.Message}）—— 探针载荷不满足业务规则属正常");
        }

        return report;
    }

    /// <summary>各项运行时检查的时间上限：插件是外部进程，卡住的检查要尽快暴露。</summary>
    private static readonly TimeSpan CheckTimeout = TimeSpan.FromSeconds(5);
}
