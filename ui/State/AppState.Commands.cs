using System.Text.Json;

namespace Ui;

/// <summary>UI → 核心:每個命令一個薄包裝。命令名與欄位名只出現在這個檔案。</summary>
public sealed partial class AppState
{

    readonly object _commandGate = new();
    readonly HashSet<string> _pendingCommands = new(StringComparer.Ordinal);

    public bool IsCommandPending(string key)
    {
        lock (_commandGate) return _pendingCommands.Contains(key);
    }

    bool BeginCommand(string key)
    {
        lock (_commandGate)
        {
            if (!_pendingCommands.Add(key)) return false;
        }
        NotifyCommandStateChanged();
        return true;
    }

    void EndCommand(string key)
    {
        lock (_commandGate) _pendingCommands.Remove(key);
        NotifyCommandStateChanged();
    }

    void NotifyCommandStateChanged()
    {
        if (MainThread.IsMainThread) CommandStateChanged?.Invoke();
        else MainThread.BeginInvokeOnMainThread(() => CommandStateChanged?.Invoke());
    }

    async Task<JsonElement?> SendKeyed(
        string key,
        string cmd,
        params (string Key, object? Value)[] fields)
    {
        if (!BeginCommand(key)) return null;
        try
        {
            return await Send(cmd, fields);
        }
        finally
        {
            EndCommand(key);
        }
    }

    public async Task<string?> AddAndVerifyAccount(
        string label,
        string school,
        string username,
        string password,
        bool isTeacher = false,
        string? courseId = null)
    {
        var reply = await SendKeyed(
            "account:add",
            "AddAccount",
            ("label", label),
            ("school", school),
            ("username", username),
            ("password", password),
            ("is_teacher", isTeacher),
            ("course_id", string.IsNullOrWhiteSpace(courseId) ? null : courseId.Trim()));
        if (!OkReply(reply)) return null;
        try
        {
            var accountId = WireShape.RequiredString(
                WireShape.Required(reply!.Value, "data"),
                "account_id");
            await Login(accountId);
            return accountId;
        }
        catch (FormatException error)
        {
            ContractError($"AddAccount reply 無效：{error.Message}");
            return null;
        }
    }

    public async Task<bool> Login(string accountId) =>
        OkReply(await SendKeyed(
            $"account:{accountId}:auth",
            "Login",
            ("account_id", accountId)));

    public async Task<bool> ImportCookies(string accountId, string cookiesJson) =>
        OkReply(await SendKeyed(
            $"account:{accountId}:auth",
            "ImportCookies",
            ("account_id", accountId),
            ("cookies_json", cookiesJson)));

    public async Task<bool> DeleteAccount(
        string accountId,
        ulong expectedRevision,
        bool removeFromGroups) =>
        OkReply(await SendKeyed(
            $"account:{accountId}:delete",
            "DeleteAccount",
            ("account_id", accountId),
            ("expected_revision", expectedRevision),
            ("remove_from_groups", removeFromGroups)));

    public Task SubmitCaptcha(string id, string text) =>
        Send("SubmitCaptcha", ("account_id", id), ("text", text));

    public async Task<CourseContract[]?> ListCommonCourses(string[] memberAccountIds)
    {
        var key = $"courses:{string.Join(',', memberAccountIds.Order(StringComparer.Ordinal))}";
        var reply = await SendKeyed(
            key,
            "ListCommonCourses",
            ("member_account_ids", memberAccountIds));
        if (!OkReply(reply)) return null;
        try
        {
            var courses = WireShape.Required(
                WireShape.Required(reply!.Value, "data"),
                "courses");
            if (courses.ValueKind != JsonValueKind.Array)
                throw new FormatException("courses 必須是陣列。");
            return courses.EnumerateArray().Select(CourseContract.FromJson).ToArray();
        }
        catch (FormatException error)
        {
            ContractError($"ListCommonCourses reply 無效：{error.Message}");
            return null;
        }
    }

    public Task<bool> CreateGroup(ulong expectedRevision, GroupInputWire group) =>
        SendDefinitionCommand(
            "group:create",
            "CreateGroup",
            ("expected_revision", expectedRevision),
            ("group", group));

    public Task<bool> UpdateGroup(string groupId, ulong expectedRevision, GroupInputWire group) =>
        SendDefinitionCommand(
            $"group:{groupId}:update",
            "UpdateGroup",
            ("group_id", groupId),
            ("expected_revision", expectedRevision),
            ("group", group));

