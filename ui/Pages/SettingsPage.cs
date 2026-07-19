namespace Ui;

/// <summary>設定(帳號分頁入口):監控參數、LLM 金鑰、核心能力、日誌。</summary>
public sealed class SettingsPage : ContentPage
{
    readonly AppState _state;
    readonly VerticalStackLayout _capsRows = new() { Spacing = 8 };

    public SettingsPage(AppState state)
    {
        _state = state;
        Title = "設定";

        // --- 監控參數 ---
        var countdown = new Entry { Text = "15", Keyboard = Keyboard.Numeric, WidthRequest = 72, HorizontalTextAlignment = TextAlignment.End };
        var threshold = new Entry { Text = "15", Keyboard = Keyboard.Numeric, WidthRequest = 72, HorizontalTextAlignment = TextAlignment.End };
        var thresholdOn = new Switch { IsToggled = true };

        var monitorCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 10,
            Children =
            {
                SettingRow("倒數秒數", "自動簽到/送出前的反悔窗", countdown),
                Theme.Divider(),
                SettingRow("防假點名門檻(%)", "全班簽到率低於此值不出手", threshold),
                Theme.Divider(),
                SettingRow("啟用門檻", "關閉後任何點名都會處理", thresholdOn),
                Theme.Primary("儲存設定", async () =>
                {
                    if (!int.TryParse(countdown.Text, out var secs) || secs < 0 ||
                        !double.TryParse(threshold.Text, out var pct) || pct is < 0 or > 100)
                    {
                        state.Notify("error", "數值格式不正確");
                        return;
                    }
                    if (await state.SaveConfig(secs, pct, thresholdOn.IsToggled))
                        state.Notify("info", "設定已儲存");
                }),
            },
        });

        // --- LLM ---
        var llmKey = new Entry { Placeholder = "API 金鑰", IsPassword = true };
        var llmCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 10,
            Children =
            {
                Theme.Dim("答題由 LLM 產生答案;金鑰加密存於保險庫。", 12.5),
                llmKey,
                Theme.Primary("儲存金鑰", async () =>
                {
                    var key = llmKey.Text?.Trim() ?? "";
                    if (key.Length == 0) { state.Notify("error", "請輸入金鑰"); return; }
                    if (await state.SetLlmKey(key))
                    {
                        llmKey.Text = "";
                        state.Notify("info", "金鑰已儲存");
                    }
                }),
            },
        });

        // --- 能力(core 判定,UI 只呈現) ---
        state.Caps.PropertyChanged += (_, _) => BuildCaps();
        BuildCaps();

        // --- 日誌 ---
        var logs = new VerticalStackLayout { Spacing = 4 };
        BindableLayout.SetItemsSource(logs, state.Logs);
        BindableLayout.SetItemTemplate(logs, new DataTemplate(() =>
        {
            var l = Theme.Dim("", 11);
            l.SetBinding(Label.TextProperty, nameof(LogEntry.Display));
            return l;
        }));
        var logEmpty = Theme.Dim("尚無日誌。", 12);
        void SyncLogEmpty() => logEmpty.IsVisible = state.Logs.Count == 0;
        state.Logs.CollectionChanged += (_, _) => SyncLogEmpty();
        SyncLogEmpty();

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    Theme.Section("監控"),
                    monitorCard,
                    Theme.Section("LLM"),
                    llmCard,
                    Theme.Section("此裝置的能力(由核心偵測)"),
                    Theme.Card(_capsRows),
                    Theme.Section("日誌"),
                    Theme.Card(new VerticalStackLayout { Spacing = 4, Children = { logEmpty, logs } }),
                },
            },
        };
    }

    static Grid SettingRow(string title, string sub, View control)
    {
        var g = new Grid { ColumnSpacing = 12 };
        g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        g.Add(new VerticalStackLayout
        {
            Spacing = 2,
            Children = { Theme.Body(title), Theme.Dim(sub, 12) },
        }, 0, 0);
        control.VerticalOptions = LayoutOptions.Center;
        g.Add(control, 1, 0);
        return g;
    }

    void BuildCaps()
    {
        _capsRows.Children.Clear();
        var caps = _state.Caps;
        foreach (var (name, on) in new (string, bool)[]
        {
            ("背景監控", caps.BackgroundMonitoring),
            ("應用內自動更新", caps.SelfUpdate),
            ("生物辨識解鎖", caps.BiometricUnlock),
            ("教師帳號 QR 輔助", caps.QrTeacherAssist),
            ("驗證碼本地辨識", caps.OcrCaptcha),
        })
        {
            var g = new Grid();
            g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
            g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
            g.Add(Theme.Body(name), 0, 0);
            g.Add(on
                ? Theme.TextPill("可用", Theme.OkL, Theme.OkD, Theme.OkBgL, Theme.OkBgD)
                : Theme.TextPill("不可用", Theme.DimL, Theme.DimD, Theme.Card2L, Theme.Card2D), 1, 0);
            _capsRows.Children.Add(g);
        }
    }
}
