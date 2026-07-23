using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Runtime.CompilerServices;

namespace Ui;

public abstract class ObservableObject : INotifyPropertyChanged
{
    public event PropertyChangedEventHandler? PropertyChanged;

    protected bool Set<T>(ref T field, T value, [CallerMemberName] string? name = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value)) return false;
        field = value;
        Raise(name);
        return true;
    }

    protected void Raise(string? name) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

/// <summary>倒數由 core 持有;UI 只渲染這兩個值(列表列/詳細頁/英雄彈窗三處綁同一 VM)。</summary>
public interface ICountdownVm : INotifyPropertyChanged
{
    int? RemainingSecs { get; }
    int TotalSecs { get; }
}

public sealed class CapsVm : ObservableObject
{
    bool _bg, _selfUpdate, _qr, _ocr;
    public bool BackgroundMonitoring { get => _bg; set { if (Set(ref _bg, value)) Raise(nameof(ForegroundOnly)); } }
    public bool SelfUpdate { get => _selfUpdate; set => Set(ref _selfUpdate, value); }
    public bool QrTeacherAssist { get => _qr; set => Set(ref _qr, value); }
    public bool OcrCaptcha { get => _ocr; set => Set(ref _ocr, value); }
    public bool ForegroundOnly => !BackgroundMonitoring;
}

public sealed class AccountVm : ObservableObject
{
    public required string Id { get; init; }
    string _label = "", _username = "", _schoolRef = "", _state = "offline";
    bool _isActive; string? _error;

    public string Label { get => _label; set => Set(ref _label, value); }
    public string Username { get => _username; set => Set(ref _username, value); }
    public string SchoolRef { get => _schoolRef; set => Set(ref _schoolRef, value); }
    public bool IsActive { get => _isActive; set => Set(ref _isActive, value); }
    public string? Error { get => _error; set => Set(ref _error, value); }
    public string State
    {
        get => _state;
        set { if (Set(ref _state, value)) { Raise(nameof(StateText)); Raise(nameof(LoginFailed)); } }
    }
    public bool LoginFailed => State == "login_failed";
    public string StateText => State switch { "online" => "已連線", "login_failed" => "登入失敗", _ => "未登入" };
}

public sealed record SchoolVm(string Key, string Label, string BaseUrl);

/// <summary>下一堂課(core 由 /api/my-courses 推導);null ⇒ 首頁該卡整塊隱藏。</summary>
public sealed record NextClassVm(string AccountId, string Course, DateTimeOffset StartTime, string Location)
{
    public string When
    {
        get
        {
            var d = StartTime - DateTimeOffset.Now;
            if (d.TotalMinutes < 1) return "即將開始";
            if (d.TotalHours < 1) return $"約 {Math.Round(d.TotalMinutes)} 分鐘後";
            if (d.TotalDays < 1) return $"約 {Math.Round(d.TotalHours)} 小時後 · {StartTime:HH:mm}";
            return $"{StartTime:M/d HH:mm}";
        }
    }
}

public sealed record LogEntry(DateTime At, string Level, string Text)
{
    public string Display => $"{At:HH:mm:ss}  [{Level}]  {Text}";
}

// ---------------- 點名 ----------------

public sealed class RollcallVm : ObservableObject, ICountdownVm
{
    public required string Id { get; init; }
    public string BaseUrl { get; set; } = "";
    public string Kind { get; set; } = "";
    public string Course { get; set; } = "";
    public double AttendanceRate { get; set; }
    public DateTime DetectedAt { get; } = DateTime.Now;
    public ObservableCollection<RollcallAccountVm> Accounts { get; } = [];

    int? _remaining; int _total; string _status = "counting"; // counting | pending | done

    public int? RemainingSecs { get => _remaining; set => Set(ref _remaining, value); }
    public int TotalSecs { get => _total; set => Set(ref _total, value); }
    public string Status
    {
        get => _status;
        set { if (Set(ref _status, value)) { Raise(nameof(StatusText)); Raise(nameof(IsCounting)); Raise(nameof(IsPending)); Raise(nameof(IsDone)); } }
    }

    public bool IsCounting => Status == "counting";
    public bool IsPending => Status == "pending";
    public bool IsDone => Status == "done";
    public string KindText => Kind switch { "radar" => "雷達", "qr" => "QR Code", "number" => "數字碼", _ => Kind };
    public int SignedCount => Accounts.Count(a => a.Signed);
    public string SubtitleText => $"{KindText} · {DetectedAt:HH:mm} · 全班簽到率 {AttendanceRate:0.#}%";
    public string StatusText => Status switch
    {
        "pending" => "暫緩中 · 可補簽",
        "done" => $"已簽到 {SignedCount}/{Accounts.Count}",
        _ => $"進行中 · 已簽 {SignedCount}/{Accounts.Count}",
    };

    public void RaiseProgress() { Raise(nameof(SignedCount)); Raise(nameof(StatusText)); }
}

