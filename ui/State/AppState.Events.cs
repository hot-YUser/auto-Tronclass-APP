using System.Text;
using System.Text.Json;

namespace Ui;

/// <summary>核心 → UI:把每個事件套用到狀態上。事件名與欄位名只出現在這個檔案。</summary>
public sealed partial class AppState
{
    // 測驗卡出現前先到的推理串流,等 QuizPrepared 建好題目再接上。
    readonly Dictionary<(string ActivityToken, string AccountId, string SubjectId), StringBuilder> _pendingReasoning = [];


    void RouteSafely(JsonElement e)
    {
        try
        {
            Route(e);
        }
        catch (Exception error)
        {
            ContractError($"事件格式無效：{error.Message}");
        }
    }

    void Route(JsonElement e)
    {
        if (!e.TryGetProperty("event", out var evEl)) return;
        switch (evEl.GetString())
        {
            case "MonitoringSnapshot":
                ApplyMonitoringSnapshot(MonitoringSnapshotContract.Parse(
                    WireShape.Required(e, "snapshot")));
                break;

            case "Caps" when e.TryGetProperty("caps", out var c):
                // 能力以 core 為唯一真值來源(監控能力在 core 端已是 target-aware)。
                Caps.BackgroundMonitoring = Bool(c, "background_monitoring");
                Caps.SelfUpdate = Bool(c, "self_update");
                Caps.QrTeacherAssist = Bool(c, "qr_teacher_assist");
                Caps.OcrCaptcha = Bool(c, "ocr_captcha");
                break;

            case "Providers":
                Schools.Clear();
                DefaultSchoolKey = Str(e, "default_key");
                if (e.TryGetProperty("schools", out var schools))
                    foreach (var s in schools.EnumerateArray())
                        Schools.Add(new SchoolVm(Str(s, "key") ?? "", Str(s, "label") ?? "", Str(s, "base_url") ?? ""));
                break;

            case "CaptchaChallenge": OnCaptcha(e); break;

            case "NextClass":
                // course 為 null(或欄位缺)⇒ 無下一堂課 → 卡片隱藏
                NextClass = Str(e, "course") is { Length: > 0 } course
                    ? new NextClassVm(Str(e, "account_id") ?? "", course,
                        DateTimeOffset.TryParse(Str(e, "start_time"), out var st) ? st : DateTimeOffset.Now,
                        Str(e, "location") ?? "")
                    : null;
                break;

            case "Settings" when e.TryGetProperty("settings", out var s):
                CurrentSettings = new SettingsSnapshot(
                    Int(s, "countdown_secs"), Dbl(s, "attendance_gate_percent"),
                    Str(s, "llm_endpoint") ?? "", Str(s, "llm_model") ?? "", Int(s, "llm_max_tokens"),
                    Bool(s, "resubmit_for_correct"), Bool(s, "enable_llm_tools"), Bool(s, "has_llm_key"),
                    Bool(s, "has_qr_remote_key"), Bool(s, "qr_remote_enabled"), Str(s, "qr_remote_base_url") ?? "https://api.hlp.qzz.io");
                SettingsChanged?.Invoke();
                break;

            case "LogLine": AddLog(Str(e, "level") ?? "info", Str(e, "text") ?? ""); break;

            case "Error":
            {
                var sev = Str(e, "severity") ?? "error";
                var msg = Str(e, "message") ?? "發生未知錯誤";
                AddLog(sev, msg);
                Toast?.Invoke(sev, msg);
                break;
            }

            case "RollcallDetected": OnRollcallDetected(e); break;
            // 門檻狀態:未達門檻沒有倒數,UI 改用即時簽到率填那個欄位(達標後 holding=false 讓回倒數)。
            case "RollcallGate":
                if (FindRollcall(Str(e, "activity_token")) is { } gated)
                {
                    if (e.TryGetProperty("rate", out var gr) && gr.ValueKind == JsonValueKind.Number)
                        gated.AttendanceRate = gr.GetDouble();
                    gated.GatePercent = Dbl(e, "gate_percent");
                    gated.Holding = Bool(e, "holding");
                    if (gated.Holding) gated.RemainingSecs = null; // 還沒開始倒數
                }
                break;
            case "PendingSignIn":
                if (FindRollcall(Str(e, "activity_token")) is { } pending)
                {
                    pending.Status = RollcallStatus.Pending;
                    pending.RemainingSecs = null;
                }
                break;
            case "SignedIn": OnSignedIn(e); break;
            case "Countdown": OnCountdown(e); break;

            case "QuizPrepared": OnQuizPrepared(e); break;
            case "ReasoningChunk": OnReasoningChunk(e); break;
            case "AnswerUpdated": OnAnswerUpdated(e); break;
            case "QuizSubmitted": OnQuizSubmitted(e); break;
        }
    }

