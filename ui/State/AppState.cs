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
    readonly Dictionary<string, string> _pendingAnswers = []; // "quiz|subject" → 剛送出的 SetAnswer 值(等 AnswerUpdated 才套用)

    public AppState(ICore core)
    {
        _core = core;
        core.EventReceived += e => MainThread.BeginInvokeOnMainThread(() => Route(e));
    }

    public Task BootAsync() => _core.BootAsync(FileSystem.AppDataDirectory);

    // ---------------- 標量 ----------------

    string _monitorState = "starting";
    public string MonitorState
    {
        get => _monitorState;
        private set { if (Set(ref _monitorState, value)) { Raise(nameof(IsMonitoring)); Raise(nameof(MonitorStateText)); } }
    }
    public bool IsMonitoring => MonitorState == "monitoring";
    public string MonitorStateText => MonitorState switch
    {
        "monitoring" => "監控中",
        "starting" => "啟動中",
        "login_failed" => "登入失敗",
        "offline" => "離線",
        _ => "閒置",
    };

    string? _activeAccountId;
    public string? ActiveAccountId { get => _activeAccountId; private set => Set(ref _activeAccountId, value); }

    NextClassVm? _nextClass;
    public NextClassVm? NextClass { get => _nextClass; private set => Set(ref _nextClass, value); }

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
            case "StateChanged": MonitorState = Str(e, "state") ?? MonitorState; break;

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
            case "PendingSignIn":
                if (FindRollcall(Str(e, "rollcall_id")) is { } pending)
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

    RollcallVm? FindRollcall(string? id) => Rollcalls.FirstOrDefault(r => r.Id == id);
    QuizVm? FindQuiz(string? id) => Quizzes.FirstOrDefault(q => q.Id == id);
    string AccountLabel(string id) => Accounts.FirstOrDefault(a => a.Id == id)?.Label ?? id;

    void OnRollcallDetected(JsonElement e)
    {
        var id = Str(e, "rollcall_id") ?? "";
        var baseUrl = Str(e, "base_url") ?? "";
        // 合併鍵 = base_url + 活動類型 + 活動ID(不同 base_url 不合併)
        var vm = Rollcalls.FirstOrDefault(r => r.Id == id && r.BaseUrl == baseUrl);
        // 新列,或上一輪已結束又被重新開放 → 都當一次新的待簽,重發英雄彈窗
        var announce = vm is null || vm.IsDone;
        if (vm is null) Rollcalls.Insert(0, vm = new RollcallVm { Id = id, BaseUrl = baseUrl });
        if (vm.IsDone) { vm.Status = "counting"; foreach (var p in vm.Accounts) { p.Signed = false; p.Method = null; } }
        vm.Kind = Str(e, "kind") ?? vm.Kind;
        vm.Course = Str(e, "course") ?? vm.Course;
        vm.AttendanceRate = Dbl(e, "attendance_rate");
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
        var vm = FindRollcall(Str(e, "rollcall_id"));
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
        var id = Str(e, "id_");
        var secs = Int(e, "remaining_secs");
        // Hold/Defer/送出後 core 會停止倒數;此時若仍收到 Countdown(Mock 的計時迴圈不理會 Hold)一律忽略,
        // 否則會把使用者的暫緩/暫緩決定翻掉、繼續自動送。只在「進行中」狀態才渲染倒數。
        switch (Str(e, "scope"))
        {
            case "rollcall" when FindRollcall(id) is { IsCounting: true } r:
                if (secs > r.TotalSecs) r.TotalSecs = secs; // 首發(最大值)當總長
                r.RemainingSecs = secs;
                break;
            case "quiz" when FindQuiz(id) is { Status: "reviewing" } q:
                if (secs > q.TotalSecs) q.TotalSecs = secs;
                q.RemainingSecs = secs;
                break;
        }
    }

    void OnQuizPrepared(JsonElement e)
    {
        var id = Str(e, "quiz_id") ?? "";
        var vm = FindQuiz(id);
        // 新列,或上一輪已結束又重新備答 → 都當一次新的待決複閱,重置狀態並重發彈窗
        var announce = vm is null || vm.Status is "done" or "discarded";
        if (vm is null) Quizzes.Insert(0, vm = new QuizVm { Id = id });
        if (vm.Status is "done" or "discarded")
        {
            vm.Status = "reviewing"; vm.RemainingSecs = null;
            foreach (var a in vm.PerAccount) a.SubmitResult = null;
            vm.Reasoning.Clear(); // 新一輪複閱:別顯示上一輪的推理串流
        }
        vm.Course = Str(e, "course") ?? vm.Course;
        // conflict_count 只作參考;送出閘門由逐題 QuestionVm.Conflict 推導(見 QuizVm.AnyConflict)
        if (e.TryGetProperty("per_account", out var perAcc))
            foreach (var a in perAcc.EnumerateArray())
            {
                var accId = Str(a, "account_id") ?? "";
                var accVm = vm.PerAccount.FirstOrDefault(x => x.AccountId == accId);
                if (accVm is null) vm.PerAccount.Add(accVm = new QuizAccountVm { AccountId = accId });
                accVm.Label = AccountLabel(accId);
                accVm.Questions.Clear(); // 重備答=以新題面為準;SubmitResult 保留
                if (a.TryGetProperty("questions", out var qs))
                    foreach (var q in qs.EnumerateArray())
                    {
                        var subjectId = Str(q, "subject_id") ?? "";
                        if (!vm.Reasoning.TryGetValue(subjectId, out var reasoning))
                            vm.Reasoning[subjectId] = reasoning = new ReasoningVm();
                        accVm.Questions.Add(new QuestionVm
                        {
                            SubjectId = subjectId,
                            Stem = Str(q, "stem") ?? "",
                            Answer = Str(q, "answer") ?? "",
                            Conflict = Bool(q, "conflict"),
                            Reasoning = reasoning,
                        });
                    }
            }
        vm.RaiseProgress();
        vm.RaiseConflictState();   // 依剛建好的逐題旗標刷新閘門/警示
        Raise(nameof(Quizzes)); // 讓列表的題數等衍生值刷新
        if (announce) HeroQuiz?.Invoke(vm);
    }

    void OnReasoningChunk(JsonElement e)
    {
        if (FindQuiz(Str(e, "quiz_id")) is not { } vm) return;
        var subjectId = Str(e, "subject_id") ?? "";
        if (!vm.Reasoning.TryGetValue(subjectId, out var reasoning))
            vm.Reasoning[subjectId] = reasoning = new ReasoningVm();
        reasoning.Append(Str(e, "text") ?? "");
    }

    void OnAnswerUpdated(JsonElement e)
    {
        if (FindQuiz(Str(e, "quiz_id")) is not { } vm) return;
        var accId = Str(e, "account_id");
        var subjectId = Str(e, "subject_id");
        var q = vm.PerAccount.FirstOrDefault(a => a.AccountId == accId)?
                  .Questions.FirstOrDefault(x => x.SubjectId == subjectId);
        if (q is null) return;
        q.Source = Str(e, "source") ?? q.Source;
        q.Conflict = Bool(e, "conflict");
        // AnswerUpdated 不帶答案值:使用者定案的值以送出時暫存的為準(絕不由 UI 自行猜)
        if (q.Source == "user" && _pendingAnswers.Remove($"{vm.Id}|{accId}|{subjectId}", out var val)) q.Answer = val;
        // 閘門/警示由逐題旗標推導,conflict 兩個方向(清除或新增)都在此一次刷新
        vm.RaiseConflictState();
    }

    void OnQuizSubmitted(JsonElement e)
    {
        if (FindQuiz(Str(e, "quiz_id")) is not { } vm) return;
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

    // ---------------- 命令(UI → core) ----------------

    public Task StartMonitoring() => Send("StartMonitoring");
    public Task StopMonitoring() => Send("StopMonitoring");

    public async Task<bool> AddAccount(string label, string school, string username, string password) =>
        OkReply(await Send("AddAccount", ("label", label), ("school", school), ("username", username), ("password", password)));
    public Task SwitchAccount(string id) => Send("SwitchAccount", ("account_id", id));
    public Task DeleteAccount(string id) => Send("DeleteAccount", ("account_id", id));
    public Task Login(string id) => Send("Login", ("account_id", id));
    public async Task<bool> ImportCookies(string id, string cookiesJson) =>
        OkReply(await Send("ImportCookies", ("account_id", id), ("cookies_json", cookiesJson)));
    public Task SubmitCaptcha(string id, string text) => Send("SubmitCaptcha", ("account_id", id), ("text", text));

    public Task SignNow(string rollcallId) => Send("SignNow", ("rollcall_id", rollcallId));
    public Task DeferSignIn(string rollcallId) => Send("DeferSignIn", ("rollcall_id", rollcallId));

    public Task SubmitNow(string quizId) => Send("SubmitNow", ("quiz_id", quizId));

    public async Task HoldAnswer(QuizVm quiz)
    {
        // 契約無獨立 Held 事件;命令由 id 對應的 Reply 完成(20-contract 信封),故 Reply ok 即「已停自動送」的確認,
        // 本地標記 held 契約上成立。若真核心其實沒停而仍自動送出,後續 QuizSubmitted → OnQuizSubmitted 會把 UI 校正為 done;
        // 故不另建影子確認機制(UI 一收到任何後續事件就回到真相)。
        if (OkReply(await Send("HoldAnswer", ("quiz_id", quiz.Id))))
            await MainThread.InvokeOnMainThreadAsync(() => { quiz.Status = "held"; quiz.RemainingSecs = null; });
    }

    public async Task DiscardAnswer(QuizVm quiz)
    {
        if (OkReply(await Send("DiscardAnswer", ("quiz_id", quiz.Id))))
            await MainThread.InvokeOnMainThreadAsync(() => { quiz.Status = "discarded"; quiz.RemainingSecs = null; });
    }

    // account_id:衝突/答案是 per-account,使用者在答題詳細切到哪個帳號就定案哪個。
    // docs/20-contract.md 的 SetAnswer 已補此欄(core 端待實作);真核心接上後行為一致。
    public Task SetAnswer(string quizId, string accountId, string subjectId, string answer)
    {
        _pendingAnswers[$"{quizId}|{accountId}|{subjectId}"] = answer;
        return Send("SetAnswer", ("quiz_id", quizId), ("account_id", accountId), ("subject_id", subjectId), ("answer", answer));
    }

    public async Task<bool> SetLlmKey(string key) => OkReply(await Send("SetLlmKey", ("key", key)));

    public async Task<bool> SaveConfig(int countdownSecs, double thresholdPct, bool thresholdEnabled) =>
        // 鍵名對齊 core Settings(config.rs)。core 只有單一 attendance_gate_percent:停用門檻 = 送 0%
        // (全班簽到率永遠 ≥ 0 → 門檻永遠通過),不需 core 端另加 enabled 欄位。
        OkReply(await Send("UpdateConfig", ("patch", new Dictionary<string, object?>
        {
            ["countdown_secs"] = countdownSecs,
            ["attendance_gate_percent"] = thresholdEnabled ? thresholdPct : 0.0,
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
