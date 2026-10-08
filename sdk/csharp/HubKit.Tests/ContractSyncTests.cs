using HubKit;
using Xunit;

namespace HubKit.Tests;

/// <summary>
/// 契约素材的一致性测试：本 SDK 里那份拷贝，必须与中台仓库里的原件逐字节相同。
///
/// 拷贝是**必须的**（SDK 要随模板包下发，下载方手里没有中台仓库），
/// 而拷贝会漂移——中台改了 proto 或规则文件，没人会记得回来重跑 sync-contract.sh。
/// 这条测试就是那个"记得"。
///
/// 在**下载包**里跑时找不到中台仓库，那时跳过：原件不在手边，这里无从比对，
/// 硬判红会让「解压就能跑测试」这件事不成立。
/// </summary>
public class ContractSyncTests
{
    /// <summary>（SDK 里相对 HubKit/ 的路径，中台仓库里相对仓库根的路径）。</summary>
    private static readonly (string Ours, string Original)[] SyncedFiles =
    [
        ("protos/hub/v1/envelope.proto", "crates/hub-proto/proto/hub/v1/envelope.proto"),
        ("protos/hub/v1/plugin.proto", "crates/hub-proto/proto/hub/v1/plugin.proto"),
        ("protos/hub/v1/registry.proto", "crates/hub-proto/proto/hub/v1/registry.proto"),
        ("protos/hub/v1/state.proto", "crates/hub-proto/proto/hub/v1/state.proto"),
        ("protos/hub/v1/gateway.proto", "crates/hub-proto/proto/hub/v1/gateway.proto"),
        // 规则文件的事实来源是 **Go 侧那份**，中台的 Rust 契约测试读的也是它。
        // 取它而不是自己再写一份：两侧读同一份文件才钉得住「判定合一」
        ("testdata/hub-rules.json", "sdk/go/hubkit/testdata/hub-rules.json"),
    ];

    [Fact]
    public void SDK里的契约拷贝与中台仓库逐字节一致()
    {
        var sdkRoot = FindSdkRoot();
        var repoRoot = FindRepoRoot();

        Assert.SkipWhen(repoRoot is null,
            "找不到中台仓库（crates/hub-proto）——多半是在**下载包**里跑。原件不在手边，无从比对。");

        var drifted = new List<string>();
        foreach (var (ours, original) in SyncedFiles)
        {
            var mine = Path.Combine(sdkRoot, "HubKit", ours);
            var reference = Path.Combine(repoRoot!, original);

            if (!File.Exists(mine))
            {
                drifted.Add($"{ours}：SDK 里没有这份文件（忘了重跑 sdk/csharp/sync-contract.sh？）");
                continue;
            }

            if (!File.ReadAllBytes(reference).SequenceEqual(File.ReadAllBytes(mine)))
            {
                drifted.Add($"{ours} 与原件 {original} 不同（重跑 sdk/csharp/sync-contract.sh 同步）");
            }
        }

        Assert.True(drifted.Count == 0, "契约拷贝已漂移：\n  " + string.Join("\n  ", drifted));
    }

    /// <summary>
    /// 本地先拦一道用的两个上限，必须与**中台的实现常量**一致。
    ///
    /// 它们不在契约文件里（是 `crates/` 里的实现细节），所以拿不到那份源码就只能跳过
    /// ——但绝不能自己编一个数：放宽了会被中台拒（本地这道白拦），
    /// 收紧了会让本来合法的写入在本地就失败，而中台那边根本没错。
    ///
    /// **扫整个 `crates/` 而不是写死一个文件**：常量住在哪个文件是中台自己的事
    /// （写这段时它刚从 `hub-grpc/src/state.rs` 搬到 `hub-core/src/state.rs`）。
    /// 写死路径的话，一次纯搬迁就会把这条测试弄红，而那是个**误报**——
    /// 误报的测试很快就会被人绕着走。
    /// </summary>
    [Fact]
    public void 本地上限常量与中台实现一致()
    {
        var repoRoot = FindRepoRoot();
        Assert.SkipWhen(repoRoot is null, "找不到中台仓库——上限常量在中台的实现里，取不到就无从比对");

        var crates = Path.Combine(repoRoot!, "crates");
        Assert.Equal(StateClient.MaxValueBytes, RustConst(crates, "MAX_VALUE_BYTES"));
        Assert.Equal(StateClient.MaxScanLimit, RustConst(crates, "MAX_SCAN_LIMIT"));
    }

    /// <summary>
    /// 从 Rust 源码里取一个字面量常量。只支持现状的两种写法：十进制字面量与
    /// <c>a * b</c> 的乘积（<c>1024 * 1024</c>）——多一种写法就该在这里显式补上，
    /// 而不是让它悄悄解析成 0 之后「意外通过」。
    /// </summary>
    private static long RustConst(string cratesDir, string name)
    {
        var pattern = $@"^\s*pub const\s+{name}\s*:\s*\w+\s*=\s*([^;]+);";
        var found = new List<string>();

        foreach (var file in Directory.EnumerateFiles(cratesDir, "*.rs", SearchOption.AllDirectories))
        {
            var source = File.ReadAllText(file);
            foreach (System.Text.RegularExpressions.Match match in
                System.Text.RegularExpressions.Regex.Matches(source, pattern, System.Text.RegularExpressions.RegexOptions.Multiline))
            {
                found.Add(match.Groups[1].Value);
            }
        }

        Assert.True(
            found.Count == 1,
            $"中台实现里应当**恰好**有一处定义 {name}，实际 {found.Count} 处。"
            + "0 处说明它被改名或挪出了 crates/；多于 1 处说明出现了第二个事实来源，"
            + "这里必须先想清楚以哪个为准");

        var value = 1L;
        foreach (var part in found[0].Split('*'))
        {
            var text = part.Trim().Replace("_", string.Empty);
            Assert.True(long.TryParse(text, out var factor), $"{name} 的写法认不出来：{found[0]}");
            value *= factor;
        }

        return value;
    }

    /// <summary>从测试产物目录往上找 SDK 根（含 <c>HubKit/HubKit.csproj</c> 的那一层）。</summary>
    private static string FindSdkRoot()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null)
        {
            if (File.Exists(Path.Combine(dir.FullName, "HubKit", "HubKit.csproj")))
            {
                return dir.FullName;
            }

            dir = dir.Parent;
        }

        throw new InvalidOperationException($"从 {AppContext.BaseDirectory} 往上找不到 SDK 根（HubKit/HubKit.csproj）");
    }

    /// <summary>从测试产物目录往上找中台仓库根（含 <c>crates/hub-proto/proto</c> 的那一层）。</summary>
    private static string? FindRepoRoot()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null)
        {
            if (Directory.Exists(Path.Combine(dir.FullName, "crates", "hub-proto", "proto", "hub", "v1")))
            {
                return dir.FullName;
            }

            dir = dir.Parent;
        }

        return null;
    }
}