    void ApplyMonitoringSnapshot(MonitoringSnapshotContract snapshot)
    {
        Monitoring = snapshot;
        if (snapshot.SessionState == "idle" &&
            snapshot.Targets.All(target => target.RuntimeState is "scheduled_off" or "manual_off" or "suppressed_by_group"))
            MonitoringServiceLifetime.Stop();
    }

    void OnCaptcha(JsonElement e)
    {
        var id = Str(e, "account_id") ?? "";
        try
        {
            var bytes = Convert.FromBase64String(Str(e, "image_b64") ?? "");
            CaptchaRequested?.Invoke(id, ImageSource.FromStream(() => new MemoryStream(bytes)));
        }
        catch (FormatException)
        {
            Toast?.Invoke("error", "驗證碼圖片格式錯誤");
        }
    }

    RollcallVm? FindRollcall(string? activityToken) => Rollcalls.FirstOrDefault(r => r.ActivityToken == activityToken);
    QuizVm? FindQuiz(string? activityToken) => Quizzes.FirstOrDefault(q => q.ActivityToken == activityToken);
    public string AccountLabel(string id) =>
        Monitoring?.Accounts.FirstOrDefault(account => account.AccountId == id)?.Label ?? id;

    void OnRollcallDetected(JsonElement e)
    {
        var activityToken = Str(e, "activity_token");
        if (string.IsNullOrWhiteSpace(activityToken))
        {
            ContractError("RollcallDetected 缺少 activity_token");
            return;
        }
        var id = Str(e, "rollcall_id") ?? "";
        var baseUrl = Str(e, "base_url") ?? "";
        // Core 的 opaque token 是活動實例唯一鍵；外部 ID 只供顯示，不能拿來路由命令。
        var vm = FindRollcall(activityToken);
        var announce = vm is null;
        if (vm is null)
            Rollcalls.Insert(0, vm = new RollcallVm { ActivityToken = activityToken, Id = id, BaseUrl = baseUrl });
        vm.Kind = Str(e, "kind") ?? vm.Kind;
        vm.Course = Str(e, "course") ?? vm.Course;
        // 首次偵測時 core 還沒查到簽到率(null)——別用 0 蓋掉 RollcallGate 已推來的活值。
        if (e.TryGetProperty("attendance_rate", out var ar) && ar.ValueKind == JsonValueKind.Number)
            vm.AttendanceRate = ar.GetDouble();
        if (e.TryGetProperty("accounts", out var arr))
            foreach (var a in arr.EnumerateArray())
            {
                var accId = a.GetString() ?? "";
                if (vm.Accounts.All(x => x.AccountId != accId))
                    vm.Accounts.Add(new RollcallAccountVm { AccountId = accId, Label = AccountLabel(accId) });
            }
        vm.RaiseProgress();
        if (announce) HeroRollcall?.Invoke(vm);
    }

