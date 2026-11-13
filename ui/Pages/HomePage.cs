using System.Collections.Specialized;
using System.ComponentModel;

namespace Ui;

/// <summary>監控首頁：每個帳號與群組一張卡，加上整體的開始與停止。</summary>
public sealed class HomePage : ContentPage
{
    readonly AppState _state;
    readonly Label _session = Theme.Strong("等待核心…", 17);
    readonly Label _globalReason = Theme.Text("", 12.5, Theme.FontRegular, Theme.DangerL, Theme.DangerD);
    readonly Label _platform = Theme.Dim("", 12);
    readonly Button _primary;
    readonly KeyedStack<TargetSnapshotContract, TargetCard> _personal;
    readonly KeyedStack<TargetSnapshotContract, TargetCard> _groups;
    readonly KeyedStack<MergePromptContract, MergePromptCard> _mergePrompts;
    readonly Label _personalEmpty = Theme.Dim("尚無學生帳號；請先到「帳號」新增並驗證。", 13);
    readonly Label _groupsEmpty = Theme.Dim("尚未建立群組。", 13);
    readonly VerticalStackLayout _mergeSection;
    readonly VerticalStackLayout _feedSection;
    readonly VerticalStackLayout _feed = new() { Spacing = 8 };
    readonly ContentView _nextClassHost = new() { IsVisible = false };
    bool _attached;

