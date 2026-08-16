// Debug-only。這支從未註冊進 DI(MauiProgram 只綁 NativeCore),要用是手動改那一行 —— 而那
// 只發生在設計時預覽/hot reload,也就是 Debug。Release 編它進去只有壞處:一份用不到的死碼,
// 外加它的反射式 JsonSerializer 會讓整個組件無法通過 NativeAOT/full-trim 分析(IL2026/IL3050)。
#if DEBUG
using System.Text.Json;

namespace TronClass.Interop;

/// <summary>
/// A design-time fake <see cref="ICore"/>. It scripts a realistic event timeline in the REAL core's
/// vocabulary (<c>Caps / VaultState / Accounts / AccountStatus / RollcallDetected / Countdown /
/// SignedIn / QuizPrepared / ReasoningChunk / QuizSubmitted / …</c>) so the whole UI — tabs, the
/// hero-moment popup, the core-owned countdown, the LLM reasoning stream, the multi-account merge —
/// can be built and previewed WITHOUT the native library. **Every command below produces a visible
/// response**, so you can wire and preview any button. Flip <c>MauiProgram</c> to <see cref="NativeCore"/>
/// for the real core; the UI does not change. Field names/shapes are verbatim from the wire
/// contract implemented by <c>QuizPreparedContract.cs</c> and <c>core/src/protocol.rs</c>.
/// </summary>
public sealed class MockCore : ICore
{
    public event Action<JsonElement>? EventReceived;

    public JsonElement? LastCaps { get; private set; }
    public JsonElement? LastProviders { get; private set; }
    public JsonElement? LastAccounts { get; private set; }
    public JsonElement? LastVaultState { get; private set; }
    public JsonElement? LastNextClass { get; private set; }

    private const string BaseUrl = "https://ilearn.thu.edu.tw";

    // Mutable so the Accounts tab is fully interactive in preview (Add/Delete/Switch re-emit Accounts).
    // teacher/course mirror AccountMeta (config.rs) so the teacher badge + QR-assist entry preview live.
    private readonly List<(string id, string label, string user, string school, bool teacher, string? course)> _accounts = new()
    {
        ("a1", "我的東海", "s1109999@thu.edu.tw", "thu", false, null),
        ("a2", "公有雲測試", "demo@example.com", "tronclass", false, null),
        ("a3", "課堂教師機", "teacher@thu.edu.tw", "thu", true, "55379"),
    };
    private string _active = "a1";
    private int _nextId = 4;
    private string _rollcallToken = "";
    private string _quizToken = "";

    // 目前生效的設定;UpdateConfig/SetLlmKey 後更新並重發,讓設定頁的預覽是活的。
    private readonly Dictionary<string, object?> _settings = new()
    {
        ["countdown_secs"] = 15,
        ["attendance_gate_percent"] = 15.0,
        ["llm_endpoint"] = "https://integrate.api.nvidia.com/v1/chat/completions",
        ["llm_model"] = "minimaxai/minimax-m3",
        ["llm_max_tokens"] = 16384,
        ["resubmit_for_correct"] = true,
        ["enable_llm_tools"] = true,
        ["has_llm_key"] = false,
    };

    public Task BootAsync(string dataDir)
    {
        Emit(new { id = (object?)null, @event = "Caps", caps = new {
            background_monitoring = true, self_update = false,
            qr_teacher_assist = true, ocr_captcha = false } });
        Emit(new { id = (object?)null, @event = "StateChanged", state = "idle" });
        // 核心以 device-key 自動解鎖：Init 後即 unlocked（使用者不需輸入主密碼）。
        Emit(new { id = (object?)null, @event = "VaultState", exists = true, unlocked = true });
        Emit(new { id = (object?)null, @event = "Providers", default_key = "thu", schools = new[] {
            new { key = "thu", label = "Tunghai University iLearn", base_url = BaseUrl },
            new { key = "tronclass", label = "TronClass Public Cloud", base_url = "https://www.tronclass.com.tw" } } });
        EmitAccounts();
        // The soonest upcoming class across monitored accounts (real core derives it from /api/my-courses).
        // Emit null instead to preview the "no upcoming class → card hidden" state.
        Emit(new { id = (object?)null, @event = "NextClass", account_id = "a1", course = "行銷管理",
            start_time = DateTime.Now.AddHours(2).ToString("yyyy-MM-ddTHH:mm:sszzz"), location = "管院 A203" });
        EmitSettings();
        return Task.CompletedTask;
    }

