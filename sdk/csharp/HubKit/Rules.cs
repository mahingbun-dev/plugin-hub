namespace HubKit;

/// <summary>
/// 本文件放**跨语言的**规则判定。
///
/// 事实来源是 <c>testdata/hub-rules.json</c>——Rust 侧（crates/hub-grpc/tests/state_rules.rs）、
/// Go 侧（sdk/go/hubkit/rules_test.go）与 C# 侧（HubKit.Tests/RulesTests.cs）读的是
/// 同一份文件并断言各自那边的实现。任一侧改了实现而没同步契约文件，**它自己那侧**的
/// 契约测试就会变红；改了契约文件而某一侧没跟上，则是那一侧变红。如此挡住
/// 「插件以为写进去了、中台拒绝」这类静默漂移。
///
/// 判定合一，**动作不合并**：中台与 mockhub 拿它去拒绝请求，插件拿它去跳过缓存
/// （fail-open，不拒账号）。同一个函数在两侧的正确动作不同，不要试图统一。
/// </summary>
public static class Rules
{
    /// <summary>
    /// 判断字符串能否作为 HubState 的 namespace / key / scan prefix。
    ///
    /// 规则：非空、最长 200 字节、只允许 <c>[A-Za-z0-9_.-]</c>。
    ///
    /// 放行 `*` 是漏洞不是功能：前缀靠字符串拼接，通配符会让 KvScan 变成跨命名空间的
    /// 模式匹配；冒号同理——它破坏前缀的结构。
    /// </summary>
    public static bool ValidStateSegment(string? s)
    {
        if (string.IsNullOrEmpty(s))
        {
            return false;
        }

        // 按**字节**而不是按字符判长度：上限是字节数，而白名单全是 ASCII，
        // 所以合法输入的长度必然等于字符数——非法输入无论先撞长度还是先撞字符集，
        // 结果都是 false，两种实现风格的差别不可观测（见 hub-rules.json 的 _byte_vs_char）
        if (System.Text.Encoding.UTF8.GetByteCount(s) > 200)
        {
            return false;
        }

        foreach (var c in s)
        {
            var ok = c is (>= 'a' and <= 'z') or (>= 'A' and <= 'Z') or (>= '0' and <= '9') or '_' or '.' or '-';
            if (!ok)
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>
    /// 判断 manifest 里的插件名是否合法。
    ///
    /// 规则：非空、最长 64 字节、首字符是字母或数字、其余位置允许 <c>[A-Za-z0-9_-]</c>。
    /// 与中台的 crates/hub-registry/src/validate.rs 的 <c>is_valid_plugin_name</c> 等价。
    /// </summary>
    public static bool ValidPluginName(string? name)
    {
        if (string.IsNullOrEmpty(name))
        {
            return false;
        }

        if (System.Text.Encoding.UTF8.GetByteCount(name) > 64)
        {
            return false;
        }

        var first = true;
        foreach (var c in name)
        {
            var alnum = c is (>= 'a' and <= 'z') or (>= 'A' and <= 'Z') or (>= '0' and <= '9');
            if (alnum)
            {
                first = false;
                continue;
            }

            // 首字符不允许 - 与 _；其余位置允许
            if (!first && c is '-' or '_')
            {
                first = false;
                continue;
            }

            return false;
        }

        return true;
    }
}