    public HomePage(AppState state)
    {
        _state = state;
        Title = "監控";
        _primary = Theme.Primary("", RunPrimaryAction);
        _personal = new(
            target => target.Target.Id,
            () => new TargetCard(state, this),
            (card, target) => card.Update(target)) { Spacing = 10 };
        _groups = new(
            target => target.Target.Id,
            () => new TargetCard(state, this),
            (card, target) => card.Update(target)) { Spacing = 10 };
        _mergePrompts = new(
            prompt => prompt.ComponentId,
            () => new MergePromptCard(state, this),
            (card, prompt) => card.Update(prompt)) { Spacing = 10 };
        _mergeSection = Section("群組重疊", _mergePrompts);
        _feedSection = Section("近期活動", _feed);

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    _nextClassHost,
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 10,
                        Children = { _session, _globalReason, _primary, _platform },
                    }),
                    Section("個人帳號", _personalEmpty, _personal),
                    Section("群組", _groupsEmpty, _groups),
                    new HorizontalStackLayout
                    {
                        Children =
                        {
                            Theme.Ghost(
                                "＋ 新增群組",
                                () => Navigation.PushAsync(new GroupEditorPage(state))),
                        },
                    },
                    _mergeSection,
                    _feedSection,
                },
            },
        };
    }

    static VerticalStackLayout Section(string title, params View[] body)
    {
        var section = new VerticalStackLayout { Spacing = 10 };
        section.Children.Add(Theme.Section(title));
        foreach (var view in body) section.Children.Add(view);
        return section;
    }

    protected override void OnAppearing()
    {
        base.OnAppearing();
        if (_attached) return;
        _attached = true;
        _state.MonitoringChanged += RenderMonitoring;
        _state.CommandStateChanged += RenderMonitoring;
        _state.PropertyChanged += OnStateChanged;
        _state.Rollcalls.CollectionChanged += OnActivitiesChanged;
        _state.Quizzes.CollectionChanged += OnActivitiesChanged;
        RenderMonitoring();
        BuildNextClass();
        BuildFeed();
    }

    protected override void OnDisappearing()
    {
        base.OnDisappearing();
        if (!_attached) return;
        _attached = false;
        _state.MonitoringChanged -= RenderMonitoring;
        _state.CommandStateChanged -= RenderMonitoring;
        _state.PropertyChanged -= OnStateChanged;
        _state.Rollcalls.CollectionChanged -= OnActivitiesChanged;
        _state.Quizzes.CollectionChanged -= OnActivitiesChanged;
    }

    void OnStateChanged(object? sender, PropertyChangedEventArgs args)
    {
        if (args.PropertyName == nameof(AppState.NextClass)) BuildNextClass();
    }

    void OnActivitiesChanged(object? sender, NotifyCollectionChangedEventArgs args) => BuildFeed();

    /// <summary>
    /// 快照或進行中的命令有變動時呼叫。卡片依 key 重用，這裡只是把新值寫進既有視圖；
    /// 值沒變的屬性不會觸發重新排版。
    /// </summary>
    void RenderMonitoring()
    {
        var snapshot = _state.Monitoring;
        _session.Text = snapshot is null ? "等待核心…" : SessionText(snapshot.SessionState);
        _globalReason.Show(snapshot?.GlobalDisabledReason);
        _platform.Show(snapshot is null ? null : WakeText(snapshot.WakeMode));

        var targets = snapshot?.Targets ?? [];
        var personal = targets.Where(target => target.Target.Kind == "account").ToArray();
        var groups = targets.Where(target => target.Target.Kind == "group").ToArray();
        _primary.IsVisible = snapshot is { CanStopAll: true } or { CanResume: true };
        _primary.Text = snapshot is { CanStopAll: true } ? "一鍵停止全部" : "恢復照時間表";
        _primary.IsEnabled = personal.Length > 0 &&
                             snapshot?.GlobalDisabledReason is null &&
                             !_state.IsCommandPending("monitoring:all");

        _personalEmpty.IsVisible = personal.Length == 0;
        _personal.Sync(personal);
        _groupsEmpty.IsVisible = groups.Length == 0;
        _groups.Sync(groups);
        var prompts = snapshot?.MergePrompts ?? [];
        _mergeSection.IsVisible = prompts.Length > 0;
        _mergePrompts.Sync(prompts);
    }

    async Task RunPrimaryAction()
    {
        var snapshot = _state.Monitoring;
        if (snapshot is null) return;
        if (snapshot.CanStopAll) await _state.StopAllMonitoring();
        else if (snapshot.CanResume) await _state.ResumeScheduledMonitoring();
    }

    static string SessionText(string state) => state switch
    {
        "running" => "監控中",
        "starting" => "正在準備監控",
        "stopping" => "正在停止監控",
        "platform_blocked" => "平台限制，已暫停偵測",
        "error" => "監控核心發生錯誤",
        _ => "等待時間表或手動開始",
    };

    static string WakeText(string mode) => mode switch
    {
        "exact" => "Android：已允許精準喚醒。強制停止後需重新開啟 App。",
        "inexact_user_action_required" => "Android：未允許精準鬧鐘；到時間只會通知，需點擊才會開始。",
        "unavailable" => "此平台無法在 App 關閉後自動喚醒。",
        _ => "Windows：App 開著時才會依時間表執行。",
    };

    void BuildNextClass()
    {
        if (_state.NextClass is not { } next)
        {
            _nextClassHost.IsVisible = false;
            _nextClassHost.Content = null;
            return;
        }
        _nextClassHost.IsVisible = true;
        _nextClassHost.Content = Theme.Card(new VerticalStackLayout
        {
            Spacing = 5,
            Children =
            {
                Theme.Section("下一堂課"),
                Theme.Strong(next.Course, 16),
                Theme.Dim(
                    $"{(next.StartTime <= DateTimeOffset.Now ? "已開始" : next.When)}" +
                    (string.IsNullOrWhiteSpace(next.Location) ? "" : $" · {next.Location}") +
                    $" · {_state.AccountLabel(next.AccountId)}",
                    12.5),
            },
        });
    }

    void BuildFeed()
    {
        _feed.Children.Clear();
        var items = _state.Rollcalls
            .Select(rollcall => (
                rollcall.DetectedAt,
                view: FeedRow(
                    "點名",
                    rollcall,
                    rollcall.Course,
                    () => ((AppShell)Shell.Current).OpenRollcallDetail(rollcall),
                    static label => label.SetBinding(
                        Label.TextProperty,
                        static (RollcallVm value) => value.StatusText))))
            .Concat(_state.Quizzes.Select(quiz => (
                quiz.DetectedAt,
                view: FeedRow(
                    "答題",
                    quiz,
                    quiz.Course,
                    () => ((AppShell)Shell.Current).OpenQuizDetail(quiz),
                    static label => label.SetBinding(
                        Label.TextProperty,
                        static (QuizVm value) => value.StatusText)))))
            .OrderByDescending(item => item.DetectedAt)
            .Take(8)
            .ToArray();
        _feedSection.IsVisible = items.Length > 0;
        foreach (var item in items) _feed.Children.Add(item.view);
    }

    static View FeedRow(
        string kind,
        ObservableObject viewModel,
        string course,
        Func<Task> open,
        Action<Label> bindStatus)
    {
        var status = Theme.Dim("", 12);
        status.BindingContext = viewModel;
        bindStatus(status);
        var grid = new Grid { ColumnSpacing = 10 };
        grid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        grid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        var pill = Theme.TextPill(kind, Theme.PrimL, Theme.PrimD, Theme.PrimBgL, Theme.PrimBgD);
        pill.VerticalOptions = LayoutOptions.Center;
        grid.Add(pill, 0, 0);
        grid.Add(new VerticalStackLayout
        {
            Spacing = 2,
            Children = { Theme.Strong(course, 14), status },
        }, 1, 0);
        var card = Theme.Card(grid, 12);
        card.OnTap(open);
        return card;
    }
}

