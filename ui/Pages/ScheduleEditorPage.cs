namespace Ui;

/// <summary>Target 的停用／全局／自訂時間表編輯器。</summary>
public sealed class ScheduleEditorPage : ContentPage
{
    readonly AppState _state;
    readonly TargetSnapshotContract _target;
    readonly Picker _binding = new()
    {
        Title = "時間表模式",
        ItemsSource = new[] { "停用", "跟隨全局", "自訂" },
    };
    readonly WeeklyScheduleForm _weekly;
    readonly Label _overlap = Theme.Text("", 12.5, Theme.FontRegular, Theme.WarnL, Theme.WarnD);
    readonly Label _quota = Theme.Text("", 12.5, Theme.FontRegular, Theme.WarnL, Theme.WarnD);

    public ScheduleEditorPage(AppState state, TargetSnapshotContract target)
    {
        _state = state;
        _target = target;
        Title = $"時間表 · {target.Name}";
        _binding.SelectedIndex = target.Schedule.Kind switch
        {
            ScheduleBindingKind.InheritGlobal => 1,
            ScheduleBindingKind.Custom => 2,
            _ => 0,
        };
        _weekly = new WeeklyScheduleForm(target.Schedule.Weekly);
        _weekly.IsVisible = _binding.SelectedIndex == 2;
        _binding.SelectedIndexChanged += (_, _) =>
        {
            _weekly.IsVisible = _binding.SelectedIndex == 2;
            RefreshWarnings();
        };
        _weekly.Changed += RefreshWarnings;
        _overlap.IsVisible = false;
        _quota.IsVisible = false;

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 10,
                        Children =
                        {
                            _binding,
                            Theme.Dim("區間為半開；結束早於開始代表跨午夜。可輸入多段，例如 08:00-12:00；13:00-17:00。", 12),
                            _weekly,
                            _overlap,
                            _quota,
                        },
                    }),
                    Theme.Primary("儲存時間表", Save),
                },
            },
        };
        RefreshWarnings();
    }

    async Task Save()
    {
        ScheduleBindingSpec schedule;
        try
        {
            schedule = ReadBinding();
        }
        catch (FormatException error)
        {
            _state.Notify("error", error.Message);
            return;
        }
        var revision = _state.Monitoring?.ConfigRevision;
        if (revision is null)
        {
            _state.Notify("error", "尚未取得設定 revision。");
            return;
        }
        if (await _state.SetTargetSchedule(_target.Target, revision.Value, schedule))
            await Navigation.PopAsync();
    }

    ScheduleBindingSpec ReadBinding() => _binding.SelectedIndex switch
    {
        0 => ScheduleBindingSpec.Disabled,
        1 => ScheduleBindingSpec.InheritGlobal,
        2 => ScheduleBindingSpec.Custom(_weekly.Read()),
        _ => throw new FormatException("請選擇時間表模式。"),
    };

    void RefreshWarnings()
    {
        ScheduleBindingSpec candidate;
        try
        {
            candidate = ReadBinding();
        }
        catch (FormatException error)
        {
            _overlap.Text = error.Message;
            _overlap.IsVisible = true;
            _quota.IsVisible = false;
            return;
        }
        var snapshot = _state.Monitoring;
        if (snapshot is null) return;
        var overlaps = ScheduleAnalysis.Overlaps(snapshot, _target, candidate);
        _overlap.Text = overlaps.Length == 0
            ? ""
            : $"時間重疊：{string.Join("、", overlaps)}。個人重疊會由群組 suppress；群組重疊會建立合併提示。";
        _overlap.IsVisible = overlaps.Length > 0;
        var weekly = ScheduleAnalysis.Resolve(candidate, snapshot.GlobalSchedule);
        _quota.Text = ScheduleAnalysis.ExceedsRollingSixHours(weekly)
            ? "Android dataSync 前景服務任一 rolling 24 小時可能超過 6 小時；平台可能暫停新偵測，無法繞過此配額。"
            : "";
        _quota.IsVisible = _quota.Text.Length > 0;
    }
}

/// <summary>七日、多區間、跨午夜的無反射表單。</summary>
public sealed class WeeklyScheduleForm : VerticalStackLayout
{
    static readonly string[] DayNames = ["週一", "週二", "週三", "週四", "週五", "週六", "週日"];
    readonly Entry[] _entries = new Entry[7];

    public event Action? Changed;

    public WeeklyScheduleForm(WeeklyScheduleSpec? schedule = null)
    {
        Spacing = 8;
        schedule ??= new WeeklyScheduleSpec();
        for (var day = 0; day < 7; day++)
        {
            var entry = new Entry
            {
                Placeholder = "08:00-12:00；13:00-17:00",
                Text = Format(schedule.Day(day)),
                ClearButtonVisibility = ClearButtonVisibility.WhileEditing,
            };
            entry.TextChanged += (_, _) => Changed?.Invoke();
            _entries[day] = entry;
            var row = new Grid { ColumnSpacing = 10 };
            row.ColumnDefinitions.Add(new ColumnDefinition(new GridLength(52)));
            row.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
            var label = Theme.Body(DayNames[day]);
            label.VerticalOptions = LayoutOptions.Center;
            row.Add(label, 0, 0);
            row.Add(entry, 1, 0);
            Children.Add(row);
        }
    }

