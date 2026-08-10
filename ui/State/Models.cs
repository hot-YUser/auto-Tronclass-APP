using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Runtime.CompilerServices;
using System.Text.Json;
using System.Text.Json.Serialization;

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

/// <summary>紀錄用的輕量格式器(顯示層,無相依)。</summary>
static class Fmt
{
    /// 紀錄時間:今天/昨天只顯示時刻,更早顯示日期。
    public static string Detected(DateTime at)
    {
        var day = at.Date;
        var today = DateTime.Now.Date;
        var prefix = day == today ? "今天" : day == today.AddDays(-1) ? "昨天" : at.ToString("M/d");
        return $"{prefix} {at:HH:mm}";
    }
}

/// <summary>倒數由 core 持有;UI 只渲染這兩個值(列表列/詳細頁/英雄彈窗三處綁同一 VM)。</summary>
public interface ICountdownVm : INotifyPropertyChanged
{
    int? RemainingSecs { get; }
    int TotalSecs { get; }
}

/// <summary>有防假點名門檻的倒數(點名才有,測驗沒有)。未達門檻時 core 不倒數,
/// 倒數欄位改渲染「即時簽到率 → 門檻」的進度,達標後讓回倒數。</summary>
public interface IGateVm : ICountdownVm
{
    bool Holding { get; }          // 簽到率未達門檻、暫不自動簽(仍可手動「立即簽到」)
    double AttendanceRate { get; } // 全班即時簽到率 %
    double GatePercent { get; }    // 門檻 %
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

/// <summary>核心目前生效的設定快照,讓設定頁反映「已存的值」而非只有預設。
/// api_key 是機密、永不過縫——只帶 <see cref="HasLlmKey"/> 布林表示是否已設定。</summary>
public sealed record SettingsSnapshot(
    int CountdownSecs, double AttendanceGatePercent,
    string LlmEndpoint, string LlmModel, int LlmMaxTokens,
    bool ResubmitForCorrect, bool EnableLlmTools, bool HasLlmKey);

public sealed class AccountVm : ObservableObject
{
    public required string Id { get; init; }
    string _label = "", _username = "", _schoolRef = "", _state = "offline";
    bool _isActive, _isTeacher; string? _error, _courseId;

    public string Label { get => _label; set => Set(ref _label, value); }
    public string Username { get => _username; set => Set(ref _username, value); }
    public string SchoolRef { get => _schoolRef; set => Set(ref _schoolRef, value); }
    public bool IsActive { get => _isActive; set => Set(ref _isActive, value); }
    /// 教師帳號:偵測到 QR 點名時,用它開一場點名取得輪替 data 替學生簽到(core spawn_qr_teacher_assist)。
    public bool IsTeacher { get => _isTeacher; set => Set(ref _isTeacher, value); }
    /// 教師帳號主持 QR 點名的課程;null/空 = core 退回教師的第一門課。
    public string? CourseId { get => _courseId; set => Set(ref _courseId, value); }
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

public sealed class RollcallVm : ObservableObject, IGateVm
{
    public required string ActivityToken { get; init; }
    public required string Id { get; init; }
    public string BaseUrl { get; set; } = "";
    public DateTime DetectedAt { get; } = DateTime.Now;
    public ObservableCollection<RollcallAccountVm> Accounts { get; } = [];

    string _kind = "", _course = "";
    int? _remaining; int _total; string _status = "counting"; // counting | pending | done
    double _rate; double _gate; bool _holding;                // 防假點名門檻(core 的 RollcallGate 事件推來)

    public string Kind { get => _kind; set { if (Set(ref _kind, value)) { Raise(nameof(KindText)); Raise(nameof(KindEmblem)); Raise(nameof(MetaText)); } } }
    public string Course { get => _course; set => Set(ref _course, value); }