/// <summary>一個監控對象（帳號或群組）的卡片。視圖只建一次，<see cref="Update"/> 寫入新值。</summary>
sealed class TargetCard : ContentView
{
    const int ResultRows = 3;

    readonly AppState _state;
    readonly Page _page;
    readonly Label _name = Theme.Strong("", 15);
    readonly TonePill _statePill = new();
    readonly Label _schedule = Theme.Dim("", 12.5);
    readonly Label _reason = Theme.Text("", 12.5, Theme.FontSemibold, Theme.DangerL, Theme.DangerD);
    readonly Label _manual = Theme.Dim("", 12);
    readonly Label _detector = Theme.Dim("", 12.5);
    readonly Label _members = Theme.Dim("", 12.5);
    readonly Label _courses = Theme.Dim("", 12.5);
    readonly Label[] _results = [.. Enumerable.Range(0, ResultRows).Select(_ => Theme.Dim("", 12))];
    readonly Label _error = Theme.Text("", 12.5, Theme.FontRegular, Theme.DangerL, Theme.DangerD);
    readonly Button _start, _stop, _editSchedule, _editGroup, _deleteGroup;
    TargetSnapshotContract? _target;
    bool? _suppressed;

    public TargetCard(AppState state, Page page)
    {
        _state = state;
        _page = page;
        _start = Theme.Ghost("立即開始", () => With(target => state.StartTarget(target.Target)));
        _stop = Theme.Ghost("立即停止", () => With(target => state.StopTarget(target.Target)));
        _editSchedule = Theme.Ghost("時間表", () => With(target =>
            page.Navigation.PushAsync(new ScheduleEditorPage(state, target))));
        _editGroup = Theme.Ghost("編輯群組", () => With(target =>
            page.Navigation.PushAsync(new GroupEditorPage(state, target))));
        _deleteGroup = Theme.Danger("刪除群組", () => With(DeleteGroup));

        var commands = new FlexLayout { Wrap = Microsoft.Maui.Layouts.FlexWrap.Wrap };
        foreach (var button in new[] { _start, _stop, _editSchedule, _editGroup, _deleteGroup })
        {
            button.Margin = new Thickness(0, 0, 8, 6);
            commands.Children.Add(button);
        }
        var body = new VerticalStackLayout
        {
            Spacing = 7,
            Children =
            {
                new HorizontalStackLayout { Spacing = 8, Children = { _name, _statePill } },
                _schedule, _reason, _manual, _detector, _members, _courses,
            },
        };
        foreach (var row in _results) body.Children.Add(row);
        body.Children.Add(_error);
        body.Children.Add(commands);
        Content = Theme.Card(body);
    }