    public Task<JsonElement> SendAsync(string cmd, params (string Key, object? Value)[] fields)
    {
        var f = new Dictionary<string, object?>();
        foreach (var (k, v) in fields) f[k] = v;
        string? Str(string k) => f.TryGetValue(k, out var v) ? v?.ToString() : null;

        switch (cmd)
        {
            case "AddAccount":
                _accounts.Add(($"a{_nextId++}", Str("label") ?? "新帳號", Str("username") ?? "", Str("school") ?? "thu",
                    f.TryGetValue("is_teacher", out var it) && it is true, Str("course_id")));
                EmitAccounts();
                break;
            case "DeleteAccount":
                _accounts.RemoveAll(a => a.id == Str("account_id"));
                if (_active == Str("account_id")) _active = _accounts.Count > 0 ? _accounts[0].id : "";
                EmitAccounts();
                break;
            case "SwitchAccount":
                if (Str("account_id") is { } sw) _active = sw;
                EmitAccounts();
                break;
            case "Login":
                Emit(new { id = (object?)null, @event = "AccountStatus", account_id = Str("account_id"), state = "online" });
                return Task.FromResult(Json(new { id = 0, @event = "LoginResult", ok = true, detail = "logged in" }));
            case "ImportCookies":
            case "SubmitCaptcha":
                Emit(new { id = (object?)null, @event = "AccountStatus", account_id = Str("account_id"), state = "online" });
                break;

            case "StartMonitoring":
                _ = RunMonitoringScript();
                break;
            case "StopMonitoring":
                Emit(new { id = (object?)null, @event = "StateChanged", state = "idle" });
                break;

            case "SignNow": // signs every participant of the activity (merge model)
                if (Str("activity_token") != _rollcallToken) return Failed("unknown rollcall activity_token");
                foreach (var a in new[] { "a1", "a2" })
                    Emit(new { id = (object?)null, @event = "SignedIn",
                        activity_token = _rollcallToken, rollcall_id = "30558", account_id = a, course = "行銷管理", method = "radar" });
                break;
            case "DeferSignIn":
                if (Str("activity_token") != _rollcallToken) return Failed("unknown rollcall activity_token");
                Emit(new { id = (object?)null, @event = "PendingSignIn", activity_token = _rollcallToken, rollcall_id = "30558" });
                break;

            case "SetAnswer": // user overrides one subject for ONE account → that account's conflict resolved
                if (Str("activity_token") != _quizToken) return Failed("unknown quiz activity_token");
                if (!f.TryGetValue("answer", out var answer) || answer is not Ui.AnswerWire wire)
                    return Failed("answer payload is empty or malformed");
                Emit(new { id = (object?)null, @event = "AnswerUpdated",
                    activity_token = _quizToken, quiz_id = "32877", account_id = Str("account_id") ?? _active,
                    subject_id = Str("subject_id"), answer = wire, display_answer = wire.Display, source = "user", conflict = false });
                break;
            case "SubmitNow":
                if (Str("activity_token") != _quizToken) return Failed("unknown quiz activity_token");
                foreach (var a in new[] { "a1", "a2" })
                    Emit(new { id = (object?)null, @event = "QuizSubmitted",
                        activity_token = _quizToken, quiz_id = "32877", account_id = a, result = "submitted (score 60)" });
                break;
            case "DiscardAnswer":
                if (Str("activity_token") != _quizToken) return Failed("unknown quiz activity_token");
                Emit(new { id = (object?)null, @event = "LogLine", level = "info", activity_token = _quizToken,
                    text = "quiz 32877 答案已捨棄，不送出" });
                break;
            case "HoldAnswer":
                if (Str("activity_token") != _quizToken) return Failed("unknown quiz activity_token");
                Emit(new { id = (object?)null, @event = "LogLine", level = "info", activity_token = _quizToken,
                    text = "quiz 32877 已暫緩，停止自動送出" });
                break;
            case "UpdateConfig":
                if (f.TryGetValue("patch", out var p) && p is IDictionary<string, object?> patch)
                    foreach (var kv in patch) _settings[kv.Key] = kv.Value;
                EmitSettings();
                break;
            case "SetLlmKey":
                _settings["has_llm_key"] = true;
                EmitSettings();
                break;
            // Shutdown: no event needed — the Reply below is the whole response.
        }
        return Task.FromResult(Json(new { id = 0, @event = "Reply", ok = true, error = (object?)null }));

        static Task<JsonElement> Failed(string error) =>
            Task.FromResult(Json(new { id = 0, @event = "Reply", ok = false, error }));
    }

    private void EmitSettings() => Emit(new { id = (object?)null, @event = "Settings", settings = _settings });

