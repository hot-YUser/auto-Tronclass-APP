namespace Ui;

/// <summary>建立、編輯與永久合併同租戶學生群組。</summary>
public sealed class GroupEditorPage : ContentPage
{
    readonly AppState _state;
    readonly ulong _expectedRevision;
    readonly string? _groupId;
    readonly string[] _mergeGroupIds;
    readonly Entry _name = new() { Placeholder = "例如：東海中文課" };
    readonly VerticalStackLayout _membersHost = new() { Spacing = 7 };
    readonly VerticalStackLayout _coursesHost = new() { Spacing = 7 };
    readonly Dictionary<string, CheckBox> _memberChecks = new(StringComparer.Ordinal);
    readonly Dictionary<string, CheckBox> _courseChecks = new(StringComparer.Ordinal);
    readonly Picker _detector = new() { Title = "偵測帳號" };
    readonly Picker _scheduleMode = new()
    {
        Title = "時間表模式",
        ItemsSource = new[] { "停用", "跟隨全局", "自訂" },
    };
    readonly WeeklyScheduleForm _weekly;
    readonly Label _courseStatus = Theme.Dim("選取成員後驗證共同課程；不選課程即不限。", 12);
    readonly Label _quotaWarning = Theme.Text("", 12, Theme.FontRegular, Theme.WarnL, Theme.WarnD);
    string[] _detectorAccountIds = [];
    string? _verifiedMemberKey;
    readonly HashSet<string> _requestedCourseIds;
    string? _preferredDetector;

    public GroupEditorPage(AppState state, TargetSnapshotContract? existing = null)
        : this(state, existing, [])
    {
    }

    public GroupEditorPage(AppState state, string[] mergeGroupIds)
        : this(state, null, mergeGroupIds)
    {
    }