    public void Update(TargetSnapshotContract target)
    {
        _target = target;
        var suppressed = target.RuntimeState == "suppressed_by_group";

        _name.Text = target.Name;
        _statePill.Set(StateTone(target.RuntimeState));
        _schedule.Text = ScheduleText(target);
        if (_suppressed != suppressed)
        {
            // 被群組接手是提醒，不是錯誤：顏色跟著換。
            _suppressed = suppressed;
            _reason.Themed(
                Label.TextColorProperty,
                suppressed ? Theme.WarnL : Theme.DangerL,
                suppressed ? Theme.WarnD : Theme.DangerD);
        }
        _reason.Show(target.DisabledReason);
        _manual.Show(target.ManualOverride is { } manual
            ? manual.ExpiresAtUtc is null
                ? $"手動{(manual.ForceOpen ? "開啟" : "停止")} · App 結束即失效"
                : $"手動{(manual.ForceOpen ? "開啟" : "停止")}至 {LocalTime(manual.ExpiresAtUtc)}"
            : null);
        _detector.Show(target.Detector is { } detector
            ? $"偵測帳號：{_state.AccountLabel(detector.AccountId)}{(detector.IsFallback ? "（備援）" : "")}"
            : null);
        _members.Show(target.GroupDefinition is { } group
            ? $"成員：{string.Join("、", group.MemberAccountIds.Select(_state.AccountLabel))}"
            : null);
        _courses.Show(target.GroupDefinition is null
            ? null
            : target.Courses.Length == 0
                ? "課程：不限"
                : $"課程：{string.Join("、", target.Courses.Select(course => course.Name))}");

        var results = target.AccountResults
            .Where(result => result.Phase != "idle")
            .OrderByDescending(result => result.UpdatedAtUtc)
            .Take(ResultRows)
            .ToArray();
        for (var index = 0; index < ResultRows; index++)
            _results[index].Show(index < results.Length ? ResultLine(results[index]) : null);
        _error.Show(target.Error?.Message);

        var key = AppState.TargetCommandKey(target.Target);
        var busy = _state.IsCommandPending(key);
        _start.IsEnabled = target.CanStart && !busy;
        _stop.IsEnabled = target.CanStop && !busy;
        _editSchedule.IsEnabled = target.CanEditSchedule && !_state.IsCommandPending($"{key}:schedule");
        _editGroup.IsVisible = _deleteGroup.IsVisible = target.Target.Kind == "group";
        _editGroup.IsEnabled = !_state.IsCommandPending($"group:{target.Target.Id}:update");
        _deleteGroup.IsEnabled = !_state.IsCommandPending($"group:{target.Target.Id}:delete");
    }

    /// <summary>按鈕按下時作用在卡片「現在」顯示的對象上，而不是建立卡片當時的那一筆。</summary>
    Task With(Func<TargetSnapshotContract, Task> action) =>
        _target is { } target ? action(target) : Task.CompletedTask;

    async Task DeleteGroup(TargetSnapshotContract target)
    {
        if (_state.Monitoring is not { } snapshot) return;
        if (await _page.DisplayAlertAsync(
                "刪除群組",
                $"確定刪除「{target.Name}」？尚未開始的簽到與作答會取消。",
                "刪除",
                "取消"))
            await _state.DeleteGroup(target.Target.Id, snapshot.ConfigRevision);
    }