    void OnSignedIn(JsonElement e)
    {
        var vm = FindRollcall(Str(e, "activity_token"));
        if (vm is null) return;
        var accId = Str(e, "account_id") ?? "";
        var part = vm.Accounts.FirstOrDefault(x => x.AccountId == accId);
        if (part is null) vm.Accounts.Add(part = new RollcallAccountVm { AccountId = accId, Label = AccountLabel(accId) });
        part.Method = Str(e, "method");
        part.Signed = true;
        vm.RaiseProgress();
        if (vm.Accounts.All(x => x.Signed))
        {
            vm.Status = RollcallStatus.Done;
            vm.RemainingSecs = null;
        }
    }

    void OnCountdown(JsonElement e)
    {
        var activityToken = Str(e, "activity_token");
        var secs = Int(e, "remaining_secs");
        // Hold/Defer/送出後 core 會停止倒數;此時若仍收到 Countdown(Mock 的計時迴圈不理會 Hold)一律忽略,
        // 否則會把使用者的暫緩/暫緩決定翻掉、繼續自動送。只在「進行中」狀態才渲染倒數。
        switch (Str(e, "scope"))
        {
            case "rollcall" when FindRollcall(activityToken) is { IsCounting: true } r:
                if (secs > r.TotalSecs) r.TotalSecs = secs; // 首發(最大值)當總長
                r.RemainingSecs = secs;
                break;
            case "quiz" when FindQuiz(activityToken) is { Status: QuizStatus.Reviewing } q:
                if (secs > q.TotalSecs) q.TotalSecs = secs;
                q.RemainingSecs = secs;
                break;
        }
    }

    void OnQuizPrepared(JsonElement e)
    {
        if (!QuizPreparedContract.TryParse(e, out var prepared, out var contractError))
        {
            ContractError(contractError);
            return;
        }
        var activityToken = prepared.ActivityToken;
        var id = prepared.QuizId;
        var vm = FindQuiz(activityToken);
        var announce = vm is null;
        if (vm is null) Quizzes.Insert(0, vm = new QuizVm { ActivityToken = activityToken, Id = id });
        vm.Course = prepared.Course;
        vm.ExpectedAccountIds.Clear();
        foreach (var expected in prepared.ExpectedAccounts)
        {
            vm.ExpectedAccountIds.Add(expected.AccountId);
            var expectedVm = vm.PerAccount.FirstOrDefault(account => account.AccountId == expected.AccountId);
            if (expectedVm is null)
                vm.PerAccount.Add(expectedVm = new QuizAccountVm { AccountId = expected.AccountId });
            expectedVm.Label = AccountLabel(expected.AccountId);
            expectedVm.AttemptState = expected.State;
        }
        // conflict_count 只作參考;送出閘門由逐題 QuestionVm.Conflict 推導(見 QuizVm.AnyConflict)
        foreach (var a in prepared.Accounts)
        {
            var accId = a.AccountId;
            var accVm = vm.PerAccount.FirstOrDefault(x => x.AccountId == accId);
            if (accVm is null) vm.PerAccount.Add(accVm = new QuizAccountVm { AccountId = accId });
            accVm.Label = AccountLabel(accId);
            accVm.AttemptState = a.State;
            accVm.Questions.Clear(); // 重備答=以新題面為準;SubmitResult 保留
            foreach (var q in a.Questions)
            {
                var reasoningKey = (accId, q.SubjectId);
                if (!vm.Reasoning.TryGetValue(reasoningKey, out var reasoning))
                    vm.Reasoning[reasoningKey] = reasoning = new ReasoningVm();
                accVm.Questions.Add(new QuestionVm
                {
                    SubjectId = q.SubjectId,
                    Stem = q.Stem,
                    QuestionType = q.Type,
                    AnswerType = q.AnswerType,
                    Options = q.Options,
                    AnswerPayload = q.Answer,
                    Conflict = q.Conflict,
                    Source = q.Source,
                    Reasoning = reasoning,
                });
            }
        }
        foreach (var gone in vm.PerAccount.Where(account => !vm.ExpectedAccountIds.Contains(account.AccountId)).ToList())
            vm.PerAccount.Remove(gone);
        foreach (var pending in _pendingReasoning.Where(item => item.Key.ActivityToken == activityToken).ToList())
        {
            var reasoningKey = (pending.Key.AccountId, pending.Key.SubjectId);
            if (!vm.Reasoning.TryGetValue(reasoningKey, out var reasoning))
                vm.Reasoning[reasoningKey] = reasoning = new ReasoningVm();
            reasoning.Append(pending.Value.ToString());
            _pendingReasoning.Remove(pending.Key);
        }
        vm.RaiseProgress();
        vm.RaiseConflictState();   // 依剛建好的逐題旗標刷新閘門/警示
        // 全部預期帳號已終端(準備失敗/活動已結束)時,這份測驗沒有可送出的內容,直接定案為完成——
        // Hero 關閉與狀態文字都依同一述詞,不假裝還在審題。
        if (vm.IsComplete)
        {
            vm.Status = QuizStatus.Done;
            vm.RemainingSecs = null;
        }
        if (announce) HeroQuiz?.Invoke(vm);
    }

