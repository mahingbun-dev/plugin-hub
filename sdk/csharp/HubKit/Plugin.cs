using Google.Protobuf;
using Google.Protobuf.Reflection;
using Hub.V1;

namespace HubKit;

/// <summary>
/// 插件必须实现的接口。
///
/// <see cref="Manifest"/> 与 <see cref="Descriptor"/> 是「我给中台什么」，
/// <see cref="ValidateAsync"/> 与 <see cref="HandleAsync"/> 是「我干什么」——
/// 四者缺一不可：契约校验、编排连线、MCP 工具聚合都建立在它们之上。
///
/// 最小插件长这样：
/// <code>
/// public sealed class MyPlugin : IPlugin
/// {
///     public PluginManifest Manifest =&gt; new() { Name = "my-plugin", Version = "0.1.0", ... };
///     public byte[] Descriptor =&gt; HubKit.Descriptors.Of();
///     public Task&lt;ValidateResponse&gt; ValidateAsync(Envelope env, CancellationToken ct) =&gt; ...;
///     public Task&lt;Envelope&gt; HandleAsync(Envelope env, CancellationToken ct) =&gt; ...;
/// }
///
/// await PluginHost.RunAsync(new MyPlugin(), HubConfig.FromEnv());
/// </code>
///
/// 插件被强制无状态：实例内存不保证跨调用保留（见 plugin-hub 的 docs/design.md），
/// 需要跨调用保留的东西走中台的 HubState 接口——再实现 <see cref="IStateAware"/>，
/// 宿主会在注册成功后把 <see cref="StateClient"/> 注入给你（凭证只有中台知道，
/// 插件作者不该自己构造它）。
/// </summary>
public interface IPlugin
{
    /// <summary>
    /// 声明插件的身份、契约（消费/生产哪些消息类型）与暴露给 agent 的工具。
    ///
    /// **同一版本号的 manifest 不可变更**：中台会拒绝「同号不同契约」的注册，
    /// 改了东西请升版本号。
    /// </summary>
    PluginManifest Manifest { get; }

    /// <summary>
    /// 本插件的 <c>FileDescriptorSet</c> 序列化字节，中台据此建立契约基线并做字段级兼容检查。
    ///
    /// 通常不必手写——用 <see cref="Descriptors.Of"/> 把生成代码里的文件描述符打包即可。
    /// 只用 <c>google.protobuf.Struct</c> 承载 JSON 的插件没有自己的 proto，返回空数组。
    /// </summary>
    byte[] Descriptor { get; }

    /// <summary>
    /// 数据校验规则。
    ///
    /// 中台在把数据交给 <see cref="HandleAsync"/> 之前**一定**先调用它；
    /// 返回 <c>Valid=false</c> 时链路短路，Handle 不会被调用。
    /// 校验规则与插件体同版本发布，因此规则不可能与实现漂移。
    ///
    /// 抛异常表示「校验器本身坏了」，中台会回 <c>Internal</c> 并把消息带上；
    /// 业务上不通过请返回 <c>Valid=false</c> + issues，那是**正常结果**不是异常。
    /// </summary>
    Task<ValidateResponse> ValidateAsync(Envelope envelope, CancellationToken cancellationToken);

    /// <summary>
    /// 插件体：自主实现的数据输入输出。
    ///
    /// 输入的 JSON 载荷用 <see cref="Envelopes.PayloadJson"/> 取，
    /// 输出用 <see cref="Envelopes.WithPayloadJson"/> 构造。
    /// 抛异常表示插件处理失败，中台会归到「插件调用失败」，调用方据此重试。
    /// </summary>
    Task<Envelope> HandleAsync(Envelope envelope, CancellationToken cancellationToken);
}

/// <summary>
/// 把生成的 proto 文件描述符打包成 <c>FileDescriptorSet</c>。
///
/// 中台要的是**编译产物**而不是源码——它需要拿 descriptor 做字段级兼容检查，
/// 而不是去解析 .proto 文本。
/// </summary>
public static class Descriptors
{
    /// <summary>
    /// 打包成 <c>FileDescriptorSet</c>。
    ///
    /// 只打包列出的文件，**不含它们的 import**：中台侧只关心本插件自己定义的消息，
    /// 跨文件引用的类型名仍会原样出现在字段描述里，兼容性判断不受影响。
    ///
    /// 也**只需要列出本插件自己定义的消息所在的文件**：中台对 <c>google.protobuf.*</c>
    /// 这类 well-known 类型有豁免，不必把 struct.proto 也带上。
    ///
    /// <code>
    /// public byte[] Descriptor =&gt; Descriptors.Of(MyPluginReflection.Descriptor);
    /// </code>
    /// </summary>
    public static byte[] Of(params FileDescriptor[] files)
    {
        var set = new FileDescriptorSet();
        foreach (var file in files)
        {
            if (file is null)
            {
                continue;
            }

            set.File.Add(file.ToProto());
        }

        return set.ToByteArray();
    }

