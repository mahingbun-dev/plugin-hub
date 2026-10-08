using System.Text;
using System.Text.Encodings.Web;
using System.Text.Json;

namespace HubKit;

/// <summary>日志级别。与 Go 侧 slog 的四个级别一一对应。</summary>
public enum HubLogLevel
{
    Debug,
    Info,
    Warn,
    Error,
}

/// <summary>
/// 结构化日志：一条记录一行 JSON、打到 stderr。
///
/// 形状**刻意与 Go 侧 slog 的 JSON handler 对齐**
/// （<c>{"time":"...","level":"INFO","msg":"已注册到中台","plugin":"...","version":"..."}</c>）：
/// 六门语言的插件会跑在同一个中台旁边，运维的采集规则、告警规则、`jq` 配方
/// 只该有一套。形状一变，那些规则就得按语言分支。
///
/// 为什么是 stderr 而不是 stdout：stdout 留给「插件的业务输出」，
/// 日志混进去会把有用的东西淹掉；Go 侧的 slog 缺省也是 stderr。
/// </summary>
public sealed class HubLogger
{
    /// <summary>
    /// 序列化选项。
    ///
    /// <c>UnsafeRelaxedJsonEscaping</c> 不是图省事，是**形状对齐**的必要条件：
    /// 缺省的编码器会把中文写成 <c>\\u5FC3\\u8DF3</c>、把时区里的加号写成 <c>\\u002B</c>，
    /// 而 Go 侧 slog 的 JSON handler 是原样写 UTF-8 的。两侧形状一散，
    /// 「用同一套 jq 配方读两边日志」就不成立了——而 <c>grep 心跳失败</c>
    /// 这种最朴素的排查动作会一声不吭地什么都搜不到。
    ///
    /// 说是 Unsafe，是因为它不转义 <c>&amp;lt; &amp;gt; &amp;amp;</c> 这类 HTML 敏感字符；
    /// 这是**日志**，出口是 stderr 不是浏览器，那条顾虑在这里不成立。
    /// </summary>
    private static readonly JsonWriterOptions WriterOptions = new()
    {
        Indented = false,
        Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
    };

    private readonly object _gate = new();

    private readonly TextWriter? _explicitWriter;

    public HubLogger(HubLogLevel minLevel = HubLogLevel.Info, TextWriter? writer = null)
    {
        MinLevel = minLevel;
        _explicitWriter = writer;
    }

    /// <summary>低于这个级别的记录被丢弃。</summary>
    public HubLogLevel MinLevel { get; }

    /// <summary>
    /// 按 <c>HUB_LOG_LEVEL</c> 构造（debug / info / warn / error，缺省 info）。
    ///
    /// 认不出的值落到 info 而不是报错：日志级别配错不该让插件起不来。
    /// </summary>
    public static HubLogger FromEnv(string variable = "HUB_LOG_LEVEL")
    {
        var raw = (Environment.GetEnvironmentVariable(variable) ?? string.Empty).Trim().ToLowerInvariant();
        var level = raw switch
        {
            "debug" => HubLogLevel.Debug,
            "warn" => HubLogLevel.Warn,
            "error" => HubLogLevel.Error,
            _ => HubLogLevel.Info,
        };
        return new HubLogger(level);
    }

    public bool IsEnabled(HubLogLevel level) => level >= MinLevel;

    public void Debug(string msg, params (string Key, object? Value)[] fields) => Write(HubLogLevel.Debug, msg, fields);

    public void Info(string msg, params (string Key, object? Value)[] fields) => Write(HubLogLevel.Info, msg, fields);

    public void Warn(string msg, params (string Key, object? Value)[] fields) => Write(HubLogLevel.Warn, msg, fields);

    public void Error(string msg, params (string Key, object? Value)[] fields) => Write(HubLogLevel.Error, msg, fields);

    private void Write(HubLogLevel level, string msg, (string Key, object? Value)[] fields)
    {
        if (!IsEnabled(level))
        {
            return;
        }

        var line = Render(level, msg, fields);

        // 一条记录必须**原子地**落盘：两个线程各写半行，读出来的就是两行都解析不了的垃圾。
        lock (_gate)
        {
            (_explicitWriter ?? Console.Error).WriteLine(line);
        }
    }

    /// <summary>把一条记录渲染成单行 JSON。公开出来是为了让测试能直接断言格式。</summary>
    public static string Render(HubLogLevel level, string msg, params (string Key, object? Value)[] fields)
    {
        using var buffer = new MemoryStream();
        using (var json = new Utf8JsonWriter(buffer, WriterOptions))
        {
            json.WriteStartObject();
            json.WriteString("time", Rfc3339Nano(DateTimeOffset.Now));
            json.WriteString("level", LevelName(level));
            json.WriteString("msg", msg);

            foreach (var (key, value) in fields)
            {
                json.WritePropertyName(key);
                WriteValue(json, value);
            }

            json.WriteEndObject();
        }

        return Encoding.UTF8.GetString(buffer.ToArray());
    }

    private static void WriteValue(Utf8JsonWriter json, object? value)
    {
        switch (value)
        {
            case null:
                json.WriteNullValue();
                break;
            case string s:
                json.WriteStringValue(s);
                break;
            case bool b:
                json.WriteBooleanValue(b);
                break;
            case int i:
                json.WriteNumberValue(i);
                break;
            case long l:
                json.WriteNumberValue(l);
                break;
            case double d:
                json.WriteNumberValue(d);
                break;
            case Exception ex:
                // 异常是「一条记录里的一列」，不是要展开的多行文本：展开会占掉多行，
                // 而采集侧是按行解析的，多出来的行会变成没有 msg 的孤儿
                json.WriteStringValue(ex.Message);
                break;
            default:
                json.WriteStringValue(value.ToString());
                break;
        }
    }

    /// <summary>
    /// Go 的 <c>time.RFC3339Nano</c> 形状：小数秒**去掉尾随零**、整数秒时不带小数点。
    ///
    /// 照抄它的理由不是为了好看——两侧日志会被同一套解析规则读，
    /// 一边是 <c>…:43.521388+08:00</c>、另一边是 <c>…:43.5213880+08:00</c>，
    /// 按字符串排序或去重时就会分成两组。
    /// </summary>
    internal static string Rfc3339Nano(DateTimeOffset t)
    {
        var body = t.ToString("yyyy-MM-ddTHH:mm:ss");
        var fraction = t.ToString("ffffff").TrimEnd('0');
        var offset = t.Offset == TimeSpan.Zero ? "Z" : t.ToString("zzz");

        return fraction.Length == 0
            ? body + offset
            : $"{body}.{fraction}{offset}";
    }

    private static string LevelName(HubLogLevel level) => level switch
    {
        HubLogLevel.Debug => "DEBUG",
        HubLogLevel.Info => "INFO",
        HubLogLevel.Warn => "WARN",
        HubLogLevel.Error => "ERROR",
        _ => "INFO",
    };
}