    /// 全班即時簽到率 %。未達門檻時 core 每秒重查一次,所以這個值是活的。
    public double AttendanceRate
    {
        get => _rate;
        set { if (Set(ref _rate, value)) { Raise(nameof(AttendanceRateText)); Raise(nameof(StatusText)); Raise(nameof(MetaText)); } }
    }
    public double GatePercent
    {
        get => _gate;
        set { if (Set(ref _gate, value)) Raise(nameof(StatusText)); }
    }
    /// true = 簽到率未達門檻,core 不會自動簽(但「立即簽到」仍可手動覆蓋)。
    public bool Holding
    {
        get => _holding;
        set { if (Set(ref _holding, value)) { Raise(nameof(StatusText)); Raise(nameof(StatusTag)); } }
    }
    public string AttendanceRateText => $"{AttendanceRate:0.#}%";

    public int? RemainingSecs { get => _remaining; set => Set(ref _remaining, value); }
    public int TotalSecs { get => _total; set => Set(ref _total, value); }
    public string Status
    {
        get => _status;
        set { if (Set(ref _status, value)) { Raise(nameof(StatusText)); Raise(nameof(StatusTag)); Raise(nameof(IsCounting)); Raise(nameof(IsPending)); Raise(nameof(IsDone)); } }
    }

    public bool IsCounting => Status == "counting";
    public bool IsPending => Status == "pending";
    public bool IsDone => Status == "done";
    public string KindText => Kind switch { "radar" => "雷達", "qr" => "QR Code", "number" => "數字碼", _ => Kind };
    /// 徽章字紋(2 字內):列表卡左側的類型標記。
    public string KindEmblem => Kind switch { "radar" => "雷達", "qr" => "QR", "number" => "數字", _ => KindText.Length >= 2 ? KindText[..2] : KindText };
    public int SignedCount => Accounts.Count(a => a.Signed);
    public string DetectedAtText => Fmt.Detected(DetectedAt);
    /// 卡片副標:類型 · 時間(· 簽到率,已知時)。
    public string MetaText => AttendanceRate > 0
        ? $"{KindText} · {DetectedAtText} · 簽到率 {AttendanceRate:0.#}%"
        : $"{KindText} · {DetectedAtText}";
    /// 右上狀態膠囊的短標籤(顏色由畫面依 Status/Holding 決定)。
    public string StatusTag => Status switch
    {
        "pending" => "已暫緩",
        "done" => "已完成",
        _ when Holding => "等待門檻",
        _ => "進行中",
    };
    public string StatusText => Status switch
    {
        "pending" => "暫緩中 · 可補簽",
        "done" => $"已簽到 {SignedCount}/{Accounts.Count}",
        // 未達門檻:講清楚在等什麼(差多少),而不是含糊的「進行中」。
        _ when Holding => $"等待簽到率 · {AttendanceRate:0.#}% / 門檻 {GatePercent:0.#}%",
        _ => $"進行中 · 已簽 {SignedCount}/{Accounts.Count}",
    };

    public void RaiseProgress() { Raise(nameof(SignedCount)); Raise(nameof(StatusText)); }
}

public sealed class RollcallAccountVm : ObservableObject
{
    public required string AccountId { get; init; }
    string _label = ""; bool _signed; string? _method;

    public string Label { get => _label; set { if (Set(ref _label, value)) Raise(nameof(ChipText)); } }
    public string? Method { get => _method; set { if (Set(ref _method, value)) { Raise(nameof(StateText)); Raise(nameof(MethodText)); Raise(nameof(ChipText)); } } }
    public bool Signed
    {
        get => _signed;
        set { if (Set(ref _signed, value)) { Raise(nameof(StateText)); Raise(nameof(ChipText)); } }
    }
    /// 簽到方式短名(雷達 / 數字碼 / QR / 自助)。
    public string MethodText => Method switch
    {
        null or "" => "",
        "radar" => "雷達",
        "number" => "數字碼",
        "self_registration" => "自助",
        var m when m.StartsWith("qr") => "QR",
        var m => m,
    };
    public string StateText => Signed ? (MethodText.Length > 0 ? $"已簽到 · {MethodText}" : "已簽到") : "等待中";
    /// 列表卡的逐帳號膠囊文字。
    public string ChipText => Signed
        ? (MethodText.Length > 0 ? $"✓ {Label} · {MethodText}" : $"✓ {Label}")
        : $"{Label} · 等待中";
}

// ---------------- 答題 ----------------

public sealed class QuizVm : ObservableObject, ICountdownVm
{
    public required string ActivityToken { get; init; }
    public required string Id { get; init; }
    public DateTime DetectedAt { get; } = DateTime.Now;
    public ObservableCollection<QuizAccountVm> PerAccount { get; } = [];
    public HashSet<string> ExpectedAccountIds { get; } = new(StringComparer.Ordinal);
    /// <summary>(account_id, subject_id) → 該帳號該題的推理串流。</summary>
    public Dictionary<(string AccountId, string SubjectId), ReasoningVm> Reasoning { get; } = [];

