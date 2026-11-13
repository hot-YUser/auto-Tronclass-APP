using System.Collections.ObjectModel;
using TronClass.Interop;

namespace Ui;

/// <summary>
/// UI 的唯一狀態層。核心事件回到 UI thread 後才改狀態,所以集合與標量都不需要鎖。
/// 核心 → UI 的事件套用在 AppState.Events.cs,UI → 核心的命令在 AppState.Commands.cs;
/// 失敗與例外一律進 Toast 與 Logs。
/// </summary>
public sealed partial class AppState : ObservableObject
{
    readonly ICore _core;
    readonly ScheduleCoordinator _schedule;
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

    /// <summary>頁面層的即時提示(儲存成功之類)也走同一條 Toast 通道。</summary>
    public void Notify(string severity, string message) => Toast?.Invoke(severity, message);

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
}
