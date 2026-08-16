using System.Collections.Specialized;
using System.ComponentModel;

namespace Ui;

/// <summary>個人與群組 target 的唯一監控控制台。</summary>
public sealed class HomePage : ContentPage
{
    readonly AppState _state;
    readonly Label _session = Theme.Strong("等待核心…", 17);
    readonly Label _globalReason = Theme.Text("", 12.5, Theme.FontRegular, Theme.DangerL, Theme.DangerD);
    readonly Label _platform = Theme.Dim("", 12);
    readonly Button _primary;
    readonly VerticalStackLayout _personal = new() { Spacing = 10 };
    readonly VerticalStackLayout _groups = new() { Spacing = 10 };
    readonly VerticalStackLayout _mergePrompts = new() { Spacing = 10 };
    readonly VerticalStackLayout _feed = new() { Spacing = 8 };
    readonly ContentView _nextClassHost = new() { IsVisible = false };
    bool _attached;

    public HomePage(AppState state)
    {
        _state = state;
        Title = "監控";
        _globalReason.IsVisible = false;
        _primary = Theme.Primary("", RunPrimaryAction);

        var monitorCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 10,
            Children =
            {
                _session,
                Theme.Dim("排程與個別手動動作由核心逐 target 執行；已取得 mutation 許可的工作會安全收尾。", 13),
                _globalReason,
                _primary,
                _platform,
            },
        });

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
                    monitorCard,
                    Theme.Section("個人帳號"),
                    _personal,
                    new HorizontalStackLayout
                    {
                        Children =
                        {
                            Theme.Primary(
                                "＋ 新增群組",
                                () => Navigation.PushAsync(new GroupEditorPage(state))),
                        },
                    },
                    Theme.Section("群組"),
                    _groups,
                    Theme.Section("群組重疊"),
                    _mergePrompts,
                    Theme.Section("近期活動"),
                    _feed,
                },
            },
        };
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

    void RenderMonitoring()
    {
        var snapshot = _state.Monitoring;
        _personal.Children.Clear();
        _groups.Children.Clear();
        _mergePrompts.Children.Clear();
        if (snapshot is null)
        {
            _session.Text = "等待核心…";
            _primary.IsVisible = false;
            _personal.Children.Add(Theme.Dim("尚未取得監控快照。", 13));
            _groups.Children.Add(Theme.Dim("尚未取得監控快照。", 13));
            _mergePrompts.Children.Add(Theme.Dim("尚無重疊群組。", 13));
            return;
        }

        _session.Text = SessionText(snapshot.SessionState);
        _globalReason.Text = snapshot.GlobalDisabledReason ?? "";
        _globalReason.IsVisible = snapshot.GlobalDisabledReason is not null;
        _platform.Text = WakeText(snapshot.WakeMode);

        var studentTargetCount = snapshot.Targets.Count(target => target.Target.Kind == "account");
        _primary.IsVisible = snapshot.CanStopAll || snapshot.CanResume;
        _primary.Text = snapshot.CanStopAll ? "一鍵停止全部" : "恢復照時間表";
        _primary.IsEnabled =
            studentTargetCount > 0 &&
            snapshot.GlobalDisabledReason is null &&
            !_state.IsCommandPending("monitoring:all");

        var personalTargets = snapshot.Targets
            .Where(target => target.Target.Kind == "account")
            .ToArray();
        if (personalTargets.Length == 0)
            _personal.Children.Add(Theme.Dim("尚無學生帳號；請先到「帳號」新增並驗證。", 13));
        else
            foreach (var target in personalTargets)
                _personal.Children.Add(BuildTargetCard(snapshot, target));

        var groupTargets = snapshot.Targets
            .Where(target => target.Target.Kind == "group")
            .ToArray();
        if (groupTargets.Length == 0)
            _groups.Children.Add(Theme.Dim("尚未建立群組。", 13));
        else
            foreach (var target in groupTargets)
                _groups.Children.Add(BuildTargetCard(snapshot, target));

        if (snapshot.MergePrompts.Length == 0)
            _mergePrompts.Children.Add(Theme.Dim("目前沒有重疊的啟用群組。", 13));
        else
            foreach (var prompt in snapshot.MergePrompts)
                _mergePrompts.Children.Add(BuildMergePrompt(snapshot, prompt));
    }

    async Task RunPrimaryAction()
    {
        var snapshot = _state.Monitoring;
        if (snapshot is null) return;
        if (snapshot.CanStopAll) await _state.StopAllMonitoring();
        else if (snapshot.CanResume) await _state.ResumeScheduledMonitoring();
    }

    View BuildTargetCard(MonitoringSnapshotContract snapshot, TargetSnapshotContract target)
    {
        var title = new HorizontalStackLayout { Spacing = 8 };
        title.Children.Add(Theme.Strong(target.Name, 15));
        title.Children.Add(StatePill(target.RuntimeState));

        var body = new VerticalStackLayout
        {
            Spacing = 7,
            Children =
            {
                title,
                Theme.Dim(ScheduleText(target), 12.5),
            },
        };
        if (target.DisabledReason is { Length: > 0 } reason)
            body.Children.Add(Theme.Text(
                reason,
                12.5,
                Theme.FontSemibold,
                target.RuntimeState == "suppressed_by_group" ? Theme.WarnL : Theme.DangerL,
                target.RuntimeState == "suppressed_by_group" ? Theme.WarnD : Theme.DangerD));
        if (target.RuntimeState == "suppressed_by_group")
            body.Children.Add(Theme.Dim("個人調整會在群組結束後生效。", 12));
        if (target.ManualOverride is { } manual)
            body.Children.Add(Theme.Dim(
                manual.ExpiresAtUtc is null
                    ? $"手動{(manual.ForceOpen ? "開啟" : "停止")} · App 結束即失效"
                    : $"手動{(manual.ForceOpen ? "開啟" : "停止")}至 {LocalTime(manual.ExpiresAtUtc)}",
                12));
        if (target.Detector is { } detector)
            body.Children.Add(Theme.Dim(
                $"偵測帳號：{_state.AccountLabel(detector.AccountId)}{(detector.IsFallback ? "（fallback）" : "")}",
                12.5));
        if (target.GroupDefinition is { } group)
        {
            body.Children.Add(Theme.Dim(
                $"成員：{string.Join("、", group.MemberAccountIds.Select(_state.AccountLabel))}",
                12.5));
            body.Children.Add(Theme.Dim(
                target.Courses.Length == 0
                    ? "課程：不限課程"
                    : $"課程：{string.Join("、", target.Courses.Select(course => course.Name))}",
                12.5));
        }
        foreach (var result in target.AccountResults
                     .Where(result => result.Phase != "idle")
                     .OrderByDescending(result => result.UpdatedAtUtc)
                     .Take(3))
            body.Children.Add(ResultRow(result));
        if (target.Error is { } error)
            body.Children.Add(Theme.Text(error.Message, 12.5, Theme.FontRegular, Theme.DangerL, Theme.DangerD));

        var commands = new FlexLayout { Wrap = Microsoft.Maui.Layouts.FlexWrap.Wrap };
        var targetKey = AppState.TargetCommandKey(target.Target);
        var pending = _state.IsCommandPending(targetKey);
        var start = Theme.Ghost("立即開始", () => _state.StartTarget(target.Target));
        start.IsEnabled = target.CanStart && !pending;
        AddCommand(commands, start);
        var stop = Theme.Ghost("立即停止", () => _state.StopTarget(target.Target));
        stop.IsEnabled = target.CanStop && !pending;
        AddCommand(commands, stop);
        var schedule = Theme.Ghost(
            "時間表",
            () => Navigation.PushAsync(new ScheduleEditorPage(_state, target)));
        schedule.IsEnabled = target.CanEditSchedule &&
                             !_state.IsCommandPending($"{targetKey}:schedule");
        AddCommand(commands, schedule);
        if (target.Target.Kind == "group")
        {
            var edit = Theme.Ghost(
                "編輯群組",
                () => Navigation.PushAsync(new GroupEditorPage(_state, target)));
            edit.IsEnabled = !_state.IsCommandPending($"group:{target.Target.Id}:update");
            AddCommand(commands, edit);
            var delete = Theme.Danger("刪除群組", () => DeleteGroup(snapshot, target));
            delete.IsEnabled = !_state.IsCommandPending($"group:{target.Target.Id}:delete");
            AddCommand(commands, delete);
        }
        body.Children.Add(commands);
        return Theme.Card(body);
    }

    async Task DeleteGroup(MonitoringSnapshotContract snapshot, TargetSnapshotContract target)
    {
        if (await DisplayAlertAsync(
                "刪除群組",
                $"確定刪除「{target.Name}」？尚未取得 mutation 許可的工作會取消。",
                "刪除",
                "取消"))
            await _state.DeleteGroup(target.Target.Id, snapshot.ConfigRevision);
    }

    View BuildMergePrompt(MonitoringSnapshotContract snapshot, MergePromptContract prompt)
    {
        var groupNames = prompt.GroupIds
            .Select(id => snapshot.Targets.FirstOrDefault(
                target => target.Target == new TargetIdSpec("group", id))?.Name ?? id);
        var detail = prompt.Coverage == "single_detector"
            ? $"可由 {_state.AccountLabel(prompt.DetectorAccountId ?? "")} 單一偵測。"
            : prompt.Warning ?? $"目前保留 {prompt.DetectorCount} 支偵測帳號，execution 仍會去重。";
        var body = new VerticalStackLayout
        {
            Spacing = 8,
            Children =
            {
                Theme.Strong(string.Join(" ＋ ", groupNames), 14),
                Theme.Dim(detail, 12.5),
            },
        };
        var actions = new FlexLayout { Wrap = Microsoft.Maui.Layouts.FlexWrap.Wrap };
        var temporary = Theme.Ghost(
            prompt.Coverage == "single_detector" ? "暫時合併偵測" : "暫時維持並確認",
            () => _state.AcknowledgeTemporaryMerge(prompt.ComponentId, snapshot.PlanRevision));
        temporary.IsEnabled = !prompt.Acknowledged &&
                              !_state.IsCommandPending($"merge:{prompt.ComponentId}");
        AddCommand(actions, temporary);
        AddCommand(actions, Theme.Primary(
            "永久合併",
            () => Navigation.PushAsync(new GroupEditorPage(_state, prompt.GroupIds))));
        body.Children.Add(actions);
        return Theme.Card(body);
    }

    static void AddCommand(FlexLayout host, Button button)
    {
        button.Margin = new Thickness(0, 0, 8, 6);
        host.Children.Add(button);
    }

    static View StatePill(string state)
    {
        var (text, light, dark, backgroundLight, backgroundDark) = state switch
        {
            "monitoring" => ("監控中", Theme.OkL, Theme.OkD, Theme.OkBgL, Theme.OkBgD),
            "starting" => ("啟動中", Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD),
            "stopping" => ("停止中", Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD),
            "suppressed_by_group" => ("已在群組中監控", Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD),
            "platform_blocked" => ("平台暫停", Theme.DangerL, Theme.DangerD, Theme.DangerBgL, Theme.DangerBgD),
            "error" => ("錯誤", Theme.DangerL, Theme.DangerD, Theme.DangerBgL, Theme.DangerBgD),
            "manual_off" => ("手動停止", Theme.DimL, Theme.DimD, Theme.Card2L, Theme.Card2D),
            _ => ("排程關閉", Theme.DimL, Theme.DimD, Theme.Card2L, Theme.Card2D),
        };
        return Theme.TextPill(text, light, dark, backgroundLight, backgroundDark);
    }

    static string SessionText(string state) => state switch
    {
        "running" => "監控工作執行中",
        "starting" => "正在準備監控",
        "stopping" => "正在停止監控",
        "platform_blocked" => "平台限制已暫停新偵測",
        "error" => "監控核心發生錯誤",
        _ => "等待排程或手動開始",
    };

    static string WakeText(string mode) => mode switch
    {
        "exact" => "Android：已允許精準喚醒。Force-stop 後需重新開啟 App。",
        "inexact_user_action_required" => "Android：未允許精準鬧鐘；邊界只會通知，需點擊才能開始。",
        "unavailable" => "此平台無法在 App 關閉後自動喚醒。",
        _ => "Windows：App 存活時才會依時間表執行。",
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
            : $"{binding} · 下一邊界 {LocalTime(target.NextBoundaryUtc)}";
    }

    static string LocalTime(string utc) =>
        DateTimeOffset.TryParse(utc, out var instant)
            ? instant.ToLocalTime().ToString("MM/dd HH:mm")
            : utc;

    View ResultRow(AccountResultContract result)
    {
        var detail = result.Error?.Message ??
                     string.Join(" · ", new[] { result.ActivityKind, result.CourseName }
                         .Where(value => !string.IsNullOrWhiteSpace(value)));
        return Theme.Dim(
            $"{_state.AccountLabel(result.AccountId)}：{ResultText(result.Phase)}" +
            (detail.Length == 0 ? "" : $" · {detail}"),
            12);
    }

    static string ResultText(string phase) => phase switch
    {
        "pending" => "準備中",
        "authorized" => "送出中",
        "succeeded" => "成功",
        "failed" => "失敗",
        "unknown_after_restart" => "重啟後結果不明",
        _ => "閒置",
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
        if (items.Length == 0)
        {
            _feed.Children.Add(Theme.Dim("尚無活動。", 13));
            return;
        }
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
