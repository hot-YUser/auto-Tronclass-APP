using System.Net.Http;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using TronClass.Interop;

// Headless proof of the C# side of the slice-1 FFI: drive the full account+login flow
// (Init → CreateVault → AddAccount → Login) against the fake server across P/Invoke, for both a
// good account (→ ok) and a bad one (→ ok:false, the false-positive guard). No MAUI, CI-able.

static class Program
{
    static readonly List<string> Events = new();
    static readonly object Gate = new();

    [UnmanagedCallersOnly(CallConvs = new[] { typeof(CallConvCdecl) })]
    static unsafe void OnEvent(byte* ptr, nuint len)
    {
        var json = Encoding.UTF8.GetString(new ReadOnlySpan<byte>(ptr, (int)len));
        lock (Gate) Events.Add(json);
    }

    static List<JsonElement> Snapshot()
    {
        lock (Gate) return Events.Select(e => JsonDocument.Parse(e).RootElement.Clone()).ToList();
    }

    static JsonElement? WaitFor(Func<JsonElement, bool> pred, int seconds)
    {
        var deadline = DateTime.UtcNow.AddSeconds(seconds);
        while (DateTime.UtcNow < deadline)
        {
            var hit = Snapshot().FirstOrDefault(v => v.TryGetProperty("event", out _) && pred(v));
            if (hit.ValueKind != JsonValueKind.Undefined) return hit;
            Thread.Sleep(100);
        }
        return null;
    }

    static bool ReplyOk(int id) =>
        WaitFor(v => v.GetProperty("event").GetString() == "Reply" && v.GetProperty("id").GetInt32() == id, 10)
            is { } r && r.GetProperty("ok").GetBoolean();

    static string? AccountId(string label) =>
        Snapshot().Where(v => v.GetProperty("event").GetString() == "Accounts").Reverse()
            .SelectMany(v => v.GetProperty("accounts").EnumerateArray())
            .FirstOrDefault(a => a.GetProperty("label").GetString() == label) is { ValueKind: JsonValueKind.Object } acc
            ? acc.GetProperty("id").GetString() : null;

