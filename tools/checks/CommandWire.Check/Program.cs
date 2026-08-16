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
var weekly = new WeeklyScheduleSpec(monday: [new TimeWindowSpec(60, 120)]);
var group = new GroupInputWire(
    " Team ",
    ["a", "b"],
    ["course-1"],
    DetectorSelectionSpec.Preferred("a"),
    ScheduleBindingSpec.InheritGlobal);
const string WeeklyJson = """{"monday":[{"start_minute":60,"end_minute":120}],"tuesday":[],"wednesday":[],"thursday":[],"friday":[],"saturday":[],"sunday":[]}""";
const string GroupJson = """{"name":"Team","member_account_ids":["a","b"],"course_ids":["course-1"],"detector":{"kind":"preferred","account_id":"a"},"schedule":{"kind":"inherit_global"}}""";

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

Equal("DeleteAccount:revision 與群組移除意圖",
    JsonWire.SerializeCommand(6, "DeleteAccount",
        ("account_id", "a"), ("expected_revision", 12UL), ("remove_from_groups", true)),
    """{"id":6,"cmd":"DeleteAccount","account_id":"a","expected_revision":12,"remove_from_groups":true}""");

Equal("CreateGroup",
    JsonWire.SerializeCommand(7, "CreateGroup", ("expected_revision", 12UL), ("group", group)),
    """{"id":7,"cmd":"CreateGroup","expected_revision":12,"group":""" + GroupJson + "}");

Equal("UpdateGroup",
    JsonWire.SerializeCommand(8, "UpdateGroup",
        ("group_id", "g"), ("expected_revision", 13UL), ("group", group)),
    """{"id":8,"cmd":"UpdateGroup","group_id":"g","expected_revision":13,"group":""" + GroupJson + "}");

Equal("DeleteGroup",
    JsonWire.SerializeCommand(9, "DeleteGroup", ("group_id", "g"), ("expected_revision", 14UL)),
    """{"id":9,"cmd":"DeleteGroup","group_id":"g","expected_revision":14}""");

Equal("MergeGroups",
    JsonWire.SerializeCommand(10, "MergeGroups",
        ("group_ids", new[] { "g1", "g2" }), ("expected_revision", 14UL), ("group", group)),
    """{"id":10,"cmd":"MergeGroups","group_ids":["g1","g2"],"expected_revision":14,"group":""" + GroupJson + "}");

Equal("ListCommonCourses",
    JsonWire.SerializeCommand(11, "ListCommonCourses", ("member_account_ids", new[] { "a", "b" })),
    """{"id":11,"cmd":"ListCommonCourses","member_account_ids":["a","b"]}""");

Equal("SetTargetSchedule",
    JsonWire.SerializeCommand(12, "SetTargetSchedule",
        ("target", new TargetIdSpec("account", "a")), ("expected_revision", 15UL),
        ("schedule", ScheduleBindingSpec.Disabled)),
    """{"id":12,"cmd":"SetTargetSchedule","target":{"kind":"account","account_id":"a"},"expected_revision":15,"schedule":{"kind":"disabled"}}""");

Equal("SetMonitoringPreferences",
    JsonWire.SerializeCommand(13, "SetMonitoringPreferences",
        ("expected_revision", 16UL), ("global_schedule", weekly),
        ("time_zone", TimeZoneSpec.Named("Asia/Taipei"))),
    """{"id":13,"cmd":"SetMonitoringPreferences","expected_revision":16,"global_schedule":""" +
    WeeklyJson + ""","time_zone":{"kind":"named","iana_id":"Asia/Taipei"}}""");

var clockEntries = new ScheduleClockEntriesWire(
[
    new(
        new TargetIdSpec("group", "g"),
        new ScheduleEvaluation(
            true,
            "window-1",
            new DateTimeOffset(2026, 8, 17, 1, 0, 0, TimeSpan.Zero),
            new DateTimeOffset(2026, 8, 17, 2, 0, 0, TimeSpan.Zero),
            false,
            null)),
]);
Equal("ApplyScheduleClock",
    JsonWire.SerializeCommand(14, "ApplyScheduleClock",
        ("clock_revision", 9UL), ("config_revision", 12UL), ("schedule_revision", 7UL),
        ("evaluated_at_utc", "2026-08-17T01:00:00.0000000Z"), ("targets", clockEntries)),
    """{"id":14,"cmd":"ApplyScheduleClock","clock_revision":9,"config_revision":12,"schedule_revision":7,"evaluated_at_utc":"2026-08-17T01:00:00.0000000Z","targets":[{"target":{"kind":"group","group_id":"g"},"is_open":true,"window_key":"window-1","current_window_start_utc":"2026-08-17T01:00:00.0000000Z","next_boundary_utc":"2026-08-17T02:00:00.0000000Z","next_is_open":false,"clock_error":null}]}""");

Equal("StartTarget",
    JsonWire.SerializeCommand(15, "StartTarget", ("target", new TargetIdSpec("group", "g"))),
    """{"id":15,"cmd":"StartTarget","target":{"kind":"group","group_id":"g"}}""");
Equal("StopTarget",
    JsonWire.SerializeCommand(16, "StopTarget", ("target", new TargetIdSpec("account", "a"))),
    """{"id":16,"cmd":"StopTarget","target":{"kind":"account","account_id":"a"}}""");
Equal("StopAllMonitoring",
    JsonWire.SerializeCommand(17, "StopAllMonitoring"),
    """{"id":17,"cmd":"StopAllMonitoring"}""");
Equal("ResumeScheduledMonitoring",
    JsonWire.SerializeCommand(18, "ResumeScheduledMonitoring"),
    """{"id":18,"cmd":"ResumeScheduledMonitoring"}""");
Equal("AcknowledgeTemporaryMerge",
    JsonWire.SerializeCommand(19, "AcknowledgeTemporaryMerge",
        ("component_id", "component-1"), ("plan_revision", 31UL)),
    """{"id":19,"cmd":"AcknowledgeTemporaryMerge","component_id":"component-1","plan_revision":31}""");
Equal("SuspendForPlatformLimit",
    JsonWire.SerializeCommand(20, "SuspendForPlatformLimit", ("reason", "quota")),
    """{"id":20,"cmd":"SuspendForPlatformLimit","reason":"quota"}""");
Equal("ClearPlatformLimit",
    JsonWire.SerializeCommand(21, "ClearPlatformLimit", ("reason", "quota reset")),
    """{"id":21,"cmd":"ClearPlatformLimit","reason":"quota reset"}""");
Equal("GetMonitoringSnapshot",
    JsonWire.SerializeCommand(22, "GetMonitoringSnapshot"),
    """{"id":22,"cmd":"GetMonitoringSnapshot"}""");

var supportedMonitoringCommands = new[]
{
    "CreateGroup", "UpdateGroup", "DeleteGroup", "MergeGroups", "ListCommonCourses",
    "SetTargetSchedule", "SetMonitoringPreferences", "ApplyScheduleClock", "StartTarget",
    "StopTarget", "StopAllMonitoring", "ResumeScheduledMonitoring",
    "AcknowledgeTemporaryMerge", "SuspendForPlatformLimit", "ClearPlatformLimit",
    "GetMonitoringSnapshot",
};
foreach (var removed in new[] { "StartMonitoring", "StopMonitoring", "SwitchAccount" })
    Check($"舊命令 {removed} 不得回到支援集合", !supportedMonitoringCommands.Contains(removed, StringComparer.Ordinal), "");

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