    void OnReasoningChunk(JsonElement e)
    {
        var activityToken = Str(e, "activity_token") ?? "";
        var accountId = Str(e, "account_id") ?? "";
        var subjectId = Str(e, "subject_id") ?? "";
        if (string.IsNullOrWhiteSpace(activityToken) || string.IsNullOrWhiteSpace(accountId) || string.IsNullOrWhiteSpace(subjectId))
        {
            ContractError("ReasoningChunk 缺少 activity_token、account_id 或 subject_id");
            return;
        }
        if (FindQuiz(activityToken) is not { } vm)
        {
            var key = (activityToken, accountId, subjectId);
            var builder = _pendingReasoning.GetValueOrDefault(key);
            if (builder is null) _pendingReasoning[key] = builder = new StringBuilder();
            builder.Append(Str(e, "text") ?? "");
            return;
        }
        var reasoningKey = (accountId, subjectId);
        if (!vm.Reasoning.TryGetValue(reasoningKey, out var reasoning))
            vm.Reasoning[reasoningKey] = reasoning = new ReasoningVm();
        reasoning.Append(Str(e, "text") ?? "");
    }

    void OnAnswerUpdated(JsonElement e)
    {
        if (FindQuiz(Str(e, "activity_token")) is not { } vm) return;
        var accId = Str(e, "account_id");
        var subjectId = Str(e, "subject_id");
        var q = vm.PerAccount.FirstOrDefault(a => a.AccountId == accId)?
                  .Questions.FirstOrDefault(x => x.SubjectId == subjectId);
        if (q is null) return;
        if (!e.TryGetProperty("answer", out var answerElement) || AnswerWire.FromJson(answerElement) is not { } answer)
        {
            ContractError("AnswerUpdated 缺少合法型別答案");
            return;
        }
        q.AnswerPayload = answer;
        q.Source = Str(e, "source") ?? q.Source;
        q.Conflict = Bool(e, "conflict");
        // 閘門/警示由逐題旗標推導,conflict 兩個方向(清除或新增)都在此一次刷新
        vm.RaiseConflictState();
    }

    void OnQuizSubmitted(JsonElement e)
    {
        if (FindQuiz(Str(e, "activity_token")) is not { } vm) return;
        var accVm = vm.PerAccount.FirstOrDefault(a => a.AccountId == Str(e, "account_id"));
        if (accVm is null) return;
        accVm.SubmitResult = Str(e, "result") ?? "已送出";
        vm.RaiseProgress();
        // 完成與否由共用述詞決定:全部預期帳號都已送出、準備失敗或活動已結束(見 QuizCompletion)。
        if (vm.IsComplete)
        {
            vm.Status = QuizStatus.Done;
            vm.RemainingSecs = null;
        }
    }


    static string? Str(JsonElement e, string key) =>
        e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
    static bool Bool(JsonElement e, string key) =>
        e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.True;
    static int Int(JsonElement e, string key) =>
        e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetInt32() : 0;
    static double Dbl(JsonElement e, string key) =>
        e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetDouble() : 0;
}
