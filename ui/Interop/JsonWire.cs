using System.Buffers;
using System.Text;
using System.Text.Json;

namespace TronClass.Interop;

/// <summary>
/// 能把自己寫成 JSON 的出站 wire model。實作者自負欄位名與省略規則,
/// 與其手寫的 <c>FromJson</c> 並列成對(讀/寫都顯式,不靠反射或屬性推導)。
/// </summary>
public interface IWireValue
{
    void WriteTo(Utf8JsonWriter writer);
}

/// <summary>
/// UI → core 出站 JSON 的序列化。刻意手寫 <see cref="Utf8JsonWriter"/> 而不用
/// <c>JsonSerializer</c> 的反射路徑:
/// <list type="number">
/// <item>NativeAOT / full trimming 下反射序列化不可用(IL2026 + IL3050)。</item>
/// <item>這是協定邊界 —— 支援哪些值型別必須<b>顯式可見</b>;未知型別 fail-closed 拋例外,
///       而不是靜默地由反射決定線上形狀。</item>
/// <item>更快、少一次中間 <c>Dictionary</c> 配置。</item>
/// </list>
/// 輸出與先前的 <c>JsonSerializer.Serialize(Dictionary&lt;string, object?&gt;)</c> <b>逐位元組相同</b>
/// (預設 JavaScriptEncoder:非 ASCII 逃逸為 <c>\uXXXX</c>;null 照寫不略過;鍵序 = 插入序),
/// 等價性由 <c>tools/checks/CommandWire.Check</c> 釘住。
/// </summary>
public static class JsonWire
{
    /// <summary>命令信封:<c>{"id":…,"cmd":…,…欄位}</c>。</summary>
    public static string SerializeCommand(ulong id, string cmd, params (string Key, object? Value)[] fields)
    {
        var buffer = new ArrayBufferWriter<byte>(256);
        using (var writer = new Utf8JsonWriter(buffer))
        {
            writer.WriteStartObject();
            writer.WriteNumber("id", id);
            writer.WriteString("cmd", cmd);
            foreach (var (key, value) in fields)
            {
                writer.WritePropertyName(key);
                WriteValue(writer, value);
            }
            writer.WriteEndObject();
        }
        return Encoding.UTF8.GetString(buffer.WrittenSpan);
    }

    /// <summary>任意欄位組成的 JSON 物件,回傳可獨立存活的 <see cref="JsonElement"/>。</summary>
    public static JsonElement Object(params (string Key, object? Value)[] fields)
    {
        var buffer = new ArrayBufferWriter<byte>(128);
        using (var writer = new Utf8JsonWriter(buffer))
        {
            writer.WriteStartObject();
            foreach (var (key, value) in fields)
            {
                writer.WritePropertyName(key);
                WriteValue(writer, value);
            }
            writer.WriteEndObject();
        }
        // Parse 後必須 Clone:JsonDocument 一旦 dispose,其 JsonElement 就失效。
        using var document = JsonDocument.Parse(buffer.WrittenMemory);
        return document.RootElement.Clone();
    }

    /// <summary>
    /// 值分派。新增支援型別請一併補 <c>CommandWire.Check</c> 的釘樁 —— 這裡的 default
    /// 分支是協定守衛,寧可建置後第一次送命令就炸,也不要送出形狀不明的 JSON。
    /// </summary>
    static void WriteValue(Utf8JsonWriter writer, object? value)
    {
        switch (value)
        {
            case null: writer.WriteNullValue(); break;
            case string text: writer.WriteStringValue(text); break;
            case bool flag: writer.WriteBooleanValue(flag); break;
            case int number: writer.WriteNumberValue(number); break;
            case long number: writer.WriteNumberValue(number); break;
            case ulong number: writer.WriteNumberValue(number); break;
            case double number: writer.WriteNumberValue(number); break;
            case IWireValue wire: wire.WriteTo(writer); break;

            // UpdateConfig 的 patch:巢狀鍵值,值本身再走同一套分派。
            case IReadOnlyDictionary<string, object?> map:
                writer.WriteStartObject();
                foreach (var (key, nested) in map)
                {
                    writer.WritePropertyName(key);
                    WriteValue(writer, nested);
                }
                writer.WriteEndObject();
                break;

            case IEnumerable<string> items:
                writer.WriteStartArray();
                foreach (var item in items) writer.WriteStringValue(item);
                writer.WriteEndArray();
                break;

            default:
                throw new InvalidOperationException(
                    $"命令欄位不支援的型別 {value.GetType()}：協定邊界只序列化顯式支援的型別。" +
                    "請在 JsonWire.WriteValue 增加分支並同步 CommandWire.Check。");
        }
    }
}