    public WeeklyScheduleSpec Read()
    {
        var days = _entries.Select((entry, day) => ParseDay(entry.Text, DayNames[day])).ToArray();
        var schedule = new WeeklyScheduleSpec(
            days[0], days[1], days[2], days[3], days[4], days[5], days[6]);
        schedule.Validate();
        return schedule;
    }

    static TimeWindowSpec[] ParseDay(string? text, string dayName)
    {
        if (string.IsNullOrWhiteSpace(text)) return [];
        var chunks = text.Split([';', '；', ','], StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries);
        var windows = new TimeWindowSpec[chunks.Length];
        for (var index = 0; index < chunks.Length; index++)
        {
            var separator = chunks[index].IndexOf('-');
            if (separator <= 0 || separator == chunks[index].Length - 1)
                throw new FormatException($"{dayName}第 {index + 1} 段格式應為 HH:mm-HH:mm。");
            var start = ParseMinute(chunks[index][..separator], allow2400: false, dayName);
            var end = ParseMinute(chunks[index][(separator + 1)..], allow2400: true, dayName);
            windows[index] = new(start, end);
        }
        return windows;
    }

    static int ParseMinute(string text, bool allow2400, string dayName)
    {
        var parts = text.Trim().Split(':');
        if (parts.Length != 2 || !int.TryParse(parts[0], out var hour) || !int.TryParse(parts[1], out var minute) ||
            minute is < 0 or > 59 || hour < 0 || hour > 24 || (hour == 24 && (!allow2400 || minute != 0)))
            throw new FormatException($"{dayName}含無效時間「{text.Trim()}」。");
        return hour * 60 + minute;
    }

    static string Format(ReadOnlySpan<TimeWindowSpec> windows) => string.Join(
        "；",
        windows.ToArray().Select(window => $"{MinuteText(window.StartMinute)}-{MinuteText(window.EndMinute)}"));

    static string MinuteText(int minute) => minute == 1440 ? "24:00" : $"{minute / 60:00}:{minute % 60:00}";
}

static class ScheduleAnalysis
{
    public static string[] Overlaps(
        MonitoringSnapshotContract snapshot,
        TargetSnapshotContract current,
        ScheduleBindingSpec candidate)
    {
        var currentIntervals = Intervals(Resolve(candidate, snapshot.GlobalSchedule));
        if (currentIntervals.Count == 0) return [];
        return snapshot.Targets
            .Where(target => target.Target != current.Target &&
                             (current.Target.Kind == "group" || target.Target.Kind == "group"))
            .Where(target => HasOverlap(
                currentIntervals,
                Intervals(Resolve(target.Schedule, snapshot.GlobalSchedule))))
            .Select(target => target.Name)
            .ToArray();
    }

    public static WeeklyScheduleSpec Resolve(ScheduleBindingSpec binding, WeeklyScheduleSpec global) => binding.Kind switch
    {
        ScheduleBindingKind.Custom => binding.Weekly ?? new WeeklyScheduleSpec(),
        ScheduleBindingKind.InheritGlobal => global,
        _ => new WeeklyScheduleSpec(),
    };

    public static bool ExceedsRollingSixHours(WeeklyScheduleSpec schedule)
    {
        var baseIntervals = Intervals(schedule);
        if (baseIntervals.Count == 0) return false;
        var repeated = baseIntervals
            .SelectMany(interval => new (int Start, int End)[]
            {
                interval,
                (interval.Start + 10080, interval.End + 10080),
            })
            .ToArray();
        var candidateStarts = repeated
            .SelectMany(interval => new[]
            {
                interval.Start,
                interval.End,
                interval.Start - 1440,
                interval.End - 1440,
            })
            .Where(value => value is >= 0 and < 10080)
            .Distinct();
        foreach (var start in candidateStarts)
        {
            var end = start + 1440;
            var covered = repeated.Sum(interval =>
                Math.Max(0, Math.Min(end, interval.End) - Math.Max(start, interval.Start)));
            if (covered > 360) return true;
        }
        return false;
    }

    static bool HasOverlap(List<(int Start, int End)> left, List<(int Start, int End)> right) =>
        left.Any(a => right.Any(b => a.Start < b.End && b.Start < a.End));

    static List<(int Start, int End)> Intervals(WeeklyScheduleSpec schedule)
    {
        var intervals = new List<(int Start, int End)>();
        for (var day = 0; day < 7; day++)
            foreach (var window in schedule.Day(day))
            {
                var start = day * 1440 + window.StartMinute;
                var end = day * 1440 + window.EndMinute;
                if (window.StartMinute > window.EndMinute) end += 1440;
                if (end <= 10080) intervals.Add((start, end));
                else
                {
                    intervals.Add((start, 10080));
                    intervals.Add((0, end - 10080));
                }
            }
        return intervals;
    }
}
