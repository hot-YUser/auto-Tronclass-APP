namespace Ui;

/// <summary>全局時間表與 Device／Named IANA 時區設定。</summary>
public sealed class MonitoringPreferencesPage : ContentPage
{
    readonly AppState _state;
    readonly ulong _expectedRevision;
    readonly Picker _zoneMode = new()
    {
        Title = "時區來源",
        ItemsSource = new[] { "跟隨裝置", "指定 IANA 時區" },
    };
    readonly Entry _iana = new() { Placeholder = "Asia/Taipei" };
    readonly WeeklyScheduleForm _weekly;
    readonly Label _zoneError = Theme.Text("", 12.5, Theme.FontRegular, Theme.DangerL, Theme.DangerD);
    readonly Label _quota = Theme.Text("", 12.5, Theme.FontRegular, Theme.WarnL, Theme.WarnD);

    public MonitoringPreferencesPage(AppState state)
    {
        _state = state;
        var snapshot = state.Monitoring ?? throw new InvalidOperationException("尚未取得 MonitoringSnapshot。");
        _expectedRevision = snapshot.ConfigRevision;
        Title = "全局時間表與時區";
        _zoneMode.SelectedIndex = snapshot.TimeZone.IsDevice ? 0 : 1;
        _iana.Text = snapshot.TimeZone.IanaId ?? "Asia/Taipei";
        _iana.IsVisible = _zoneMode.SelectedIndex == 1;
        _weekly = new WeeklyScheduleForm(snapshot.GlobalSchedule);
        _zoneError.IsVisible = false;
        _quota.IsVisible = false;
        _zoneMode.SelectedIndexChanged += (_, _) =>
        {
            _iana.IsVisible = _zoneMode.SelectedIndex == 1;
            ValidateZone();
        };
        _iana.TextChanged += (_, _) => ValidateZone();
        _weekly.Changed += RefreshQuota;

        var platformRows = new VerticalStackLayout { Spacing = 8 };
        platformRows.Children.Add(Theme.Dim(
            "Force-stop 後 Android 會停用鬧鐘與 receiver，必須重新開啟 App；Windows 不會開機自啟。",
            12));
#if ANDROID
        var exactStatus = Theme.Dim("", 12.5);
        var exactButton = Theme.Ghost(
            "管理精準鬧鐘權限",
            () =>
            {
                AndroidScheduleAlarms.OpenExactAlarmSettings(
                    global::Android.App.Application.Context);
                return Task.CompletedTask;
            });
        void RefreshExactAccess()
        {
            var exact = AndroidScheduleAlarms.CanScheduleExact(
                global::Android.App.Application.Context);
            exactStatus.Text = exact
                ? "已允許精準鬧鐘：未來 boundary 可自動啟動。"
                : "未允許精準鬧鐘：inexact alarm 只會通知，需點擊才能開始。";
        }
        platformRows.Children.Add(exactStatus);
        platformRows.Children.Add(exactButton);
        Appearing += (_, _) => RefreshExactAccess();
        RefreshExactAccess();
#endif

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    Theme.Section("時區"),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 8,
                        Children =
                        {
                            _zoneMode,
                            _iana,
                            Theme.Dim("具名時區固定儲存 IANA ID；無法解析時 automatic target 會 fail closed，不會改用固定 offset。", 12),
                            _zoneError,
                        },
                    }),
                    Theme.Section("平台喚醒"),
                    Theme.Card(platformRows),
                    Theme.Section("每週預設時間表"),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 8,
                        Children =
                        {
                            Theme.Dim("選擇「跟隨全局」的 target 會使用此表；空表表示沒有自動時段。", 12),
                            _weekly,
                            _quota,
                        },
                    }),
                    Theme.Primary("儲存全局排程", Save),
                },
            },
        };
        ValidateZone();
        RefreshQuota();
    }

    async Task Save()
    {
        WeeklyScheduleSpec weekly;
        try
        {
            weekly = _weekly.Read();
        }
        catch (FormatException error)
        {
            _state.Notify("error", error.Message);
            return;
        }
        var zone = ReadZone();
        if (zone is null) return;
        if (await _state.SaveMonitoringPreferences(_expectedRevision, weekly, zone))
            await Navigation.PopAsync();
    }

    TimeZoneSpec? ReadZone()
    {
        if (_zoneMode.SelectedIndex == 0) return TimeZoneSpec.Device;
        var input = _iana.Text?.Trim() ?? "";
        if (input.Length == 0)
        {
            ShowZoneError("請輸入 IANA 時區 ID。");
            return null;
        }
        var normalized = TimeZoneInfo.TryConvertWindowsIdToIanaId(input, out var iana) && iana is not null
            ? iana
            : input;
        try
        {
            _ = TimeZoneInfo.FindSystemTimeZoneById(normalized);
            _iana.Text = normalized;
            ShowZoneError(null);
            return TimeZoneSpec.Named(normalized);
        }
        catch (TimeZoneNotFoundException)
        {
            ShowZoneError($"找不到時區「{input}」。請輸入例如 Asia/Taipei。");
            return null;
        }
        catch (InvalidTimeZoneException)
        {
            ShowZoneError($"時區「{input}」資料無效。");
            return null;
        }
    }

    void ValidateZone()
    {
        if (_zoneMode.SelectedIndex == 0)
        {
            ShowZoneError(null);
            return;
        }
        _ = ReadZone();
    }

    void ShowZoneError(string? message)
    {
        _zoneError.Text = message ?? "";
        _zoneError.IsVisible = message is not null;
    }

    void RefreshQuota()
    {
        try
        {
            _quota.Text = ScheduleAnalysis.ExceedsRollingSixHours(_weekly.Read())
                ? "Android 任一 rolling 24 小時可能超過 6 小時 dataSync 配額；平台可能暫停新偵測。"
                : "";
        }
        catch (FormatException error)
        {
            _quota.Text = error.Message;
        }
        _quota.IsVisible = _quota.Text.Length > 0;
    }
}