    static Tone StateTone(string state) => state switch
    {
        "monitoring" => new("監控中", ToneKind.Ok),
        "starting" => new("啟動中", ToneKind.Warn),
        "stopping" => new("停止中", ToneKind.Warn),
        "suppressed_by_group" => new("由群組監控", ToneKind.Warn),
        "platform_blocked" => new("平台暫停", ToneKind.Danger),
        "error" => new("錯誤", ToneKind.Danger),
        "manual_off" => new("手動停止", ToneKind.Neutral),
        _ => new("時間表外", ToneKind.Neutral),
    };

    static string ScheduleText(TargetSnapshotContract target)
    {
        var binding = target.Schedule.Kind switch
        {
            ScheduleBindingKind.Disabled => "時間表：停用",
            ScheduleBindingKind.InheritGlobal => "時間表：跟隨全局",
            _ => "時間表：自訂",
        };
        return target.NextBoundaryUtc is null
            ? binding
            : $"{binding} · 下次切換 {LocalTime(target.NextBoundaryUtc)}";
    }

    static string LocalTime(string utc) =>
        DateTimeOffset.TryParse(utc, out var instant)
            ? instant.ToLocalTime().ToString("MM/dd HH:mm")
            : utc;

    string ResultLine(AccountResultContract result)
    {
        var detail = result.Error?.Message ??
                     string.Join(" · ", new[] { result.ActivityKind, result.CourseName }
                         .Where(value => !string.IsNullOrWhiteSpace(value)));
        var phase = result.Phase switch
        {
            "pending" => "準備中",
            "authorized" => "送出中",
            "succeeded" => "成功",
            "failed" => "失敗",
            "unknown_after_restart" => "重啟後結果不明",
            _ => "閒置",
        };
        return $"{_state.AccountLabel(result.AccountId)}：{phase}" +
               (detail.Length == 0 ? "" : $" · {detail}");
    }
}

/// <summary>兩個以上啟用中的群組有重疊成員時的提示卡。</summary>
sealed class MergePromptCard : ContentView
{
    readonly AppState _state;
    readonly Label _groups = Theme.Strong("", 14);
    readonly Label _detail = Theme.Dim("", 12.5);
    readonly Button _temporary;
    MergePromptContract? _prompt;

    public MergePromptCard(AppState state, Page page)
    {
        _state = state;
        _temporary = Theme.Ghost("", () =>
            _prompt is { } prompt && state.Monitoring is { } snapshot
                ? state.AcknowledgeTemporaryMerge(prompt.ComponentId, snapshot.PlanRevision)
                : Task.CompletedTask);
        var permanent = Theme.Primary("永久合併", () =>
            _prompt is { } prompt
                ? page.Navigation.PushAsync(new GroupEditorPage(state, prompt.GroupIds))
                : Task.CompletedTask);
        var actions = new FlexLayout { Wrap = Microsoft.Maui.Layouts.FlexWrap.Wrap };
        foreach (var button in new[] { _temporary, permanent })
        {
            button.Margin = new Thickness(0, 0, 8, 6);
            actions.Children.Add(button);
        }
        Content = Theme.Card(new VerticalStackLayout
        {
            Spacing = 8,
            Children = { _groups, _detail, actions },
        });
    }

    public void Update(MergePromptContract prompt)
    {
        _prompt = prompt;
        var single = prompt.Coverage == "single_detector";
        var targets = _state.Monitoring?.Targets ?? [];
        _groups.Text = string.Join(" ＋ ", prompt.GroupIds.Select(id =>
            targets.FirstOrDefault(target => target.Target == new TargetIdSpec("group", id))?.Name ?? id));
        _detail.Text = single
            ? $"可由 {_state.AccountLabel(prompt.DetectorAccountId ?? "")} 一個帳號偵測。"
            : prompt.Warning ?? $"目前保留 {prompt.DetectorCount} 個偵測帳號；同一活動只會處理一次。";
        _temporary.Text = single ? "暫時合併偵測" : "暫時維持並確認";
        _temporary.IsEnabled = !prompt.Acknowledged &&
                               !_state.IsCommandPending($"merge:{prompt.ComponentId}");
    }
}