    public Task<bool> DeleteGroup(string groupId, ulong expectedRevision) =>
        SendDefinitionCommand(
            $"group:{groupId}:delete",
            "DeleteGroup",
            ("group_id", groupId),
            ("expected_revision", expectedRevision));

    public Task<bool> MergeGroups(
        string[] groupIds,
        ulong expectedRevision,
        GroupInputWire group) =>
        SendDefinitionCommand(
            $"group:merge:{string.Join(',', groupIds.Order(StringComparer.Ordinal))}",
            "MergeGroups",
            ("group_ids", groupIds),
            ("expected_revision", expectedRevision),
            ("group", group));

    public Task<bool> SetTargetSchedule(
        TargetIdSpec target,
        ulong expectedRevision,
        ScheduleBindingSpec schedule) =>
        SendDefinitionCommand(
            $"{TargetCommandKey(target)}:schedule",
            "SetTargetSchedule",
            ("target", target),
            ("expected_revision", expectedRevision),
            ("schedule", schedule));

    public Task<bool> SaveMonitoringPreferences(
        ulong expectedRevision,
        WeeklyScheduleSpec globalSchedule,
        TimeZoneSpec timeZone) =>
        SendDefinitionCommand(
            "monitoring:preferences",
            "SetMonitoringPreferences",
            ("expected_revision", expectedRevision),
            ("global_schedule", globalSchedule),
            ("time_zone", timeZone));

    async Task<bool> SendDefinitionCommand(
        string key,
        string command,
        params (string Key, object? Value)[] fields)
    {
        if (!OkReply(await SendKeyed(key, command, fields))) return false;
        try
        {
            await _schedule.OnResumeAsync();
        }
        catch (Exception error)
        {
            AddLog("error", $"定義已儲存，但排程時鐘更新失敗：{error.Message}");
            Toast?.Invoke("error", "設定已儲存；排程暫停，請重新開啟 App。");
        }
        return true;
    }

    public static string TargetCommandKey(TargetIdSpec target) => $"target:{target.Kind}:{target.Id}";

    public async Task<bool> StartTarget(TargetIdSpec target)
    {
        if (!_bootReady || !TryStartMonitoringService()) return false;
        return OkReply(await SendKeyed(
            TargetCommandKey(target),
            "StartTarget",
            ("target", target)));
    }

    public async Task<bool> StopTarget(TargetIdSpec target) =>
        OkReply(await SendKeyed(
            TargetCommandKey(target),
            "StopTarget",
            ("target", target)));

    public async Task<bool> StopAllMonitoring()
    {
        var ok = OkReply(await SendKeyed("monitoring:all", "StopAllMonitoring"));
        if (ok) MonitoringServiceLifetime.Stop();
        return ok;
    }

    public async Task<bool> ResumeScheduledMonitoring()
    {
        if (!OkReply(await SendKeyed("monitoring:all", "ResumeScheduledMonitoring")))
            return false;
        await _schedule.OnResumeAsync();
        return TryStartMonitoringService();
    }

    public async Task<bool> AcknowledgeTemporaryMerge(string componentId, ulong planRevision) =>
        OkReply(await SendKeyed(
            $"merge:{componentId}",
            "AcknowledgeTemporaryMerge",
            ("component_id", componentId),
            ("plan_revision", planRevision)));

    public async Task<bool> SuspendForPlatformLimit(string reason) =>
        OkReply(await SendKeyed(
            "platform:limit",
            "SuspendForPlatformLimit",
            ("reason", reason)));

    public async Task<bool> ClearPlatformLimit(string reason) =>
        OkReply(await SendKeyed(
            "platform:limit",
            "ClearPlatformLimit",
            ("reason", reason)));

    bool TryStartMonitoringService()
    {
        try
        {
            MonitoringServiceLifetime.Start();
            return true;
        }
        catch (Exception error)
        {
            AddLog("error", $"無法啟動背景監控服務：{error.Message}");
            Toast?.Invoke("error", $"無法啟動背景監控服務：{error.Message}");
            return false;
        }
    }

    public Task SignNow(RollcallVm rollcall) => Send("SignNow", ("activity_token", rollcall.ActivityToken));
    public Task DeferSignIn(RollcallVm rollcall) => Send("DeferSignIn", ("activity_token", rollcall.ActivityToken));

    public Task SubmitNow(QuizVm quiz) => Send("SubmitNow", ("activity_token", quiz.ActivityToken));