    string _course = "";
    int? _remaining; int _total; string _status = "reviewing"; // reviewing | held | discarded | done

    public string Course { get => _course; set => Set(ref _course, value); }
    public int? RemainingSecs { get => _remaining; set => Set(ref _remaining, value); }
    public int TotalSecs { get => _total; set => Set(ref _total, value); }
    public string Status
    {
        get => _status;
        set { if (Set(ref _status, value)) { Raise(nameof(CanSubmit)); Raise(nameof(StatusText)); Raise(nameof(StatusTag)); Raise(nameof(ActionsVisible)); Raise(nameof(HasConflicts)); } }
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
    public string DetectedAtText => Fmt.Detected(DetectedAt);
    /// 徽章字紋:答題沒有子類型,固定「測驗」。
    public string KindEmblem => "測驗";
    public string SubtitleText => $"{QuestionCount} 題 · {DetectedAtText}";
    /// 右上狀態膠囊的短標籤(顏色由畫面依 Status/衝突 決定)。
    public string StatusTag => Status switch
    {
        "done" => "已送出",
        "held" => "已暫緩",
        "discarded" => "已捨棄",
        _ => AnyConflict ? "待定案" : "審題中",
    };
    public string StatusText => Status switch
    {
        "done" => $"已送出 {SubmittedCount}/{PerAccount.Count}",
        "held" => "已暫緩 · 待手動送出",
        "discarded" => "已捨棄",
        _ => HasConflicts ? $"審題中 · {ConflictCount} 處衝突" : "審題中",
    };

    /// <summary>某題 conflict 旗標變動後呼叫,一次刷新所有衍生的閘門/警示/狀態文字。</summary>
    public void RaiseConflictState() { Raise(nameof(AnyConflict)); Raise(nameof(ConflictCount)); Raise(nameof(CanSubmit)); Raise(nameof(HasConflicts)); Raise(nameof(ConflictText)); Raise(nameof(StatusText)); Raise(nameof(StatusTag)); }

    public void RaiseProgress() { Raise(nameof(SubmittedCount)); Raise(nameof(StatusText)); Raise(nameof(SubtitleText)); }
}

public sealed class QuizAccountVm : ObservableObject
{
    public required string AccountId { get; init; }
    string _label = ""; string? _submitResult; string _attemptState = "waiting";

    public string AttemptState
    {
        get => _attemptState;
        set { if (Set(ref _attemptState, value)) Raise(nameof(ChipText)); }
    }

    public string Label { get => _label; set { if (Set(ref _label, value)) Raise(nameof(ChipText)); } }
    public ObservableCollection<QuestionVm> Questions { get; } = [];
    public string? SubmitResult
    {
        get => _submitResult;
        set { if (Set(ref _submitResult, value)) { Raise(nameof(Submitted)); Raise(nameof(ChipText)); } }
    }
    public bool Submitted => SubmitResult != null;
    /// 列表卡的逐帳號膠囊文字。
    public string ChipText => Submitted ? $"✓ {Label}" : AttemptState switch
    {
        "failed" => $"✕ {Label} · 準備失敗",
        "gone" => $"— {Label} · 活動已結束",
        "preparing" or "waiting" => $"{Label} · 準備中",
        _ => $"{Label} · 待送出",
    };
}

public sealed class QuestionVm : ObservableObject
{
    public required string SubjectId { get; init; }
    public string Stem { get; set; } = "";
    public string QuestionType { get; set; } = "";
    public string AnswerType { get; set; } = "";
    public IReadOnlyList<QuestionOptionVm> Options { get; set; } = [];
    public ReasoningVm? Reasoning { get; set; }

    AnswerWire? _answerPayload;
    bool _conflict;
    string _source = "llm";

