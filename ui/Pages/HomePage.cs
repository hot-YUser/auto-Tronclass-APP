using System.Collections.Specialized;
using System.ComponentModel;
using Microsoft.Maui.Controls.Shapes;

namespace Ui;

/// <summary>首頁:監控總開關、帳號摘要、背景可用性、近期活動。</summary>
public sealed class HomePage : ContentPage
{
    readonly AppState _state;
    readonly Dictionary<AccountVm, PropertyChangedEventHandler> _accHooks = [];
    readonly FlexLayout _accountChips = new() { Wrap = Microsoft.Maui.Layouts.FlexWrap.Wrap };
    readonly VerticalStackLayout _feed = new() { Spacing = 8 };
    readonly ContentView _nextClassHost = new() { IsVisible = false };
    readonly Ellipse _dot;
    readonly Border _nextWhenPill;
    readonly Label _nextWhen;
    readonly Label _stateText;
    readonly Button _toggle;
    bool _attached;

    public HomePage(AppState state)
    {
        _state = state;
        Title = "首頁";

        // --- 監控總開關卡 ---
        _dot = Theme.Dot(Theme.DimL, Theme.DimD, 11);
        _stateText = Theme.Strong("", 17);
        _stateText.VerticalOptions = LayoutOptions.Center;
        _toggle = Theme.Primary("", async () =>
            await (state.IsMonitoring ? state.StopMonitoring() : state.StartMonitoring()));

        _nextWhen = Theme.Text("", 11.5, Theme.FontSemibold, Theme.PrimL, Theme.PrimD);
        _nextWhenPill = Theme.Pill(_nextWhen, Theme.PrimBgL, Theme.PrimBgD);

        var fgPill = Theme.TextPill("僅前景執行 · 螢幕關閉時暫停監控", Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD);
        fgPill.BindingContext = state.Caps;
        fgPill.SetBinding(IsVisibleProperty, static (CapsVm caps) => caps.ForegroundOnly);

        var monitorCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 12,
            Children =
            {
                new HorizontalStackLayout { Spacing = 8, Children = { _dot, _stateText } },
                Theme.Dim("監控開啟時,偵測到點名會自動簽到、偵測到測驗會由 LLM 備答後自動送出;有時限操作前都會先彈窗讓你介入。", 13),
                _toggle,
                fgPill,
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
                    Theme.Section("被監控帳號"),
                    _accountChips,
                    Theme.Section("近期活動"),
                    _feed,
                },
            },
        };
    }
    void SyncMonitor()
    {
        _stateText.Text = _state.MonitorStateText;
        var (light, dark) = _state.MonitorState switch
        {
            "monitoring" => (Theme.OkL, Theme.OkD),
            "login_failed" or "offline" => (Theme.DangerL, Theme.DangerD),
            _ => (Theme.DimL, Theme.DimD),
        };
        _dot.SetAppTheme<Brush>(
            Shape.FillProperty,
            new SolidColorBrush(light),
            new SolidColorBrush(dark));
        _toggle.Text = _state.IsMonitoring ? "停止監控" : "開始監控";
        _toggle.IsEnabled = _state.CanToggleMonitoring;
    }

    // 所有 singleton/collection 訂閱都綁頁面生命週期:離開畫面即退訂,
    // 長命發布者(AppState/Accounts/Rollcalls/Quizzes/Ticked)不會握住本頁 → 舊頁可被 GC。
    protected override void OnAppearing()
    {
        base.OnAppearing();
        if (_attached) return;
        _attached = true;
        _state.PropertyChanged += OnStateChanged;
        _state.Ticked += OnTicked;
        _state.Accounts.CollectionChanged += OnAccountsChanged;
        _state.Rollcalls.CollectionChanged += OnRollcallsChanged;
        _state.Quizzes.CollectionChanged += OnQuizzesChanged;
        // 每次進場都以現值同步(暫離期間可能漏掉的事件一併補上)
        SyncMonitor();
        BuildNextClass();
        SyncAccountHooks();
        BuildAccountChips();
        BuildFeed();
    }

    protected override void OnDisappearing()
    {
        base.OnDisappearing();
        if (!_attached) return;
        _attached = false;
        _state.PropertyChanged -= OnStateChanged;
        _state.Ticked -= OnTicked;
        _state.Accounts.CollectionChanged -= OnAccountsChanged;
        _state.Rollcalls.CollectionChanged -= OnRollcallsChanged;
        _state.Quizzes.CollectionChanged -= OnQuizzesChanged;
        UnhookAccounts(); // 長命 AccountVm 不得握住本頁
    }

    void OnStateChanged(object? _, PropertyChangedEventArgs a)
    {
        if (a.PropertyName is nameof(AppState.MonitorState) or nameof(AppState.CanToggleMonitoring)) SyncMonitor();
        else if (a.PropertyName == nameof(AppState.NextClass)) BuildNextClass();
    }

    void OnAccountsChanged(object? _, NotifyCollectionChangedEventArgs __) { SyncAccountHooks(); BuildAccountChips(); }
    void OnRollcallsChanged(object? _, NotifyCollectionChangedEventArgs __) => BuildFeed();
    void OnQuizzesChanged(object? _, NotifyCollectionChangedEventArgs __) => BuildFeed();

    /// <summary>
    /// 既有 Tick 心跳:更新下一堂課相對時間 + 狀態點微脈動(core 活著的證明)。
    /// 例外全接(卸離/釋放中的視圖上動畫可能失敗);卸離後不再動畫。
    /// </summary>
    async void OnTicked()
    {
        if (!_attached) return;
        RefreshNextClassTime();
        if (!_state.IsMonitoring) return;
        try
        {
            await _dot.ScaleToAsync(1.3, 120, Easing.CubicOut);
            if (!_attached) return; // 卸離後不再動畫
            await _dot.ScaleToAsync(1.0, 260, Easing.CubicIn);
        }
        catch (Exception)
        {
            // 心跳本身不可外洩例外(頁面可能正在卸離/釋放)
        }
    }

    /// <summary>下一堂課的相對時間文字只在變化時寫入(Tick 每秒一發,文字每分鐘才變)。</summary>
    void RefreshNextClassTime()
    {
        if (_state.NextClass is not { } nc || !_nextClassHost.IsVisible) return;
        var text = NextClassWhen(nc);
        if (_nextWhen.Text != text) _nextWhen.Text = text;
    }

    /// <summary>下一堂課卡。重建只發生在 NextClass 變動;相對時間由 Tick 原地更新(不新增 timer)。</summary>
    void BuildNextClass()
    {
        if (_state.NextClass is not { } nc)
        {
            _nextClassHost.IsVisible = false;
            _nextClassHost.Content = null;
            return;
        }
        _nextClassHost.IsVisible = true;
        var accLabel = _state.Accounts.FirstOrDefault(a => a.Id == nc.AccountId)?.Label;
        var meta = new HorizontalStackLayout { Spacing = 8 };
        _nextWhen.Text = NextClassWhen(nc);
        meta.Children.Add(_nextWhenPill);
        if (!string.IsNullOrEmpty(nc.Location)) meta.Children.Add(Centered(Theme.Dim(nc.Location, 13)));
        if (!string.IsNullOrEmpty(accLabel)) meta.Children.Add(Centered(Theme.Dim($"· {accLabel}", 13)));
        _nextClassHost.Content = Theme.Card(new VerticalStackLayout
        {
            Spacing = 6,
            Children = { Theme.Section("下一堂課"), Theme.Strong(nc.Course, 16), meta },
        });
    }

    /// <summary>已開始的課不得顯示「即將開始」(模型 When 對負差也回「即將開始」,這裡補正)。</summary>
    static string NextClassWhen(NextClassVm nc) =>
        nc.StartTime <= DateTimeOffset.Now ? "已開始" : nc.When;

    static Label Centered(Label l) { l.VerticalOptions = LayoutOptions.Center; return l; }

    void SyncAccountHooks()
    {
        foreach (var a in _state.Accounts)
            if (!_accHooks.ContainsKey(a))
            {
                void H(object? _, PropertyChangedEventArgs e)
                {
                    if (e.PropertyName is nameof(AccountVm.State) or nameof(AccountVm.Label)) BuildAccountChips();
                }
                a.PropertyChanged += H;
                _accHooks[a] = H;
            }
        foreach (var a in _accHooks.Keys.Where(k => !_state.Accounts.Contains(k)).ToList())
        {
            a.PropertyChanged -= _accHooks[a];
            _accHooks.Remove(a);
        }
    }

    void UnhookAccounts()
    {
        foreach (var (a, h) in _accHooks) a.PropertyChanged -= h;
        _accHooks.Clear();
    }

    void BuildAccountChips()
    {
        _accountChips.Children.Clear();
        if (_state.Accounts.Count == 0)
        {
            _accountChips.Children.Add(Theme.Dim("尚未新增帳號 — 到「帳號」分頁新增。", 13));
            return;
        }
        foreach (var a in _state.Accounts)
        {
            var (fgL, fgD, bgL, bgD) = a.State switch
            {
                "online" => (Theme.OkL, Theme.OkD, Theme.OkBgL, Theme.OkBgD),
                "login_failed" => (Theme.DangerL, Theme.DangerD, Theme.DangerBgL, Theme.DangerBgD),
                _ => (Theme.DimL, Theme.DimD, Theme.Card2L, Theme.Card2D),
            };
            var pill = Theme.TextPill($"{a.Label} · {a.StateText}", fgL, fgD, bgL, bgD);
            pill.Margin = new Thickness(0, 0, 6, 6);
            _accountChips.Children.Add(pill);
        }
    }

    void BuildFeed()
    {
        _feed.Children.Clear();
        var items = _state.Rollcalls
            .Select(r => (r.DetectedAt, view: FeedRow("點名", r, r.Course, () => ((AppShell)Shell.Current).OpenRollcallDetail(r),
                static l => l.SetBinding(Label.TextProperty, static (RollcallVm v) => v.StatusText))))
            .Concat(_state.Quizzes
                .Select(q => (q.DetectedAt, view: FeedRow("答題", q, q.Course, () => ((AppShell)Shell.Current).OpenQuizDetail(q),
                    static l => l.SetBinding(Label.TextProperty, static (QuizVm v) => v.StatusText)))))
            .OrderByDescending(x => x.DetectedAt)
            .Take(8)
            .ToList();
        if (items.Count == 0)
        {
            _feed.Children.Add(Theme.Dim("尚無活動。開始監控後,點名與測驗會即時出現。", 13));
            return;
        }
        foreach (var (_, view) in items) _feed.Children.Add(view);
    }

    /// 綁定動作由呼叫端傳入,而不是傳字串路徑或 getter 委派:字串路徑要靠反射解析(full trim /
    /// NativeAOT 下屬性會被砍掉,綁定靜默失效),而 MAUI 的 bindings source generator 要求 lambda
    /// 必須「字面」出現在 SetBinding 呼叫點(傳委派會得到 BSG0002),所以 lambda 留在呼叫端。
    static View FeedRow(string kind, ObservableObject vm, string course, Func<Task> open, Action<Label> bindStatus)
    {
        var status = Theme.Dim("", 12);
        status.BindingContext = vm;
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