    public async Task HoldAnswer(QuizVm quiz)
    {
        // 契約無獨立 Held 事件;命令由 id 對應的 Reply 完成(20-contract 信封),故 Reply ok 即「已停自動送」的確認,
        // 本地標記 held 契約上成立。若真核心其實沒停而仍自動送出,後續 QuizSubmitted → OnQuizSubmitted 會把 UI 校正為 done;
        // 故不另建影子確認機制(UI 一收到任何後續事件就回到真相)。
        if (OkReply(await Send("HoldAnswer", ("activity_token", quiz.ActivityToken))))
            await MainThread.InvokeOnMainThreadAsync(() => { quiz.Status = QuizStatus.Held; quiz.RemainingSecs = null; });
    }

    public async Task DiscardAnswer(QuizVm quiz)
    {
        if (OkReply(await Send("DiscardAnswer", ("activity_token", quiz.ActivityToken))))
            await MainThread.InvokeOnMainThreadAsync(() => { quiz.Status = QuizStatus.Discarded; quiz.RemainingSecs = null; });
    }

    // account_id:衝突/答案是 per-account,使用者在答題詳細切到哪個帳號就定案哪個。
    public Task SetAnswer(QuizVm quiz, string accountId, string subjectId, AnswerWire answer) =>
        Send("SetAnswer", ("activity_token", quiz.ActivityToken), ("account_id", accountId),
            ("subject_id", subjectId), ("answer", answer));

    public async Task<bool> SetLlmKey(string key) => OkReply(await Send("SetLlmKey", ("key", key)));
    public async Task<bool> SetQrRemoteKey(string key) => OkReply(await Send("SetQrRemoteKey", ("key", key)));

    public async Task<bool> SaveQrRemoteConfig(bool enabled, string baseUrl) =>
        OkReply(await Send("UpdateConfig", ("patch", new Dictionary<string, object?>
        {
            ["qr_remote_enabled"] = enabled,
            ["qr_remote_base_url"] = baseUrl,
        })));

    public async Task<bool> SaveConfig(int countdownSecs, double thresholdPct, bool thresholdEnabled) =>
        // 鍵名對齊 core Settings(config.rs)。core 只有單一 attendance_gate_percent:停用門檻 = 送 0%
        // (全班簽到率永遠 ≥ 0 → 門檻永遠通過),不需 core 端另加 enabled 欄位。
        OkReply(await Send("UpdateConfig", ("patch", new Dictionary<string, object?>
        {
            ["countdown_secs"] = countdownSecs,
            ["attendance_gate_percent"] = thresholdEnabled ? thresholdPct : 0.0,
        })));

    /// <summary>LLM 連線與答題偏好(端點/模型/max_tokens/更正/教材工具)。金鑰另走 SetLlmKey(保險庫)。</summary>
    public async Task<bool> SaveLlmSettings(string endpoint, string model, int maxTokens, bool resubmit, bool tools) =>
        OkReply(await Send("UpdateConfig", ("patch", new Dictionary<string, object?>
        {
            ["llm_endpoint"] = endpoint,
            ["llm_model"] = model,
            ["llm_max_tokens"] = maxTokens,
            ["resubmit_for_correct"] = resubmit,
            ["enable_llm_tools"] = tools,
        })));

    /// <summary>統一送命令；失敗與例外一律 Toast+Logs，reply 內最新 snapshot 也立即成為 UI authority。</summary>
    async Task<JsonElement?> Send(string cmd, params (string Key, object? Value)[] fields)
    {
        try
        {
            var reply = await _core.SendAsync(cmd, fields);
            if (reply.TryGetProperty("data", out var data) &&
                data.ValueKind == JsonValueKind.Object &&
                data.TryGetProperty("snapshot", out var snapshotElement))
            {
                var snapshot = MonitoringSnapshotContract.Parse(snapshotElement);
                await MainThread.InvokeOnMainThreadAsync(() => ApplyMonitoringSnapshot(snapshot));
            }
            if (!OkReply(reply))
            {
                var err = Str(reply, "error") ?? Str(reply, "reason") ?? Str(reply, "detail") ?? "操作失敗";
                MainThread.BeginInvokeOnMainThread(() =>
                {
                    AddLog("error", $"{cmd}:{err}");
                    Toast?.Invoke("error", err);
                });
            }
            return reply;
        }
        catch (Exception ex)
        {
            MainThread.BeginInvokeOnMainThread(() =>
            {
                AddLog("error", $"{cmd} 失敗:{ex.Message}");
                Toast?.Invoke("error", $"{cmd} 失敗:{ex.Message}");
            });
            return null;
        }
    }

    static bool OkReply(JsonElement? r) =>
        r is { ValueKind: JsonValueKind.Object } el &&
        el.TryGetProperty("ok", out var ok) && ok.ValueKind == JsonValueKind.True;
}
