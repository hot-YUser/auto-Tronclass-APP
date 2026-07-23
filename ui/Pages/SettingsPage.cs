using System.ComponentModel;

namespace Ui;

/// <summary>設定(帳號分頁入口):監控參數、LLM 連線/答題偏好、LLM 金鑰、核心能力、日誌。
/// 所有欄位以核心推來的 <see cref="AppState.CurrentSettings"/> 現值填入(非只有預設),
/// 並訂閱 <see cref="AppState.SettingsChanged"/> 隨儲存後回填。金鑰本身不過縫,只顯示「已/未設定」。</summary>
public sealed class SettingsPage : ContentPage
{
    readonly AppState _state;
    readonly VerticalStackLayout _capsRows = new() { Spacing = 8 };

    // 監控
    readonly Entry _countdown = NumEntry();
    readonly Entry _threshold = NumEntry();
    readonly Switch _thresholdOn = new() { IsToggled = true };
    // LLM
    readonly Label _keyStatus = Theme.Dim("", 12.5);
    readonly Entry _llmKey = new() { Placeholder = "API 金鑰", IsPassword = true };
    readonly Entry _endpoint = new() { Placeholder = "https://…/v1/chat/completions" };
    readonly Entry _model = new() { Placeholder = "模型名稱" };
    readonly Entry _maxTokens = NumEntry();
    readonly Switch _resubmit = new();
    readonly Switch _tools = new();

    readonly Action _onSettings;
    readonly PropertyChangedEventHandler _onCaps;

    public SettingsPage(AppState state)
    {
        _state = state;
        Title = "設定";

        // --- 監控參數 ---
        var monitorCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 10,
            Children =
            {
                SettingRow("倒數秒數", "自動簽到/送出前的反悔窗", _countdown),
                Theme.Divider(),
                SettingRow("防假點名門檻(%)", "全班簽到率低於此值不出手", _threshold),
                Theme.Divider(),
                SettingRow("啟用門檻", "關閉後任何點名都會處理", _thresholdOn),
                Theme.Primary("儲存監控設定", async () =>
                {
                    if (!int.TryParse(_countdown.Text, out var secs) || secs < 0 ||
                        !double.TryParse(_threshold.Text, out var pct) || pct is < 0 or > 100)
                    {
                        state.Notify("error", "數值格式不正確");
                        return;
                    }
                    if (await state.SaveConfig(secs, pct, _thresholdOn.IsToggled))
                        state.Notify("info", "監控設定已儲存");
                }),
            },
        });

        // --- LLM 金鑰(存保險庫,與其他 LLM 設定分開) ---
        var keyCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 10,
            Children =
            {
                Theme.Dim("答題由 LLM 產生答案;金鑰加密存於保險庫,不寫入設定檔。", 12.5),
                _keyStatus,
                _llmKey,
                Theme.Primary("儲存金鑰", async () =>
                {
                    var key = _llmKey.Text?.Trim() ?? "";
                    if (key.Length == 0) { state.Notify("error", "請輸入金鑰"); return; }
                    if (await state.SetLlmKey(key))
                    {
                        _llmKey.Text = "";
                        state.Notify("info", "金鑰已儲存");
                    }
                }),
            },
        });

        // --- LLM 連線與答題偏好 ---
        var llmCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 10,
            Children =
            {
                SettingRow("端點", "LLM chat-completions 網址", _endpoint),
                Theme.Divider(),
                SettingRow("模型", "如 minimaxai/minimax-m3", _model),
                Theme.Divider(),
                SettingRow("最大 tokens", "0 = 使用安全預設(16384)", _maxTokens),
                Theme.Divider(),
                SettingRow("作答後更正拿滿分", "可重考時交卷→讀正解→再交", _resubmit),
                Theme.Divider(),
                SettingRow("允許讀取課程教材", "題目缺背景時讓 LLM 查教材/PDF", _tools),
                Theme.Primary("儲存 LLM 設定", async () =>
                {
                    var endpoint = _endpoint.Text?.Trim() ?? "";
                    var model = _model.Text?.Trim() ?? "";
                    if (endpoint.Length == 0 || model.Length == 0) { state.Notify("error", "端點與模型不可空白"); return; }
                    if (!int.TryParse(_maxTokens.Text, out var mt) || mt < 0) { state.Notify("error", "最大 tokens 格式不正確"); return; }
                    if (await state.SaveLlmSettings(endpoint, model, mt, _resubmit.IsToggled, _tools.IsToggled))
                        state.Notify("info", "LLM 設定已儲存");
                }),
            },
        });

        // --- 能力(core 判定,UI 只呈現) ---
        _onCaps = (_, _) => BuildCaps();
        state.Caps.PropertyChanged += _onCaps;
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

        // 以核心現值回填(現在 + 每次 SettingsChanged)。
        _onSettings = () => MainThread.BeginInvokeOnMainThread(Populate);
        state.SettingsChanged += _onSettings;
        Populate();

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
                    Theme.Section("LLM 金鑰"),
                    keyCard,
                    Theme.Section("LLM 連線與答題"),
                    llmCard,
                    Theme.Section("此裝置的能力(由核心偵測)"),
                    Theme.Card(_capsRows),
                    Theme.Section("日誌"),
                    Theme.Card(new VerticalStackLayout { Spacing = 4, Children = { logEmpty, logs } }),
                },
            },
        };
    }

    /// <summary>以核心現值填入所有欄位(修正舊版「開啟設定頁只顯示預設、不反映已存值」)。</summary>
    void Populate()
    {
        var s = _state.CurrentSettings;
        if (s is null) return;
        _countdown.Text = s.CountdownSecs.ToString();
        _thresholdOn.IsToggled = s.AttendanceGatePercent > 0;
        _threshold.Text = (s.AttendanceGatePercent > 0 ? s.AttendanceGatePercent : 15).ToString("0.#");
        _endpoint.Text = s.LlmEndpoint;
        _model.Text = s.LlmModel;
        _maxTokens.Text = s.LlmMaxTokens.ToString();
        _resubmit.IsToggled = s.ResubmitForCorrect;
        _tools.IsToggled = s.EnableLlmTools;
        _keyStatus.Text = s.HasLlmKey ? "金鑰狀態:已設定" : "金鑰狀態:尚未設定";
    }

    protected override void OnDisappearing()
    {
        base.OnDisappearing();
        _state.SettingsChanged -= _onSettings;
        _state.Caps.PropertyChanged -= _onCaps;
    }

    static Entry NumEntry() => new() { Keyboard = Keyboard.Numeric, WidthRequest = 140, HorizontalTextAlignment = TextAlignment.End };

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