    public AnswerWire? AnswerPayload
    {
        get => _answerPayload;
        set { if (Set(ref _answerPayload, value)) Raise(nameof(Answer)); }
    }
    public string Answer => AnswerPayload?.Display ?? "";
    public bool Conflict { get => _conflict; set => Set(ref _conflict, value); }
    public string Source
    {
        get => _source;
        set { if (Set(ref _source, value)) Raise(nameof(SourceText)); }
    }
    public string SourceText => Source == "user" ? "你定案" : "LLM";
}

public sealed record QuestionOptionVm(string Id, string Text);

/// <summary>
/// UI ↔ core 的唯一答案 wire model。保留答案種類，避免把選項 ID、填空陣列與自由文字
/// 壓成不可逆字串；屬性名稱直接對齊 Rust <c>AnswerWire</c>。
/// </summary>
public sealed record AnswerWire
{
    [JsonPropertyName("kind")]
    public required string Kind { get; init; }

    [JsonPropertyName("option_ids")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string[]? OptionIds { get; init; }

    [JsonPropertyName("values")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string[]? Values { get; init; }

    [JsonPropertyName("value")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Value { get; init; }

    [JsonPropertyName("letters")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string[]? Letters { get; init; }

    [JsonIgnore]
    public string Display => Kind switch
    {
        "options" => string.Join(", ", OptionIds ?? []),
        "blanks" => string.Join(" ||| ", Values ?? []),
        "vote" => string.Join(", ", Letters ?? []),
        "text" => Value ?? "",
        _ => "",
    };

    public static AnswerWire? FromJson(JsonElement element)
    {
        if (element.ValueKind is JsonValueKind.Null or JsonValueKind.Undefined) return null;
        if (element.ValueKind != JsonValueKind.Object ||
            !element.TryGetProperty("kind", out var kindElement) ||
            kindElement.ValueKind != JsonValueKind.String)
            return null;

        var kind = kindElement.GetString() ?? "";
        return kind switch
        {
            "options" when NonEmptyStringArray(element, "option_ids") is { } values =>
                new AnswerWire { Kind = kind, OptionIds = values },
            "blanks" when NonEmptyStringArray(element, "values") is { } values =>
                new AnswerWire { Kind = kind, Values = values },
            "text" when NonEmptyString(element, "value") is { } value =>
                new AnswerWire { Kind = kind, Value = value },
            "vote" when NonEmptyStringArray(element, "letters") is { } values =>
                new AnswerWire { Kind = kind, Letters = values },
            _ => null,
        };
    }

    public AnswerWire FromManualInput(string input)
    {
        var trimmed = input.Trim();
        return Kind switch
        {
            "options" => new AnswerWire { Kind = Kind, OptionIds = SplitList(trimmed) },
            "blanks" => new AnswerWire
            {
                Kind = Kind,
                Values = trimmed.Split("|||", StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries),
            },
            "vote" => new AnswerWire { Kind = Kind, Letters = SplitList(trimmed) },
            _ => new AnswerWire { Kind = "text", Value = trimmed },
        };
    }

    static string[] SplitList(string input) =>
        input.Split([',', '，', ' ', '\t', '\r', '\n'], StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries);

    static string[]? NonEmptyStringArray(JsonElement element, string property)
    {
        if (!element.TryGetProperty(property, out var values) || values.ValueKind != JsonValueKind.Array)
            return null;
        var parsed = new List<string>();
        foreach (var value in values.EnumerateArray())
        {
            if (value.ValueKind != JsonValueKind.String || string.IsNullOrWhiteSpace(value.GetString()))
                return null;
            parsed.Add(value.GetString()!);
        }
        return parsed.Count > 0 ? parsed.ToArray() : null;
    }

    static string? NonEmptyString(JsonElement element, string property) =>
        element.TryGetProperty(property, out var value) && value.ValueKind == JsonValueKind.String &&
        !string.IsNullOrWhiteSpace(value.GetString())
            ? value.GetString()
            : null;
}

public sealed class ReasoningVm : ObservableObject
{
    string _text = "";
    public string Text => _text;
    public bool HasText => _text.Length > 0;
    public void Append(string chunk) { _text += chunk; Raise(nameof(Text)); Raise(nameof(HasText)); }
}