public sealed class RollcallAccountVm : ObservableObject
{
    public required string AccountId { get; init; }
    string _label = ""; bool _signed; string? _method;

    public string Label { get => _label; set => Set(ref _label, value); }
    public string? Method { get => _method; set { if (Set(ref _method, value)) Raise(nameof(StateText)); } }
    public bool Signed
    {
        get => _signed;
        set { if (Set(ref _signed, value)) Raise(nameof(StateText)); }
    }
    public string StateText => Signed ? (Method is null ? "已簽到" : $"已簽到 · {Method}") : "等待中";
}

// ---------------- 答題 ----------------

public sealed class QuizVm : ObservableObject, ICountdownVm
{
    public required string Id { get; init; }
    public string Course { get; set; } = "";
    public DateTime DetectedAt { get; } = DateTime.Now;
    public ObservableCollection<QuizAccountVm> PerAccount { get; } = [];
    /// <summary>subject_id → 共享推理串流(跨帳號同題共用一份,ReasoningChunk 無 account_id)。</summary>
    public Dictionary<string, ReasoningVm> Reasoning { get; } = [];

    int? _remaining; int _total; string _status = "reviewing"; // reviewing | held | discarded | done

    public int? RemainingSecs { get => _remaining; set => Set(ref _remaining, value); }
    public int TotalSecs { get => _total; set => Set(ref _total, value); }
    public string Status
    {
        get => _status;
        set { if (Set(ref _status, value)) { Raise(nameof(CanSubmit)); Raise(nameof(StatusText)); Raise(nameof(ActionsVisible)); Raise(nameof(HasConflicts)); } }
    }

    // 送出閘門的權威真相 = 逐帳號逐題的 conflict 旗標(UI 已持有),不靠 core 的純量 conflict_count。
    // 只要「任一帳號任一題」仍衝突就鎖送出,直到全部經 SetAnswer 定案——絕不靜默覆蓋任何帳號的既有作答。
    public bool AnyConflict => PerAccount.Any(a => a.Questions.Any(q => q.Conflict));
    public int ConflictCount => PerAccount.Sum(a => a.Questions.Count(q => q.Conflict));
    public bool CanSubmit => !AnyConflict && Status is "reviewing" or "held";
    public bool ActionsVisible => Status is "reviewing" or "held";
    public bool HasConflicts => AnyConflict && ActionsVisible;
    public string ConflictText => $"尚有 {ConflictCount} 處與你既有的作答衝突,定案後才能送出";
    public int QuestionCount => PerAccount.FirstOrDefault()?.Questions.Count ?? 0;
    public int SubmittedCount => PerAccount.Count(a => a.SubmitResult != null);
    public string SubtitleText => $"{QuestionCount} 題 · {DetectedAt:HH:mm}";
    public string StatusText => Status switch
    {
        "done" => $"已送出 {SubmittedCount}/{PerAccount.Count}",
        "held" => "已暫緩 · 待手動送出",
        "discarded" => "已捨棄",
        _ => HasConflicts ? $"審題中 · {ConflictCount} 處衝突" : "審題中",
    };

    /// <summary>某題 conflict 旗標變動後呼叫,一次刷新所有衍生的閘門/警示/狀態文字。</summary>
    public void RaiseConflictState() { Raise(nameof(AnyConflict)); Raise(nameof(ConflictCount)); Raise(nameof(CanSubmit)); Raise(nameof(HasConflicts)); Raise(nameof(ConflictText)); Raise(nameof(StatusText)); }

    public void RaiseProgress() { Raise(nameof(SubmittedCount)); Raise(nameof(StatusText)); Raise(nameof(SubtitleText)); }
}

public sealed class QuizAccountVm : ObservableObject
{
    public required string AccountId { get; init; }
    string _label = ""; string? _submitResult;

    public string Label { get => _label; set => Set(ref _label, value); }
    public ObservableCollection<QuestionVm> Questions { get; } = [];
    public string? SubmitResult
    {
        get => _submitResult;
        set { if (Set(ref _submitResult, value)) Raise(nameof(Submitted)); }
    }
    public bool Submitted => SubmitResult != null;
}

public sealed class QuestionVm : ObservableObject
{
    public required string SubjectId { get; init; }
    public string Stem { get; set; } = "";
    public ReasoningVm? Reasoning { get; set; }

    string _answer = ""; bool _conflict; string _source = "llm";

    public string Answer { get => _answer; set => Set(ref _answer, value); }
    public bool Conflict { get => _conflict; set => Set(ref _conflict, value); }
    public string Source
    {
        get => _source;
        set { if (Set(ref _source, value)) Raise(nameof(SourceText)); }
    }
    public string SourceText => Source == "user" ? "你定案" : "LLM";
}

public sealed class ReasoningVm : ObservableObject
{
    string _text = "";
    public string Text => _text;
    public bool HasText => _text.Length > 0;
    public void Append(string chunk) { _text += chunk; Raise(nameof(Text)); Raise(nameof(HasText)); }
}
