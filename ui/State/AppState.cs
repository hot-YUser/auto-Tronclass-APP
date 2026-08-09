using System.Collections.ObjectModel;
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
    readonly Dictionary<(string ActivityToken, string SubjectId), string> _pendingReasoning = [];
    bool _bootReady;

    public AppState(ICore core)
    {
        _core = core;
        core.EventReceived += e => MainThread.BeginInvokeOnMainThread(() => Route(e));
    }

    public async Task BootAsync()
    {
        try
        {
            await _core.BootAsync(DataPaths.Resolve());
            _bootReady = true;
            Raise(nameof(CanToggleMonitoring));
        }
        catch (Exception error)
        {
            _bootReady = false;
            MonitoringServiceLifetime.Stop();
            MonitorState = "idle";
            AddLog("error", $"初始化失敗：{error.Message}");
            Toast?.Invoke("error", $"初始化失敗：{error.Message}");
        }
    }

    // ---------------- 標量 ----------------

    string _monitorState = "starting";
    public string MonitorState
    {
        get => _monitorState;
        private set
        {
            if (Set(ref _monitorState, value))
            {
                Raise(nameof(IsMonitoring));
                Raise(nameof(MonitorStateText));
                Raise(nameof(CanToggleMonitoring));
            }
        }
    }
    public bool IsMonitoring => MonitorState == "monitoring";
    public bool CanToggleMonitoring => _bootReady && MonitorState is not ("starting" or "stopping");
    public string MonitorStateText => MonitorState switch
    {
        "monitoring" => "監控中",
        "starting" => "啟動中",
        "stopping" => "停止中",
        "login_failed" => "登入失敗",
        "offline" => "離線",
        _ => "閒置",
    };

    string? _activeAccountId;
    public string? ActiveAccountId { get => _activeAccountId; private set => Set(ref _activeAccountId, value); }

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

    public ObservableCollection<AccountVm> Accounts { get; } = [];
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

    void Route(JsonElement e)
    {
        if (!e.TryGetProperty("event", out var evEl)) return;
        switch (evEl.GetString())
        {
            case "Tick": Ticked?.Invoke(); break;
            case "StateChanged":
                MonitorState = Str(e, "state") ?? MonitorState;
                if (MonitorState == "idle") MonitoringServiceLifetime.Stop();
                break;

            case "Caps" when e.TryGetProperty("caps", out var c):
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

            case "Accounts": OnAccounts(e); break;

            case "AccountStatus":
                if (Accounts.FirstOrDefault(a => a.Id == Str(e, "account_id")) is { } acct)
                {
                    acct.State = Str(e, "state") ?? acct.State;
                    acct.Error = Str(e, "error");
                }
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

    void OnAccounts(JsonElement e)
    {
        ActiveAccountId = Str(e, "active");
        var seen = new HashSet<string>();
        if (e.TryGetProperty("accounts", out var arr))
            foreach (var a in arr.EnumerateArray())
            {
                var id = Str(a, "id") ?? "";
                seen.Add(id);
                var vm = Accounts.FirstOrDefault(x => x.Id == id);
                if (vm is null) Accounts.Add(vm = new AccountVm { Id = id });
                vm.Label = Str(a, "label") ?? "";
                vm.Username = Str(a, "username") ?? "";
                vm.SchoolRef = Str(a, "school_ref") ?? "";
                vm.IsTeacher = Bool(a, "is_teacher");
                vm.CourseId = Str(a, "course_id");
                vm.IsActive = id == ActiveAccountId;
            }
        foreach (var gone in Accounts.Where(x => !seen.Contains(x.Id)).ToList()) Accounts.Remove(gone);
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
    string AccountLabel(string id) => Accounts.FirstOrDefault(a => a.Id == id)?.Label ?? id;

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
        // conflict_count 只作參考;送出閘門由逐題 QuestionVm.Conflict 推導(見 QuizVm.AnyConflict)
        var seenAccounts = new HashSet<string>();
        foreach (var a in prepared.Accounts)
        {
            var accId = a.AccountId;
            seenAccounts.Add(accId);
            var accVm = vm.PerAccount.FirstOrDefault(x => x.AccountId == accId);
            if (accVm is null) vm.PerAccount.Add(accVm = new QuizAccountVm { AccountId = accId });
            accVm.Label = AccountLabel(accId);
            accVm.Questions.Clear(); // 重備答=以新題面為準;SubmitResult 保留
            foreach (var q in a.Questions)
            {
                if (!vm.Reasoning.TryGetValue(q.SubjectId, out var reasoning))
                    vm.Reasoning[q.SubjectId] = reasoning = new ReasoningVm();
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
        foreach (var gone in vm.PerAccount.Where(account => !seenAccounts.Contains(account.AccountId)).ToList())
            vm.PerAccount.Remove(gone);
        foreach (var pending in _pendingReasoning.Where(item => item.Key.ActivityToken == activityToken).ToList())
        {
            if (!vm.Reasoning.TryGetValue(pending.Key.SubjectId, out var reasoning))
                vm.Reasoning[pending.Key.SubjectId] = reasoning = new ReasoningVm();
            reasoning.Append(pending.Value);
            _pendingReasoning.Remove(pending.Key);
        }
        vm.RaiseProgress();
        vm.RaiseConflictState();   // 依剛建好的逐題旗標刷新閘門/警示
        Raise(nameof(Quizzes)); // 讓列表的題數等衍生值刷新
        if (announce) HeroQuiz?.Invoke(vm);
    }

    void OnReasoningChunk(JsonElement e)
    {
        var activityToken = Str(e, "activity_token") ?? "";
        var subjectId = Str(e, "subject_id") ?? "";
        if (FindQuiz(activityToken) is not { } vm)
        {
            var key = (activityToken, subjectId);
            _pendingReasoning[key] = _pendingReasoning.GetValueOrDefault(key) + (Str(e, "text") ?? "");
            return;
        }
        if (!vm.Reasoning.TryGetValue(subjectId, out var reasoning))
            vm.Reasoning[subjectId] = reasoning = new ReasoningVm();
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
        if (vm.PerAccount.All(a => a.Submitted))
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

    public async Task StartMonitoring()
    {
        if (!_bootReady || MonitorState is "monitoring" or "starting" or "stopping") return;
        MonitorState = "starting";
        // Android 12+ 只允許在使用者前景互動時啟動多數 FGS；按鈕呼叫路徑就是授權時點。
        try
        {
            MonitoringServiceLifetime.Start();
        }
        catch (Exception error)
        {
            MonitorState = "idle";
            AddLog("error", $"無法啟動背景監控服務：{error.Message}");
            Toast?.Invoke("error", $"無法啟動背景監控服務：{error.Message}");
            return;
        }
        if (!OkReply(await Send("StartMonitoring")))
        {
            MonitoringServiceLifetime.Stop();
            if (MonitorState == "starting") MonitorState = "idle";
        }
    }

    public async Task StopMonitoring()
    {
        if (MonitorState != "monitoring") return;
        MonitorState = "stopping";
        if (OkReply(await Send("StopMonitoring"))) MonitoringServiceLifetime.Stop();
        else if (MonitorState == "stopping") MonitorState = "monitoring";
    }

    public async Task<bool> AddAccount(string label, string school, string username, string password,
                                       bool isTeacher = false, string? courseId = null) =>
        OkReply(await Send("AddAccount", ("label", label), ("school", school), ("username", username), ("password", password),
                           ("is_teacher", isTeacher), ("course_id", string.IsNullOrWhiteSpace(courseId) ? null : courseId.Trim())));
    public Task SwitchAccount(string id) => Send("SwitchAccount", ("account_id", id));
    public Task DeleteAccount(string id) => Send("DeleteAccount", ("account_id", id));
    public Task Login(string id) => Send("Login", ("account_id", id));
    public async Task<bool> ImportCookies(string id, string cookiesJson) =>
        OkReply(await Send("ImportCookies", ("account_id", id), ("cookies_json", cookiesJson)));
    public Task SubmitCaptcha(string id, string text) => Send("SubmitCaptcha", ("account_id", id), ("text", text));

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

    /// <summary>統一送命令:Reply 失敗與例外一律 Toast+Logs(錯誤永不吞)。回 null 表示丟例外。</summary>
    async Task<JsonElement?> Send(string cmd, params (string Key, object? Value)[] fields)
    {
        try
        {
            var reply = await _core.SendAsync(cmd, fields);
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
        r is { } el && !(el.TryGetProperty("ok", out var ok) && ok.ValueKind == JsonValueKind.False);

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
