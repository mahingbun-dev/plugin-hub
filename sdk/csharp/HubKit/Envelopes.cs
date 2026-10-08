using System.Security.Cryptography;
using System.Buffers.Binary;
using Google.Protobuf;
using Google.Protobuf.WellKnownTypes;
using Hub.V1;

namespace HubKit;

/// <summary>
/// 信封与载荷的读写助手。
///
/// 载荷走 <c>google.protobuf.Any</c>，契约标识是 type_url 的最后一段
/// （全限定消息名，如 <c>google.protobuf.Struct</c>）。本文件提供「直接调用」
/// 场景下的 JSON 载荷读写：agent 经 MCP、外部系统经 HTTP 调用插件时，
/// 中台把 JSON 对象包成 <c>Struct</c> 送进来。
/// </summary>
public static class Envelopes
{
    /// <summary>
    /// 「直接调用」载荷的类型标识。
    ///
    /// flow 内部传递的是业务类型，插件两种都可能收到，用 <see cref="PayloadJson"/>
    /// 区分：它只认这一种，别的返回 null。
    /// </summary>
    public const string StructTypeUrl = "type.googleapis.com/google.protobuf.Struct";

    /// <summary>
    /// 上面那个载荷的全限定消息名。
    ///
    /// 在 manifest 里声明 <c>Consumes = { new MessageContract { FqName = Envelopes.StructFqName } }</c>
    /// 表示「本插件接受直接调用的 JSON 载荷」。它是 well-known 类型，中台不要求
    /// 它出现在插件自己的 descriptor 里。
    /// </summary>
    public const string StructFqName = "google.protobuf.Struct";

    /// <summary>
    /// 判断是否属于 protobuf 平台提供的 well-known 类型。
    ///
    /// 中台对 <c>google.protobuf.*</c> 豁免「声明必须出现在自己的 descriptor 里」
    /// 这条检查；插件侧的 L1 自检用同一个判断，避免两边规则漂移。
    /// </summary>
    public static bool IsWellKnownFqName(string fqName) => fqName.StartsWith("google.protobuf.", StringComparison.Ordinal);

    /// <summary>
    /// ULID 的字符表（Crockford Base32：剔除 I/L/O/U，避免手抄时与 1/0 混淆）。
    /// 中台（Rust 的 ulid crate）生成的 id 也是这个形状。
    /// </summary>
    private const string UlidEncoding = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    /// <summary>
    /// 生成一个 ULID：48 位毫秒时间戳 + 80 位随机数，Crockford Base32 编码的 26 字符。
    ///
    /// <see cref="NewEnvelope"/> 用它填 message_id 与 trace_id。选 ULID 而不是 UUID 的
    /// 理由与中台一致：时间部分在前，同一批消息按 id 排序就是按时间排序；时间戳之外
    /// 还掺了加密级随机位，「同一毫秒内也不重复」才有保障——message_id 是总线的幂等键
    /// （at-least-once 下去重靠它），撞了就是误伤业务数据。
    ///
    /// 用 BCL 手写而不是引第三方 ULID 包：SDK 的依赖面（Grpc / Protobuf）之外每多一个包，
    /// 插件团队的依赖树就多一分冲突可能，而这几十行编码逻辑不构成引入它的理由。
    /// </summary>
    public static string NewUlid()
    {
        var ms = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        Span<byte> random = stackalloc byte[10];
        RandomNumberGenerator.Fill(random);

        // 128 位拆成高低两个 64 位段（时间戳占高 48 位，其余是随机数），免得引 BigInteger：
        // 它按补码解释最高位，带符号的右移会把「补零」变成「补 1」
        var hi = ((ulong)(ms & 0xFFFF_FFFF_FFFF) << 16) | ((ulong)random[0] << 8) | random[1];
        var lo = BinaryPrimitives.ReadUInt64BigEndian(random[2..]);

        Span<char> chars = stackalloc char[26];
        // 26 字符 × 5 位 = 130 位，比 128 位多出的 2 个零位落在首字符上
        // （ULID 规范：首字符只含最高 3 位，取值 0..7）
        chars[0] = UlidEncoding[(int)(hi >> 61)];
        hi = (hi << 3) | (lo >> 61);
        lo <<= 3;
        for (var i = 1; i < 26; i++)
        {
            chars[i] = UlidEncoding[(int)(hi >> 59)];
            hi = (hi << 5) | (lo >> 59);
            lo <<= 5;
        }

        return new string(chars);
    }

    /// <summary>
    /// 构造一个带全新 message_id / trace_id 的空信封。
    ///
    /// 自己发起一条数据流（Publish、或 <see cref="GatewayClient.InvokePluginAsync"/>
    /// 之外手动装配 Invoke 的信封）时用它起步，载荷再用 <see cref="WithPayloadJson"/> /
    /// <see cref="WithPayload"/> 装。type 留空不替调用方决定语义：触发路径各自知道
    /// 这次携带的是请求还是事件。
    /// </summary>
    public static Envelope NewEnvelope() => new() { MessageId = NewUlid(), TraceId = NewUlid() };

    /// <summary>
    /// 取出信封里的 JSON 载荷。载荷不是 <c>Struct</c>（例如 flow 内部传的业务类型）
    /// 或解不开时返回 null，此时插件应改为按自己的业务类型去解析。
    ///
    /// ⚠️ 数值一律是 <c>double</c>——<c>google.protobuf.Struct</c> 只有一种数值类型。
    /// 大单号这类超出 2^53 的整数请用字符串承载，别指望 JSON 数字。
    /// </summary>
    public static Dictionary<string, object?>? PayloadJson(Envelope? env)
    {
        var payload = env?.Payload;
        if (payload is null || payload.TypeUrl != StructTypeUrl)
        {
            return null;
        }

        // Any 里装的到底是不是 Struct 只能靠解一次确认：type_url 是插件自己写的字符串，
        // 不是类型系统保证的东西
        if (!payload.Is(Struct.Descriptor))
        {
            return null;
        }

        return StructToMap(payload.Unpack<Struct>());
    }