    GroupEditorPage(AppState state, TargetSnapshotContract? existing, string[] mergeGroupIds)
    {
        _state = state;
        var snapshot = state.Monitoring ?? throw new InvalidOperationException("尚未取得 MonitoringSnapshot。");
        _expectedRevision = snapshot.ConfigRevision;
        _groupId = existing?.Target.Kind == "group" ? existing.Target.Id : null;
        _mergeGroupIds = mergeGroupIds;
        Title = _groupId is not null ? "編輯群組" : mergeGroupIds.Length > 0 ? "永久合併群組" : "新增群組";

        var sourceTargets = mergeGroupIds.Length == 0
            ? existing is null ? [] : new[] { existing }
            : snapshot.Targets.Where(target => mergeGroupIds.Contains(target.Target.Id, StringComparer.Ordinal)).ToArray();
        var memberIds = sourceTargets
            .SelectMany(target => target.GroupDefinition?.MemberAccountIds ?? [])
            .Distinct(StringComparer.Ordinal)
            .ToHashSet(StringComparer.Ordinal);
        _requestedCourseIds = sourceTargets
            .SelectMany(target => target.GroupDefinition?.CourseIds ?? [])
            .ToHashSet(StringComparer.Ordinal);
        _preferredDetector = existing?.GroupDefinition?.DetectorSelection is { IsAuto: false } selection
            ? selection.AccountId
            : null;
        _name.Text = existing?.Name ?? "";

        var sourceSchedule = existing?.Schedule ?? ScheduleBindingSpec.Disabled;
        _scheduleMode.SelectedIndex = sourceSchedule.Kind switch
        {
            ScheduleBindingKind.InheritGlobal => 1,
            ScheduleBindingKind.Custom => 2,
            _ => 0,
        };
        _weekly = new WeeklyScheduleForm(sourceSchedule.Weekly);
        _weekly.IsVisible = _scheduleMode.SelectedIndex == 2;
        _scheduleMode.SelectedIndexChanged += (_, _) =>
        {
            _weekly.IsVisible = _scheduleMode.SelectedIndex == 2;
            RefreshQuotaWarning();
        };
        _weekly.Changed += RefreshQuotaWarning;
        _quotaWarning.IsVisible = false;

        foreach (var account in snapshot.Accounts.Where(account => account.Role == "student"))
        {
            var check = new CheckBox { IsChecked = memberIds.Contains(account.AccountId) };
            check.CheckedChanged += (_, _) =>
            {
                _verifiedMemberKey = null;
                _courseChecks.Clear();
                _coursesHost.Children.Clear();
                _courseStatus.Text = "成員已改變，請重新驗證共同課程；不選課程即不限。";
                RefreshMemberAvailability();
                RefreshDetector();
            };
            _memberChecks[account.AccountId] = check;
            var label = Theme.Body($"{account.Label} · {account.SchoolRef}");
            label.VerticalOptions = LayoutOptions.Center;
            var row = new Grid { ColumnSpacing = 10 };
            row.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
            row.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
            row.Add(check, 0, 0);
            row.Add(label, 1, 0);
            _membersHost.Children.Add(row);
        }

        var verifyCourses = Theme.Ghost("驗證共同課程", VerifyCourses);
        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    Theme.Section("群組"),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 10,
                        Children = { _name, _membersHost, _detector },
                    }),
                    Theme.Section("共同課程"),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 8,
                        Children = { _courseStatus, verifyCourses, _coursesHost },
                    }),
                    Theme.Section("時間表"),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 8,
                        Children = { _scheduleMode, _weekly, _quotaWarning },
                    }),
                    Theme.Primary(_groupId is null ? "建立群組" : "儲存群組", Save),
                },
            },
        };
        RefreshMemberAvailability();
        RefreshDetector();
        RefreshQuotaWarning();
    }

    async Task VerifyCourses()
    {
        var memberIds = SelectedMembers();
        if (memberIds.Length == 0)
        {
            _state.Notify("error", "請先選取成員。");
            return;
        }
        var courses = await _state.ListCommonCourses(memberIds);
        if (courses is null)
        {
            _verifiedMemberKey = null;
            _courseStatus.Text = "無法驗證所有成員課程；只能清空課程並以不限課程儲存。";
            return;
        }
        _verifiedMemberKey = MemberKey(memberIds);
        _courseChecks.Clear();
        _coursesHost.Children.Clear();
        foreach (var course in courses)
        {
            var check = new CheckBox { IsChecked = _requestedCourseIds.Contains(course.CourseId) };
            _courseChecks[course.CourseId] = check;
            var row = new Grid { ColumnSpacing = 10 };
            row.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
            row.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
            row.Add(check, 0, 0);
            row.Add(Centered(Theme.Body(course.Name)), 1, 0);
            _coursesHost.Children.Add(row);
        }
        var removed = _requestedCourseIds.Where(id => courses.All(course => course.CourseId != id)).ToArray();
        _courseStatus.Text = removed.Length == 0
            ? courses.Length == 0 ? "全員沒有共同課程；保留空選取即不限課程。" : "已驗證；不選任何課程即不限課程。"
            : $"下列原課程已不再共同並取消選取：{string.Join("、", removed)}。";
    }

    async Task Save()
    {
        var name = _name.Text?.Trim() ?? "";
        var members = SelectedMembers();
        var minimum = _groupId is null ? 2 : 1;
        if (name.Length == 0)
        {
            _state.Notify("error", "群組名稱不得為空。");
            return;
        }
        if (members.Length < minimum)
        {
            _state.Notify("error", _groupId is null ? "建立群組至少需要兩名學生。" : "群組至少需要一名學生。");
            return;
        }
        var courseIds = _courseChecks.Where(pair => pair.Value.IsChecked).Select(pair => pair.Key).ToArray();
        if (courseIds.Length > 0 && _verifiedMemberKey != MemberKey(members))
        {
            _state.Notify("error", "成員已改變；綁定課程前必須重新驗證共同課程。");
            return;
        }

        ScheduleBindingSpec schedule;
        try
        {
            schedule = _scheduleMode.SelectedIndex switch
            {
                0 => ScheduleBindingSpec.Disabled,
                1 => ScheduleBindingSpec.InheritGlobal,
                2 => ScheduleBindingSpec.Custom(_weekly.Read()),
                _ => throw new FormatException("請選擇時間表模式。"),
            };
        }
        catch (FormatException error)
        {
            _state.Notify("error", error.Message);
            return;
        }

        var detector = _detector.SelectedIndex <= 0
            ? DetectorSelectionSpec.Auto
            : DetectorSelectionSpec.Preferred(_detectorAccountIds[_detector.SelectedIndex - 1]);
        var input = new GroupInputWire(name, members, courseIds, detector, schedule);
        bool saved;
        if (_mergeGroupIds.Length > 0)
            saved = await _state.MergeGroups(_mergeGroupIds, _expectedRevision, input);
        else if (_groupId is not null)
            saved = await _state.UpdateGroup(_groupId, _expectedRevision, input);
        else
            saved = await _state.CreateGroup(_expectedRevision, input);
        if (saved) await Navigation.PopAsync();
    }

    void RefreshMemberAvailability()
    {
        var snapshot = _state.Monitoring;
        if (snapshot is null) return;
        var selected = SelectedMembers();
        var tenantSchool = selected.Length == 0
            ? null
            : snapshot.Accounts.First(account => account.AccountId == selected[0]).SchoolRef;
        foreach (var account in snapshot.Accounts.Where(account => account.Role == "student"))
            _memberChecks[account.AccountId].IsEnabled =
                _memberChecks[account.AccountId].IsChecked || tenantSchool is null || account.SchoolRef == tenantSchool;
    }

    void RefreshDetector()
    {
        var snapshot = _state.Monitoring;
        if (snapshot is null) return;
        var previous = _detector.SelectedIndex > 0 && _detector.SelectedIndex - 1 < _detectorAccountIds.Length
            ? _detectorAccountIds[_detector.SelectedIndex - 1]
            : _preferredDetector;
        _detectorAccountIds = SelectedMembers();
        _detector.ItemsSource = new[] { "自動輪替" }
            .Concat(_detectorAccountIds.Select(id => $"偏好：{snapshot.Accounts.First(account => account.AccountId == id).Label}"))
            .ToArray();
        var preferredIndex = previous is null ? -1 : Array.IndexOf(_detectorAccountIds, previous);
        _detector.SelectedIndex = preferredIndex < 0 ? 0 : preferredIndex + 1;
    }

    void RefreshQuotaWarning()
    {
        try
        {
            var schedule = _scheduleMode.SelectedIndex switch
            {
                1 => _state.Monitoring?.GlobalSchedule ?? new WeeklyScheduleSpec(),
                2 => _weekly.Read(),
                _ => new WeeklyScheduleSpec(),
            };
            _quotaWarning.Text = ScheduleAnalysis.ExceedsRollingSixHours(schedule)
                ? "Android 任一 rolling 24 小時可能超過 6 小時 dataSync 配額；平台可能暫停新偵測。"
                : "";
        }
        catch (FormatException error)
        {
            _quotaWarning.Text = error.Message;
        }
        _quotaWarning.IsVisible = _quotaWarning.Text.Length > 0;
    }

    string[] SelectedMembers() => _memberChecks
        .Where(pair => pair.Value.IsChecked)
        .Select(pair => pair.Key)
        .ToArray();

    static string MemberKey(IEnumerable<string> members) =>
        string.Join("\u001f", members.Order(StringComparer.Ordinal));

    static Label Centered(Label label)
    {
        label.VerticalOptions = LayoutOptions.Center;
        return label;
    }
}