    /// One pass of the time-limited flows: a radar rollcall (detect → 15s countdown → sign each account),
    /// then an exam (prepared per-account with 1 conflict → LLM reasoning stream → 15s countdown → submit).
    private async Task RunMonitoringScript()
    {
        Emit(new { id = (object?)null, @event = "StateChanged", state = "monitoring" });
        foreach (var a in _accounts)
            Emit(new { id = (object?)null, @event = "AccountStatus", account_id = a.id, state = "online" });
        await Task.Delay(2500);

        const string rc = "30558";
        _rollcallToken = $"mock-rollcall-{Guid.NewGuid():N}";
        Emit(new { id = (object?)null, @event = "RollcallDetected", activity_token = _rollcallToken,
            rollcall_id = rc, base_url = BaseUrl,
            kind = "radar", course = "行銷管理", attendance_rate = (object?)null, accounts = new[] { "a1", "a2" } });
        // 未達門檻:core 不倒數,只每秒回報即時簽到率(UI 用它填倒數欄位)。爬過門檻才 holding=false → 開始倒數。
        var gate = Convert.ToDouble(_settings["attendance_gate_percent"]);
        foreach (var r in new[] { 3.7, 7.4, 11.1, 14.8 })
        {
            Emit(new { id = (object?)null, @event = "RollcallGate", activity_token = _rollcallToken,
                rollcall_id = rc, rate = r, gate_percent = gate, holding = true });
            await Task.Delay(900);
        }
        Emit(new { id = (object?)null, @event = "RollcallGate", activity_token = _rollcallToken,
            rollcall_id = rc, rate = 42.0, gate_percent = gate, holding = false });
        for (var s = 15; s >= 0; s--)
        {
            Emit(new { id = (object?)null, @event = "Countdown", scope = "rollcall",
                activity_token = _rollcallToken, external_id = rc, remaining_secs = s });
            await Task.Delay(700);
        }
        foreach (var a in new[] { "a1", "a2" })
            Emit(new { id = (object?)null, @event = "SignedIn", activity_token = _rollcallToken,
                rollcall_id = rc, account_id = a, course = "行銷管理", method = "radar" });
        await Task.Delay(1500);

        const string qz = "32877";
        // BOTH accounts conflict on Q1 → conflict_count 2. Lets the UI preview the multi-account gate:
        // submit stays LOCKED until every account's conflict is resolved (resolve one → still locked;
        // resolve both → unlocks). Never silently overwrite a user's existing answer.
        await EmitQuizPreparedFixture();
        foreach (var chunk in new[] { "讓我想想，", "第一題問台灣最高峰，", "玉山 3952 公尺，", "所以答案是玉山。" })
        {
            Emit(new { id = (object?)null, @event = "ReasoningChunk", activity_token = _quizToken,
                subject_id = "1", text = chunk });
            await Task.Delay(450);
        }
        for (var s = 15; s >= 0; s--)
        {
            Emit(new { id = (object?)null, @event = "Countdown", scope = "quiz",
                activity_token = _quizToken, external_id = qz, remaining_secs = s });
            await Task.Delay(700);
        }
        foreach (var a in new[] { "a1", "a2" })
            Emit(new { id = (object?)null, @event = "QuizSubmitted", activity_token = _quizToken,
                quiz_id = qz, account_id = a, result = "submitted (score 60)" });
    }

    private async Task EmitQuizPreparedFixture()
    {
        await using var stream = await FileSystem.OpenAppPackageFileAsync("contract/quiz_prepared_v1.json");
        using var document = await JsonDocument.ParseAsync(stream);
        var fixture = document.RootElement.Clone();
        _quizToken = fixture.GetProperty("activity_token").GetString() ?? "";
        Emit(fixture);
    }

    private void EmitAccounts() => Emit(new { id = (object?)null, @event = "Accounts", active = _active,
        accounts = _accounts.ConvertAll(a => new { id = a.id, label = a.label, username = a.user, school_ref = a.school,
            is_teacher = a.teacher, course_id = a.course }) });

    private void Emit(object o) => Emit(Json(o));

    private void Emit(JsonElement el)
    {
        if (el.TryGetProperty("event", out var ev))
        {
            switch (ev.GetString())
            {
                case "Caps": LastCaps = el; break;
                case "Providers": LastProviders = el; break;
                case "Accounts": LastAccounts = el; break;
                case "VaultState": LastVaultState = el; break;
                case "NextClass": LastNextClass = el; break;
            }
        }
        EventReceived?.Invoke(el);
    }

    // 這裡刻意保留反射式序列化:mock 的 payload 是一堆一次性匿名型別,手寫 writer 只會讓
    // 「照著真核心 wire 格式抄」這件事更難看清楚。豁免範圍僅此一個方法,而且整個 MockCore
    // 只存在於 Debug(見檔頭 #if DEBUG),永遠不會進入 NativeAOT/trim 的出貨路徑。
#pragma warning disable IL2026, IL3050 // Debug-only 設計時假核心,不出貨
    private static JsonElement Json(object o) => JsonSerializer.SerializeToElement(o);
#pragma warning restore IL2026, IL3050
}
#endif
