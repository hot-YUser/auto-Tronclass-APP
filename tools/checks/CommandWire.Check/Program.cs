// 出站命令信封的線上形態守門員。
//
// 為什麼需要這支:ProtocolContract.Check 只驗「入站」(core → UI 的 QuizPrepared 解析)。
// 命令的「出站」序列化在此之前完全沒有測試覆蓋 —— 而它就是 UI 對核心說話的唯一管道,
// 鍵名或型別錯一個字,核心就收到看不懂的命令。
//
// 期望值不是推導出來的,是從改寫前的 JsonSerializer 反射實作實際跑出來捕捉的,用來釘住
// JsonWire(手寫 Utf8JsonWriter)與舊實作等價。三個刻意保留的行為:
//   1. 非 ASCII 逃逸成 \uXXXX(預設 JavaScriptEncoder)—— 由「全 ASCII + 無損往返」兩條斷言鎖住,
//      不手寫逃逸字面值(那既難讀又容易抄錯,反而變成假的 ground truth)
//   2. null 照寫,不略過
//   3. 鍵序 = 插入序(id、cmd,然後依呼叫端給的順序)
using System.Text.Json;
using TronClass.Interop;
using Ui;

var failures = 0;

// ---- 逐位元組釘樁(純 ASCII 情境,期望值直接可讀)----

Equal("字串/bool/null 全形狀:null 必須寫出,不得略過",
    JsonWire.SerializeCommand(1, "AddAccount",
        ("label", "demo"), ("school", "thu"), ("username", "u"), ("password", "p"),
        ("is_teacher", false), ("course_id", null)),
    """{"id":1,"cmd":"AddAccount","label":"demo","school":"thu","username":"u","password":"p","is_teacher":false,"course_id":null}""");

Equal("UpdateConfig:巢狀 patch 的 int + double",
    JsonWire.SerializeCommand(2, "UpdateConfig", ("patch", new Dictionary<string, object?>
    {
        ["countdown_secs"] = 30,
        ["attendance_gate_percent"] = 12.5,
    })),
    """{"id":2,"cmd":"UpdateConfig","patch":{"countdown_secs":30,"attendance_gate_percent":12.5}}""");

Equal("UpdateConfig:巢狀 patch 的 string + int + bool",
    JsonWire.SerializeCommand(3, "UpdateConfig", ("patch", new Dictionary<string, object?>
    {
        ["llm_endpoint"] = "https://x/v1",
        ["llm_model"] = "m",
        ["llm_max_tokens"] = 4096,
        ["resubmit_for_correct"] = true,
        ["enable_llm_tools"] = false,
    })),
    """{"id":3,"cmd":"UpdateConfig","patch":{"llm_endpoint":"https://x/v1","llm_model":"m","llm_max_tokens":4096,"resubmit_for_correct":true,"enable_llm_tools":false}}""");

Equal("SetAnswer:AnswerWire 陣列種類(null 成員整個略過,不送 null)",
    JsonWire.SerializeCommand(4, "SetAnswer", ("activity_token", "t"), ("account_id", "a"),
        ("subject_id", "1"), ("answer", new AnswerWire { Kind = "options", OptionIds = ["o1", "o2"] })),
    """{"id":4,"cmd":"SetAnswer","activity_token":"t","account_id":"a","subject_id":"1","answer":{"kind":"options","option_ids":["o1","o2"]}}""");

Equal("SetAnswer:AnswerWire 純文字種類",
    JsonWire.SerializeCommand(5, "SetAnswer", ("activity_token", "t"), ("account_id", "a"),
        ("subject_id", "2"), ("answer", new AnswerWire { Kind = "text", Value = "Jade Mt" })),
    """{"id":5,"cmd":"SetAnswer","activity_token":"t","account_id":"a","subject_id":"2","answer":{"kind":"text","value":"Jade Mt"}}""");

Equal("無欄位命令",
    JsonWire.SerializeCommand(6, "StopMonitoring"),
    """{"id":6,"cmd":"StopMonitoring"}""");

// 本地合成的失敗 Reply:欄位形狀必須與核心的 Reply 信封一致,否則 AppState.OkReply 讀不到。
Equal("JsonWire.Object:失敗 Reply 信封",
    JsonWire.Object(("id", 99UL), ("event", "Reply"), ("ok", false), ("error", "core disposed")).GetRawText(),
    """{"id":99,"event":"Reply","ok":false,"error":"core disposed"}""");

// ---- 編碼行為釘樁(非 ASCII 與控制字元)----

const string Tricky = "引\"號\\與\n換行 中文";
var escaped = JsonWire.SerializeCommand(7, "SubmitCaptcha", ("account_id", "a"), ("text", Tricky));
Check("非 ASCII 必須逃逸成 \\uXXXX(與舊反射實作同編碼器)",
    escaped.All(char.IsAscii), $"輸出含未逃逸的非 ASCII 字元：{escaped}");
Check("逃逸必須無損:讀回來要與原字串逐字相同",
    JsonDocument.Parse(escaped).RootElement.GetProperty("text").GetString() == Tricky,
    "讀回的字串與原字串不符");

const string Cjk = "核心已釋放，無法執行命令。";
var reply = JsonWire.Object(("error", Cjk));
Check("Object() 同樣逃逸且無損", reply.GetRawText().All(char.IsAscii) && reply.GetProperty("error").GetString() == Cjk,
    $"實際 {reply.GetRawText()}");

// ---- AnswerWire 讀寫對稱 ----
// 寫出去的形狀,FromJson 要能原樣讀回來。注意 record 的 == 對 string[] 是參考比較,
// 所以比「再次序列化後的文字」,不比物件本身。
foreach (var wire in new[]
{
    new AnswerWire { Kind = "options", OptionIds = ["o1", "o2"] },
    new AnswerWire { Kind = "blanks", Values = ["甲", "乙"] },
    new AnswerWire { Kind = "text", Value = "玉山" },
    new AnswerWire { Kind = "vote", Letters = ["A"] },
})
{
    var written = JsonWire.Object(("answer", wire)).GetProperty("answer").GetRawText();
    using var parsed = JsonDocument.Parse(written);
    var back = AnswerWire.FromJson(parsed.RootElement);
    var rewritten = back is null ? "<null>" : JsonWire.Object(("answer", back)).GetProperty("answer").GetRawText();
    Check($"AnswerWire 讀寫對稱：{wire.Kind}", written == rewritten,
        $"\n    寫出     {written}\n    讀回再寫 {rewritten}");
}

// ---- 協定守衛 ----
// 沒有顯式支援的型別必須爆炸,不得靜默送出形狀不明的 JSON(舊的反射實作會默默序列化)。
try
{
    JsonWire.SerializeCommand(8, "Bogus", ("weird", new object()));
    Check("未知型別必須 fail-closed", false, "沒有拋出例外");
}
catch (InvalidOperationException)
{
    Check("未知型別必須 fail-closed", true, "");
}

Console.WriteLine(failures == 0
    ? "CommandWire.Check：全部通過"
    : $"CommandWire.Check：{failures} 項失敗");
return failures == 0 ? 0 : 1;

void Equal(string name, string actual, string expected) =>
    Check(name, actual == expected, $"\n    預期 {expected}\n    實際 {actual}");

void Check(string name, bool ok, string detail)
{
    if (ok) return;
    failures++;
    Console.Error.WriteLine($"契約檢查失敗：{name}{detail}");
}