    static unsafe int Main(string[] args)
    {
        var baseUrl = args.Length > 0 ? args[0] : "http://127.0.0.1:8779";
        var dataDir = Path.Combine(Path.GetTempPath(), "tron-smoke-" + Guid.NewGuid().ToString("N"))
            .Replace('\\', '/');
        var handle = NativeMethods.core_init(&OnEvent);

        void Send(int id, object fields)
        {
            var dict = new Dictionary<string, object> { ["id"] = id };
            foreach (var p in fields.GetType().GetProperties()) dict[p.Name] = p.GetValue(fields)!;
            var bytes = Encoding.UTF8.GetBytes(JsonSerializer.Serialize(dict));
            fixed (byte* p = bytes) NativeMethods.core_send(handle, p, (nuint)bytes.Length);
        }

        Send(1, new { cmd = "Init", data_dir = dataDir });
        var initOk = ReplyOk(1);
        Send(2, new { cmd = "CreateVault", master_password = "pw" });
        var vaultOk = ReplyOk(2);

        Send(3, new { cmd = "AddAccount", label = "good", school = baseUrl, username = "test", password = "secret" });
        ReplyOk(3);
        var goodId = WaitFor(_ => AccountId("good") != null, 5) != null ? AccountId("good") : null;
        Send(4, new { cmd = "Login", account_id = goodId });
        var goodLogin = WaitFor(v => v.GetProperty("event").GetString() == "LoginResult" && v.GetProperty("id").GetInt32() == 4, 15);

        Send(5, new { cmd = "AddAccount", label = "bad", school = baseUrl, username = "test", password = "WRONG" });
        ReplyOk(5);
        var badId = AccountId("bad");
        Send(6, new { cmd = "Login", account_id = badId });
        var badLogin = WaitFor(v => v.GetProperty("event").GetString() == "LoginResult" && v.GetProperty("id").GetInt32() == 6, 15);

        // Slices 2+3: configure the (fake) LLM, monitor, then auto-sign a rollcall and auto-submit a quiz.
        Send(7, new { cmd = "UpdateConfig", patch = new { countdown_secs = 2, quiz_detect_secs = 1, llm_endpoint = baseUrl + "/v1/chat/completions" } });
        ReplyOk(7);
        Send(8, new { cmd = "SetLlmKey", key = "fake-key" });
        ReplyOk(8);
        Send(9, new { cmd = "StartMonitoring" });
        ReplyOk(9);
        using (var http = new HttpClient())
        {
            http.PostAsync(baseUrl + "/_test/open_rollcall",
                new StringContent("{\"id\":\"SMOKE1\",\"kind\":\"self_registration\",\"attendance_rate\":100}")).GetAwaiter().GetResult();
            http.PostAsync(baseUrl + "/_test/open_quiz",
                new StringContent("{\"activity_id\":\"SMOKEQ\",\"course_id\":\"C1\",\"subjects\":[{\"id\":\"q1\",\"type\":\"short_answer\",\"content\":\"why\"}]}")).GetAwaiter().GetResult();
        }
        var autoSigned = WaitFor(v => v.GetProperty("event").GetString() == "SignedIn" && v.GetProperty("rollcall_id").GetString() == "SMOKE1", 20) != null;
        var autoSubmitted = WaitFor(v => v.GetProperty("event").GetString() == "QuizSubmitted" && v.GetProperty("quiz_id").GetString() == "SMOKEQ", 25) != null;

        // Slice 4: patch a new tuning knob, then drive a captcha login end-to-end (challenge → submit).
        Send(10, new { cmd = "UpdateConfig", patch = new { llm_max_tokens = 12345, log_level = "debug" } });
        var configUpdated = ReplyOk(10);

        using (var http = new HttpClient())
        {
            http.PostAsync(baseUrl + "/_test/captcha",
                new StringContent("{\"required\":true,\"expected\":\"Z9Z9\"}")).GetAwaiter().GetResult();
        }
        Send(11, new { cmd = "AddAccount", label = "cap", school = baseUrl, username = "cap", password = "secret" });
        ReplyOk(11);
        var capId = AccountId("cap");
        Send(12, new { cmd = "Login", account_id = capId });
        var challenge = WaitFor(v => v.GetProperty("event").GetString() == "CaptchaChallenge", 10);
        if (challenge != null) Send(13, new { cmd = "SubmitCaptcha", account_id = capId, text = "Z9Z9" });
        var capResult = WaitFor(v => v.GetProperty("event").GetString() == "LoginResult" && v.GetProperty("id").GetInt32() == 12, 12);
        var captchaSolved = capResult?.GetProperty("ok").GetBoolean() ?? false;

        var tick = WaitFor(v => v.GetProperty("event").GetString() == "Tick", 3) != null;
        var stateChanged = Snapshot().Any(v => v.GetProperty("event").GetString() == "StateChanged");

        NativeMethods.core_free(handle);

        var good = goodLogin?.GetProperty("ok").GetBoolean() ?? false;
        var bad = badLogin?.GetProperty("ok").GetBoolean() ?? true;
        Console.WriteLine($"init={initOk} vault={vaultOk} goodLogin={good} badLogin={bad} autoSigned={autoSigned} autoSubmitted={autoSubmitted} captchaSolved={captchaSolved} configUpdated={configUpdated} tick={tick} state={stateChanged}");
        var pass = initOk && vaultOk && good && !bad && autoSigned && autoSubmitted && captchaSolved && configUpdated && tick && stateChanged;
        Console.WriteLine(pass ? "SLICE4 SMOKE PASS" : "SLICE4 SMOKE FAIL");
        return pass ? 0 : 1;
    }
}
