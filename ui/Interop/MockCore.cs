using System.Text.Json;

namespace TronClass.Interop;

/// <summary>
/// A design-time fake <see cref="ICore"/>. It scripts a realistic event timeline in the REAL core's
/// vocabulary (<c>Caps / VaultState / Accounts / AccountStatus / RollcallDetected / Countdown /
/// SignedIn / QuizPrepared / ReasoningChunk / QuizSubmitted / …</c>) so the whole UI — tabs, the
/// hero-moment popup, the core-owned countdown, the LLM reasoning stream, the multi-account merge —
/// can be built and previewed WITHOUT the native library. **Every command below produces a visible
/// response**, so you can wire and preview any button. Flip <c>MauiProgram</c> to <see cref="NativeCore"/>
/// for the real core; the UI does not change. Field names/shapes are verbatim from <c>docs/20-contract.md</c>.
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
    private readonly List<(string id, string label, string user, string school)> _accounts = new()
    {
        ("a1", "我的東海", "s1109999@thu.edu.tw", "thu"),
        ("a2", "公有雲測試", "demo@example.com", "tronclass"),
    };
    private string _active = "a1";
    private int _nextId = 3;

    public Task BootAsync(string dataDir)
    {
        Emit(new { id = (object?)null, @event = "Caps", caps = new {
            background_monitoring = true, self_update = true, biometric_unlock = false,
            qr_teacher_assist = true, ocr_captcha = false } });
        Emit(new { id = (object?)null, @event = "StateChanged", state = "idle" });
        Emit(new { id = (object?)null, @event = "VaultState", exists = true, unlocked = false });
        Emit(new { id = (object?)null, @event = "Providers", default_key = "thu", schools = new[] {
            new { key = "thu", label = "Tunghai University iLearn", base_url = BaseUrl },
            new { key = "tronclass", label = "TronClass Public Cloud", base_url = "https://www.tronclass.com.tw" } } });
        EmitAccounts();
        // The soonest upcoming class across monitored accounts (real core derives it from /api/my-courses).
        // Emit null instead to preview the "no upcoming class → card hidden" state.
        Emit(new { id = (object?)null, @event = "NextClass", account_id = "a1", course = "行銷管理",
            start_time = DateTime.Now.AddHours(2).ToString("yyyy-MM-ddTHH:mm:sszzz"), location = "管院 A203" });
        return Task.CompletedTask;
    }

    public Task<JsonElement> SendAsync(string cmd, params (string Key, object? Value)[] fields)
    {
        var f = new Dictionary<string, object?>();
        foreach (var (k, v) in fields) f[k] = v;
        string? Str(string k) => f.TryGetValue(k, out var v) ? v?.ToString() : null;

        switch (cmd)
        {
            case "CreateVault":
            case "Unlock":
            case "UnlockWithKeystore":
                Emit(new { id = (object?)null, @event = "VaultState", exists = true, unlocked = true });
                break;
            case "LockVault":
                Emit(new { id = (object?)null, @event = "VaultState", exists = true, unlocked = false });
                break;

            case "AddAccount":
                _accounts.Add(($"a{_nextId++}", Str("label") ?? "新帳號", Str("username") ?? "", Str("school") ?? "thu"));
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
                foreach (var a in new[] { "a1", "a2" })
                    Emit(new { id = (object?)null, @event = "SignedIn",
                        rollcall_id = Str("rollcall_id"), account_id = a, course = "行銷管理", method = "radar" });
                break;
            case "DeferSignIn":
                Emit(new { id = (object?)null, @event = "PendingSignIn", rollcall_id = Str("rollcall_id") });
                break;

            case "SetAnswer": // user overrides one subject for ONE account → that account's conflict resolved
                Emit(new { id = (object?)null, @event = "AnswerUpdated",
                    quiz_id = Str("quiz_id"), account_id = Str("account_id") ?? _active, subject_id = Str("subject_id"), source = "user", conflict = false });
                break;
            case "SubmitNow":
                foreach (var a in new[] { "a1", "a2" })
                    Emit(new { id = (object?)null, @event = "QuizSubmitted",
                        quiz_id = Str("quiz_id"), account_id = a, result = "submitted (score 60)" });
                break;
            case "DiscardAnswer":
                Emit(new { id = (object?)null, @event = "LogLine", level = "info", text = $"quiz {Str("quiz_id")} 答案已捨棄，不送出" });
                break;
            case "HoldAnswer":
                Emit(new { id = (object?)null, @event = "LogLine", level = "info", text = $"quiz {Str("quiz_id")} 已暫緩，停止自動送出" });
                break;
            // UpdateConfig / SetLlmKey / Shutdown: no event needed — the Reply below is the whole response.
        }
        return Task.FromResult(Json(new { id = 0, @event = "Reply", ok = true, error = (object?)null }));
    }

    /// One pass of the time-limited flows: a radar rollcall (detect → 15s countdown → sign each account),
    /// then an exam (prepared per-account with 1 conflict → LLM reasoning stream → 15s countdown → submit).
    private async Task RunMonitoringScript()
    {
        Emit(new { id = (object?)null, @event = "StateChanged", state = "monitoring" });
        foreach (var a in _accounts)
            Emit(new { id = (object?)null, @event = "AccountStatus", account_id = a.id, state = "online" });
        await Task.Delay(2500);

        const string rc = "30558";
        Emit(new { id = (object?)null, @event = "RollcallDetected", rollcall_id = rc, base_url = BaseUrl,
            kind = "radar", course = "行銷管理", attendance_rate = 42.0, accounts = new[] { "a1", "a2" } });
        for (var s = 15; s >= 0; s--)
        {
            Emit(new { id = (object?)null, @event = "Countdown", scope = "rollcall", id_ = rc, remaining_secs = s });
            await Task.Delay(700);
        }
        foreach (var a in new[] { "a1", "a2" })
            Emit(new { id = (object?)null, @event = "SignedIn", rollcall_id = rc, account_id = a, course = "行銷管理", method = "radar" });
        await Task.Delay(1500);

        const string qz = "32877";
        // BOTH accounts conflict on Q1 → conflict_count 2. Lets the UI preview the multi-account gate:
        // submit stays LOCKED until every account's conflict is resolved (resolve one → still locked;
        // resolve both → unlocks). Never silently overwrite a user's existing answer.
        Emit(new { id = (object?)null, @event = "QuizPrepared", quiz_id = qz, course = "行銷管理", conflict_count = 2,
            per_account = new[] {
                new { account_id = "a1", questions = new[] {
                    new { subject_id = "1", stem = "台灣最高的山是哪一座？", answer = "玉山", conflict = true },
                    new { subject_id = "2", stem = "水的化學式是？", answer = "H2O", conflict = false } } },
                new { account_id = "a2", questions = new[] {
                    new { subject_id = "1", stem = "台灣最高的山是哪一座？", answer = "玉山", conflict = true },
                    new { subject_id = "2", stem = "水的化學式是？", answer = "H2O", conflict = false } } } } });
        foreach (var chunk in new[] { "讓我想想，", "第一題問台灣最高峰，", "玉山 3952 公尺，", "所以答案是玉山。" })
        {
            Emit(new { id = (object?)null, @event = "ReasoningChunk", quiz_id = qz, subject_id = "1", text = chunk });
            await Task.Delay(450);
        }
        for (var s = 15; s >= 0; s--)
        {
            Emit(new { id = (object?)null, @event = "Countdown", scope = "quiz", id_ = qz, remaining_secs = s });
            await Task.Delay(700);
        }
        foreach (var a in new[] { "a1", "a2" })
            Emit(new { id = (object?)null, @event = "QuizSubmitted", quiz_id = qz, account_id = a, result = "submitted (score 60)" });
    }

    private void EmitAccounts() => Emit(new { id = (object?)null, @event = "Accounts", active = _active,
        accounts = _accounts.ConvertAll(a => new { id = a.id, label = a.label, username = a.user, school_ref = a.school }) });

    private void Emit(object o)
    {
        var el = Json(o);
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

    private static JsonElement Json(object o) => JsonSerializer.SerializeToElement(o);
}