    /// <summary>
    /// 解析 <c>FileDescriptorSet</c>，返回其中的消息全限定名。
    ///
    /// 刻意直接遍历 descriptor 结构，而不是走 <c>FileDescriptor.BuildFromByteStrings</c>：
    /// 后者要求 descriptor 自包含（能解析出所有 import），而插件提交的 descriptor
    /// 只含自己的 proto——中台侧的 Rust 实现同样只遍历不解析引用，两边必须一致，
    /// 否则会出现「本地检查不过但中台接受」这种更糟的分歧。
    /// </summary>
    public static HashSet<string> MessageNames(byte[] raw)
    {
        var set = FileDescriptorSet.Parser.ParseFrom(raw);
        var found = new HashSet<string>(StringComparer.Ordinal);

        foreach (var file in set.File)
        {
            Collect(file.Package, file.MessageType, found);
        }

        return found;
    }

    private static void Collect(string prefix, IEnumerable<DescriptorProto> messages, HashSet<string> into)
    {
        foreach (var message in messages)
        {
            // map 字段会生成合成的 XxxEntry 消息，属实现细节，不算契约类型
            if (message.Options?.MapEntry == true)
            {
                continue;
            }

            if (message.Name.Length == 0)
            {
                continue;
            }

            var fq = prefix.Length == 0 ? message.Name : $"{prefix}.{message.Name}";
            into.Add(fq);
            Collect(fq, message.NestedType, into);
        }
    }
}

/// <summary>
/// 把拒绝码翻成人话。
///
/// **名字取自 proto 的描述符，而不是 C# 枚举的 <c>ToString()</c>**：生成的 C# 枚举
/// 值是 <c>BreakingChange</c> 这种驼峰形式，而中台、Go 侧与文档里的表格用的都是
/// proto 名（<c>REJECT_CODE_BREAKING_CHANGE</c>）。拿 C# 名去打日志，人照着文档
/// 对不上号——「日志里是 BreakingChange，文档里是 BREAKING_CHANGE」这种落差
/// 看着小，排查时正卡在这里。
/// </summary>
public static class RejectCodes
{
    private const string Prefix = "REJECT_CODE_";

    private static readonly EnumDescriptor? Descriptor =
        RegistryReflection.Descriptor.EnumTypes.FirstOrDefault(e => e.Name == nameof(RejectCode));

    public static string Name(RejectCode code)
    {
        // 认不出的编号不静默留空：日志里出现一个空字段，读的人只会以为是采集丢了
        var name = Descriptor?.FindValueByNumber((int)code)?.Name;
        if (string.IsNullOrEmpty(name))
        {
            return $"未知拒绝码({(int)code})";
        }

        return name.StartsWith(Prefix, StringComparison.Ordinal) ? name[Prefix.Length..] : name;
    }
}

/// <summary>
/// 中台拒绝了注册。
///
/// 拒绝原因是结构化的，<see cref="Exception.Message"/> 会把它们逐条排开——插件作者照着改就行，
/// 不必去翻中台的日志。
///
/// 保持多行文本是有意的：它是**错误值**的标准呈现，给日志包装、异常链与
/// <c>catch</c> 之后的自行打印用。日志是另一个呈现渠道，走 <c>Registrar</c>，
/// 把同一条信息摊成结构化的多行。
/// </summary>
public sealed class RegistrationRejectedException(IReadOnlyList<Rejection> rejections)
    : Exception(Compose(rejections))
{
    public IReadOnlyList<Rejection> Rejections { get; } = rejections;

    private static string Compose(IReadOnlyList<Rejection> rejections)
    {
        if (rejections.Count == 0)
        {
            return "中台拒绝了注册（未给出原因）";
        }

        var lines = rejections.Select(r =>
        {
            var head = $"  - {RejectCodes.Name(r.Code)}: {r.Message}";
            return r.Detail.Length > 0 ? $"{head}\n      {r.Detail}" : head;
        });

        return "中台拒绝了注册：\n" + string.Join("\n", lines);
    }
}
