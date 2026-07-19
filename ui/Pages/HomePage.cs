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

    public HomePage(AppState state)
    {
        _state = state;
        Title = "首頁";

        // --- 監控總開關卡 ---
        var dot = Theme.Dot(Theme.DimL, Theme.DimD, 11);
        var stateText = Theme.Strong("", 17);
        stateText.VerticalOptions = LayoutOptions.Center;
        var toggle = Theme.Primary("", async () =>
            await (state.IsMonitoring ? state.StopMonitoring() : state.StartMonitoring()));

        void SyncMonitor()
        {
            stateText.Text = state.MonitorStateText;
            var (l, d) = state.MonitorState switch
            {
                "monitoring" => (Theme.OkL, Theme.OkD),
                "login_failed" or "offline" => (Theme.DangerL, Theme.DangerD),
                _ => (Theme.DimL, Theme.DimD),
            };
            dot.SetAppTheme<Brush>(Shape.FillProperty, new SolidColorBrush(l), new SolidColorBrush(d));
            toggle.Text = state.IsMonitoring ? "停止監控" : "開始監控";
        }
        state.PropertyChanged += (_, a) =>
        {
            if (a.PropertyName == nameof(AppState.MonitorState)) SyncMonitor();
            else if (a.PropertyName == nameof(AppState.NextClass)) BuildNextClass();
        };
        SyncMonitor();
        BuildNextClass();

        // Tick 心跳:狀態點微脈動(core 活著的證明)
        state.Ticked += async () =>
        {
            if (!state.IsMonitoring) return;
            await dot.ScaleToAsync(1.3, 120, Easing.CubicOut);
            await dot.ScaleToAsync(1.0, 260, Easing.CubicIn);
        };

        var fgPill = Theme.TextPill("僅前景執行 · 螢幕關閉時暫停監控", Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD);
        fgPill.BindingContext = state.Caps;
        fgPill.SetBinding(IsVisibleProperty, nameof(CapsVm.ForegroundOnly));

        var monitorCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 12,
            Children =
            {
                new HorizontalStackLayout { Spacing = 8, Children = { dot, stateText } },
                Theme.Dim("監控開啟時,偵測到點名會自動簽到、偵測到測驗會由 LLM 備答後自動送出;有時限操作前都會先彈窗讓你介入。", 13),
                toggle,
                fgPill,
            },
        });

        // --- 帳號摘要 chips ---
        state.Accounts.CollectionChanged += (_, _) => { SyncAccountHooks(); BuildAccountChips(); };
        SyncAccountHooks();
        BuildAccountChips();

        // --- 近期活動 ---
        state.Rollcalls.CollectionChanged += (_, _) => BuildFeed();
        state.Quizzes.CollectionChanged += (_, _) => BuildFeed();
        BuildFeed();

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
        meta.Children.Add(Theme.TextPill(nc.When, Theme.PrimL, Theme.PrimD, Theme.PrimBgL, Theme.PrimBgD));
        if (!string.IsNullOrEmpty(nc.Location)) meta.Children.Add(Centered(Theme.Dim(nc.Location, 13)));
        if (!string.IsNullOrEmpty(accLabel)) meta.Children.Add(Centered(Theme.Dim($"· {accLabel}", 13)));
        _nextClassHost.Content = Theme.Card(new VerticalStackLayout
        {
            Spacing = 6,
            Children = { Theme.Section("下一堂課"), Theme.Strong(nc.Course, 16), meta },
        });
    }

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
            .Select(r => (r.DetectedAt, view: FeedRow("點名", r, r.Course, () => ((AppShell)Shell.Current).OpenRollcallDetail(r), nameof(RollcallVm.StatusText))))
            .Concat(_state.Quizzes
                .Select(q => (q.DetectedAt, view: FeedRow("答題", q, q.Course, () => ((AppShell)Shell.Current).OpenQuizDetail(q), nameof(QuizVm.StatusText)))))
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

    static View FeedRow(string kind, ObservableObject vm, string course, Func<Task> open, string statusPath)
    {
        var status = Theme.Dim("", 12);
        status.BindingContext = vm;
        status.SetBinding(Label.TextProperty, statusPath);

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
