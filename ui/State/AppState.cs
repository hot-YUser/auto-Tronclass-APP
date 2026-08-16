using System.Collections.ObjectModel;
using System.Text;
using System.Text.Json;
using TronClass.Interop;

namespace Ui;

/// <summary>
/// UI 的唯一狀態層:訂一次 <see cref="ICore.EventReceived"/>(marshal 回 UI thread 後才改狀態,全 App 零鎖)、
/// 維護各集合/標量、提供命令薄包裝(wire 命令與欄位字串只活在這一檔)。錯誤永不吞:Reply 失敗與例外一律進 Toast+Logs。
/// </summary>
public sealed class AppState : ObservableObject
{
    readonly ICore _core;
    readonly ScheduleCoordinator _schedule;
    // Quiz 出現前的推理串流暫存:StringBuilder 線性累積,避免逐 chunk string concat 的 O(n²);
    // 轉入 ReasoningVm 時才 ToString 一次;已綁定畫面的可見更新仍全量投影,頻率由 core batching 限制。
    readonly Dictionary<(string ActivityToken, string AccountId, string SubjectId), StringBuilder> _pendingReasoning = [];
    bool _bootReady;

    public AppState(ICore core, ScheduleCoordinator schedule)
    {
        _core = core;
        _schedule = schedule;
        core.EventReceived += e => MainThread.BeginInvokeOnMainThread(() => RouteSafely(e));
        schedule.Diagnostic += message =>
            MainThread.BeginInvokeOnMainThread(() => AddLog("error", message));
    }

    public bool BootReady => _bootReady;

    readonly object _bootGate = new();
    Task? _bootTask;

    /// <summary>
    /// single-flight:並發呼叫(OnAppearing 重試、Android FGS)共享同一次 boot,不會重入;
    /// 失敗後下次呼叫自動重試。已成功 boot 過的 handle,NativeCore 不會重送 Init。
    /// </summary>
    public Task BootAsync()
    {
        lock (_bootGate) return _bootTask ??= BootCoreAsync();
    }

    async Task BootCoreAsync()
    {
        try
        {
            await _schedule.BootAsync(DataPaths.Resolve());
            _bootReady = true;
            Raise(nameof(BootReady));
        }
        catch (Exception error)
        {
            _bootReady = false;
            Raise(nameof(BootReady));
            AddLog("error", $"初始化失敗：{error.Message}");
            Toast?.Invoke("error", $"初始化失敗：{error.Message}");
        }
        finally
        {
            // 結束後清掉快取:失敗可重試;成功則 NativeCore 端已快取完成 task,再呼叫不會重送 Init。
            lock (_bootGate) _bootTask = null;
        }
    }

    public async Task ResumeScheduleAsync()
    {
        if (!_bootReady) return;
        try
        {
            await _schedule.OnResumeAsync();
            if (Monitoring?.PlatformBlock is { } block)
                await ClearPlatformLimit(block.Reason);
        }
        catch (Exception error)
        {
            AddLog("error", $"排程重算失敗：{error.Message}");
            Toast?.Invoke("error", $"排程重算失敗：{error.Message}");
        }
    }

    // ---------------- 封閉監控快照 ----------------

    MonitoringSnapshotContract? _monitoring;
    public MonitoringSnapshotContract? Monitoring
    {
        get => _monitoring;
        private set
        {
            if (!Set(ref _monitoring, value)) return;
            Raise(nameof(HasMonitoringSnapshot));
            MonitoringChanged?.Invoke();
        }
    }
    public bool HasMonitoringSnapshot => Monitoring is not null;
    public event Action? MonitoringChanged;
    public event Action? CommandStateChanged;

    NextClassVm? _nextClass;
    public NextClassVm? NextClass { get => _nextClass; private set => Set(ref _nextClass, value); }

    SettingsSnapshot? _settings;
    /// <summary>核心目前生效的設定;設定頁據此填入現值(null = 尚未收到)。</summary>
    public SettingsSnapshot? CurrentSettings { get => _settings; private set => Set(ref _settings, value); }
    public event Action? SettingsChanged;

    public CapsVm Caps { get; } = new();
    public List<SchoolVm> Schools { get; } = [];
    public string? DefaultSchoolKey { get; private set; }

    // ---------------- 集合(只在 UI thread 讀寫) ----------------

    public ObservableCollection<RollcallVm> Rollcalls { get; } = [];
    public ObservableCollection<QuizVm> Quizzes { get; } = [];
    public ObservableCollection<LogEntry> Logs { get; } = [];

    // ---------------- 片刻(一次性觸發,已在 UI thread) ----------------

    public event Action<RollcallVm>? HeroRollcall;
    public event Action<QuizVm>? HeroQuiz;
    public event Action<string, ImageSource>? CaptchaRequested; // (account_id, 圖)
    public event Action<string, string>? Toast;                 // (severity, message)
    public event Action? Ticked;

    /// <summary>頁面層的即時提示(儲存成功之類)也走同一條 Toast 通道。</summary>
    public void Notify(string severity, string message) => Toast?.Invoke(severity, message);

    // ---------------- 事件路由 ----------------

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
            case "Tick": Ticked?.Invoke(); break;
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


            // VaultState：核心以 device-key 自動解鎖（無主密碼），使用者不需介入；
            // 硬失敗會另以 Error 事件呈現，故此處不需處理。

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
                    Bool(s, "resubmit_for_correct"), Bool(s, "enable_llm_tools"), Bool(s, "has_llm_key"));
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
                    pending.Status = "pending";
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
            vm.Status = "done";
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
            case "quiz" when FindQuiz(activityToken) is { Status: "reviewing" } q:
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
            vm.Status = "done";
            vm.RemainingSecs = null;
        }
        Raise(nameof(Quizzes)); // 讓列表的題數等衍生值刷新
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
            vm.Status = "done";
            vm.RemainingSecs = null;
        }
    }

    void AddLog(string level, string text)
    {
        Logs.Insert(0, new LogEntry(DateTime.Now, level, text));
        while (Logs.Count > 200) Logs.RemoveAt(Logs.Count - 1);
    }

    void ContractError(string message)
    {
        AddLog("error", $"核心協議錯誤：{message}");
        Toast?.Invoke("error", $"核心協議錯誤：{message}");
    }

    // ---------------- 命令(UI → core) ----------------

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
            await MainThread.InvokeOnMainThreadAsync(() => { quiz.Status = "held"; quiz.RemainingSecs = null; });
    }

    public async Task DiscardAnswer(QuizVm quiz)
    {
        if (OkReply(await Send("DiscardAnswer", ("activity_token", quiz.ActivityToken))))
            await MainThread.InvokeOnMainThreadAsync(() => { quiz.Status = "discarded"; quiz.RemainingSecs = null; });
    }

    // account_id:衝突/答案是 per-account,使用者在答題詳細切到哪個帳號就定案哪個。
    public Task SetAnswer(QuizVm quiz, string accountId, string subjectId, AnswerWire answer) =>
        Send("SetAnswer", ("activity_token", quiz.ActivityToken), ("account_id", accountId),
            ("subject_id", subjectId), ("answer", answer));

    public async Task<bool> SetLlmKey(string key) => OkReply(await Send("SetLlmKey", ("key", key)));

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

    // ---------------- JSON 取值 ----------------

    static string? Str(JsonElement e, string key) =>
        e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
    static bool Bool(JsonElement e, string key) =>
        e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.True;
    static int Int(JsonElement e, string key) =>
        e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetInt32() : 0;
    static double Dbl(JsonElement e, string key) =>
        e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetDouble() : 0;
}