    /// <summary>
    /// 把 JSON 对象装进信封的载荷。
    ///
    /// 返回**新**信封，原信封不被修改——链路里可能有别的持有者。
    /// </summary>
    public static Envelope WithPayloadJson(Envelope? env, IDictionary<string, object?> payload)
    {
        var packed = Any.Pack(MapToStruct(payload));
        var clone = Clone(env);
        clone.Payload = packed;
        return clone;
    }

    /// <summary>把业务类型装进信封的载荷（flow 内部传递用）。</summary>
    public static Envelope WithPayload(Envelope? env, IMessage message)
    {
        var clone = Clone(env);
        clone.Payload = Any.Pack(message);
        return clone;
    }

    /// <summary>信封的绝对截止时间。未设置时返回 null。</summary>
    public static DateTimeOffset? Deadline(Envelope? env)
    {
        var ms = env?.DeadlineMs ?? 0;
        return ms <= 0 ? null : DateTimeOffset.FromUnixTimeMilliseconds(ms);
    }

    /// <summary>
    /// 距离截止时间还剩多久。未设置时返回 null；已过期时返回 <see cref="TimeSpan.Zero"/>。
    ///
    /// deadline 逐跳递减：插件应据此提前放弃，而不是把时间耗光后让上层的超时兜底。
    /// </summary>
    public static TimeSpan? Budget(Envelope? env)
    {
        var deadline = Deadline(env);
        if (deadline is null)
        {
            return null;
        }

        var left = deadline.Value - DateTimeOffset.UtcNow;
        return left < TimeSpan.Zero ? TimeSpan.Zero : left;
    }

    /// <summary>判断信封是否已过截止时间。未设置 deadline 时不算过期。</summary>
    public static bool Expired(Envelope? env) => Budget(env) == TimeSpan.Zero;

    /// <summary>构造「校验通过」的响应。</summary>
    public static ValidateResponse Valid() => new() { Valid = true };

    /// <summary>
    /// 构造「校验不通过」的响应。
    ///
    /// 每个 issue 的 path 要能定位到具体字段（例如 <c>payload.items[2].sku</c>），
    /// 中台会原样把它回给调用方，agent 靠它改数据重试。
    /// </summary>
    public static ValidateResponse Invalid(params ValidationIssue[] issues)
    {
        var response = new ValidateResponse { Valid = false };
        response.Issues.AddRange(issues);
        return response;
    }

    /// <summary>构造一条错误级校验问题。</summary>
    public static ValidationIssue Issue(string path, string message) => new()
    {
        Path = path,
        Message = message,
        Severity = Severity.Error,
    };

    /// <summary>
    /// 构造一条警告级校验问题。
    ///
    /// 警告不会让校验失败——用它标记「能放行但值得记一笔」的情况。
    /// </summary>
    public static ValidationIssue Warn(string path, string message) => new()
    {
        Path = path,
        Message = message,
        Severity = Severity.Warning,
    };

    /// <summary>深拷贝信封。null 视作空信封。</summary>
    public static Envelope Clone(Envelope? env) => env?.Clone() ?? new Envelope();

    /// <summary><c>Struct</c> → 普通字典。数值统一是 <c>double</c>。</summary>
    public static Dictionary<string, object?> StructToMap(Struct s)
    {
        var map = new Dictionary<string, object?>(s.Fields.Count, StringComparer.Ordinal);
        foreach (var (key, value) in s.Fields)
        {
            map[key] = ValueToObject(value);
        }

        return map;
    }

    /// <summary>普通字典 → <c>Struct</c>。认 string / bool / 各种数值 / 枚举 / 嵌套字典 / 列表 / null。</summary>
    public static Struct MapToStruct(IDictionary<string, object?> map)
    {
        var s = new Struct();
        foreach (var (key, value) in map)
        {
            s.Fields[key] = ObjectToValue(value);
        }

        return s;
    }

    private static object? ValueToObject(Value v) => v.KindCase switch
    {
        Value.KindOneofCase.NullValue => null,
        Value.KindOneofCase.NumberValue => v.NumberValue,
        Value.KindOneofCase.StringValue => v.StringValue,
        Value.KindOneofCase.BoolValue => v.BoolValue,
        Value.KindOneofCase.StructValue => StructToMap(v.StructValue),
        Value.KindOneofCase.ListValue => v.ListValue.Values.Select(ValueToObject).ToList(),
        _ => null,
    };

    private static Value ObjectToValue(object? o) => o switch
    {
        null => Value.ForNull(),
        string s => Value.ForString(s),
        bool b => Value.ForBool(b),
        int i => Value.ForNumber(i),
        long l => Value.ForNumber(l),
        float f => Value.ForNumber(f),
        double d => Value.ForNumber(d),
        decimal m => Value.ForNumber((double)m),
        // 枚举与其它叶类型按字符串走：Struct 只有一种数值类型，
        // 把 enum 塞成数字会让读的人对不上号
        System.Enum e => Value.ForString(e.ToString()),
        IDictionary<string, object?> nested => Value.ForStruct(MapToStruct(nested)),
        System.Collections.IEnumerable list => BuildList(list),
        _ => Value.ForString(o.ToString() ?? string.Empty),
    };

    private static Value BuildList(System.Collections.IEnumerable items)
    {
        var list = new ListValue();
        foreach (var item in items)
        {
            list.Values.Add(ObjectToValue(item));
        }

        return Value.ForList(list.Values.ToArray());
    }
}
