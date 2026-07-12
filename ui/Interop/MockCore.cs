using System.Text.Json;

namespace TronClass.Interop;

/// <summary>
/// A design-time fake <see cref="ICore"/>. It scripts a realistic event timeline in the REAL core's
/// vocabulary (<c>Caps / VaultState / Accounts / AccountStatus / RollcallDetected / Countdown /
/// SignedIn / QuizPrepared / ReasoningChunk / QuizSubmitted / …</c>) so the whole UI — tabs, the
/// hero-moment popup, the core-owned countdown, the LLM reasoning stream, the multi-account merge —
/// can be built and previewed WITHOUT the native library. Flip <c>MauiProgram</c> to
/// <see cref="NativeCore"/> for the real core; the UI does not change. Every field name/shape below
/// is taken verbatim from the real emit sites (see <c>docs/20-contract.md</c>).
/// </summary>
public sealed class MockCore : ICore
{
    public event Action<JsonElement>? EventReceived;

    public JsonElement? LastCaps { get; private set; }
    public JsonElement? LastProviders { get; private set; }
    public JsonElement? LastAccounts { get; private set; }
    public JsonElement? LastVaultState { get; private set; }

    private const string BaseUrl = "https://ilearn.thu.edu.tw";

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
        Emit(new { id = (object?)null, @event = "Accounts", active = "a1", accounts = new[] {
            new { id = "a1", label = "我的東海", username = "s1109999@thu.edu.tw", school_ref = "thu" },
            new { id = "a2", label = "公有雲測試", username = "demo@example.com", school_ref = "tronclass" } } });
        return Task.CompletedTask;
    }

    public Task<JsonElement> SendAsync(string cmd, params (string Key, object? Value)[] fields)
    {
        var f = new Dictionary<string, object?>();
        foreach (var (k, v) in fields) f[k] = v;

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
            case "StartMonitoring":
                _ = RunMonitoringScript();
                break;
            case "StopMonitoring":
                Emit(new { id = (object?)null, @event = "StateChanged", state = "idle" });
                break;
            case "SignNow": // signs every participant of the activity (merge model)
                foreach (var a in new[] { "a1", "a2" })
                    Emit(new { id = (object?)null, @event = "SignedIn",
                        rollcall_id = f.GetValueOrDefault("rollcall_id"), account_id = a, course = "行銷管理", method = "radar" });
                break;
            case "SubmitNow":
                foreach (var a in new[] { "a1", "a2" })
                    Emit(new { id = (object?)null, @event = "QuizSubmitted",
                        quiz_id = f.GetValueOrDefault("quiz_id"), account_id = a, result = "submitted (score 60)" });
                break;
        }
        return Task.FromResult(Json(new { id = 0, @event = "Reply", ok = true, error = (object?)null }));
    }

    /// One pass of the time-limited flows: a radar rollcall (detect → 15s countdown → sign each account),
    /// then an exam (prepared per-account with 1 conflict → LLM reasoning stream → 15s countdown → submit).
    private async Task RunMonitoringScript()
    {
        Emit(new { id = (object?)null, @event = "StateChanged", state = "monitoring" });
        Emit(new { id = (object?)null, @event = "AccountStatus", account_id = "a1", state = "online" });
        Emit(new { id = (object?)null, @event = "AccountStatus", account_id = "a2", state = "online" });
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
        Emit(new { id = (object?)null, @event = "QuizPrepared", quiz_id = qz, course = "行銷管理", conflict_count = 1,
            per_account = new[] {
                new { account_id = "a1", questions = new[] {
                    new { subject_id = "1", stem = "台灣最高的山是哪一座？", answer = "玉山", conflict = true },
                    new { subject_id = "2", stem = "水的化學式是？", answer = "H2O", conflict = false } } },
                new { account_id = "a2", questions = new[] {
                    new { subject_id = "1", stem = "台灣最高的山是哪一座？", answer = "玉山", conflict = false },
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
            }
        }
        EventReceived?.Invoke(el);
    }

    private static JsonElement Json(object o) => JsonSerializer.SerializeToElement(o);
}
