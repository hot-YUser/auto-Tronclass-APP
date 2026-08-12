using System.Collections.Specialized;
using Microsoft.Maui.Controls.Shapes;

namespace Ui;

/// <summary>點名列表:進行中與近期紀錄(合併後一活動一列,新的在最上)。</summary>
public sealed class RollcallListPage : ContentPage
{
    readonly AppState _state;
    readonly EmptyState _empty;
    bool _attached;

    public RollcallListPage(AppState state)
    {
        _state = state;
        Title = "點名";

        var host = new VerticalStackLayout { Spacing = 10 };
        BindableLayout.SetItemsSource(host, state.Rollcalls);
        BindableLayout.SetItemTemplate(host, new DataTemplate(() => new RollcallRow()));

        _empty = new EmptyState("尚無點名紀錄", "開始監控後,偵測到的點名會即時出現在這裡,並保留為紀錄。");

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children = { new StatusBanner(state), _empty, host },
            },
        };
    }

    // singleton 訂閱綁頁面生命週期:離開畫面即退訂(長命 Rollcalls 不握住本頁)。
    protected override void OnAppearing()
    {
        base.OnAppearing();
        if (_attached) return;
        _attached = true;
        _state.Rollcalls.CollectionChanged += OnRollcallsChanged;
        SyncEmpty();
    }

    protected override void OnDisappearing()
    {
        base.OnDisappearing();
        if (!_attached) return;
        _attached = false;
        _state.Rollcalls.CollectionChanged -= OnRollcallsChanged;
    }

    void OnRollcallsChanged(object? _, NotifyCollectionChangedEventArgs __) => SyncEmpty();

    void SyncEmpty() => _empty.IsVisible = _state.Rollcalls.Count == 0;
}

/// <summary>紀錄卡:類型徽章 + 課程 + 狀態膠囊 + meta(類型·時間·簽到率) + 逐帳號簽到 chips + 迷你倒數。</summary>
sealed class RollcallRow : Border
{
    public RollcallRow()
    {
        Padding = 14;
        StrokeThickness = 1;
        StrokeShape = new RoundRectangle { CornerRadius = 18 };
        this.Themed(BackgroundColorProperty, Theme.CardL, Theme.CardD).StrokeThemed(Theme.LineL, Theme.LineD);
        this.OnTap(() => BindingContext is RollcallVm vm
            ? ((AppShell)Shell.Current).OpenRollcallDetail(vm)
            : Task.CompletedTask);
    }

    protected override void OnBindingContextChanged()
    {
        base.OnBindingContextChanged();
        if (BindingContext is not RollcallVm vm) { Content = null; return; }

        var (emblem, glyph) = Theme.Emblem();
        glyph.SetBinding(Label.TextProperty, new Binding(nameof(RollcallVm.KindEmblem), source: vm));

        var course = Theme.Strong("", 15.5);
        course.VerticalOptions = LayoutOptions.Center;
        course.SetBinding(Label.TextProperty, new Binding(nameof(RollcallVm.Course), source: vm));

        var statusPill = new StatusPill(vm, () => RollcallToneOf(vm), nameof(RollcallVm.StatusTag));
        statusPill.VerticalOptions = LayoutOptions.Center;
        statusPill.HorizontalOptions = LayoutOptions.End;

        var header = new Grid { ColumnSpacing = 8 };
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        header.Add(course, 0, 0);
        header.Add(statusPill, 1, 0);

        var meta = Theme.Dim("", 12.5);
        meta.SetBinding(Label.TextProperty, new Binding(nameof(RollcallVm.MetaText), source: vm));

        var chips = new ChipsView(vm.Accounts,
            o => { var a = (RollcallAccountVm)o; return (a.ChipText, a.Signed); },
            nameof(RollcallAccountVm.ChipText));

        var countdown = new CountdownView(vm, "自動簽到", 12, showRate: false) { Margin = new Thickness(0, 2, 0, 0) };

        var body = new VerticalStackLayout { Spacing = 6, Children = { header, meta, chips, countdown } };

        var grid = new Grid { ColumnSpacing = 12 };
        grid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        grid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        grid.Add(emblem, 0, 0);
        grid.Add(body, 1, 0);
        Content = grid;
    }

    /// 狀態膠囊的短標籤 + 語意色:已完成綠、暫緩/等待門檻琥珀、進行中主色。列表卡與詳細頁共用。
    internal static (string, Color, Color, Color, Color) RollcallToneOf(RollcallVm vm) => vm.Status switch
    {
        "done" => (vm.StatusTag, Theme.OkL, Theme.OkD, Theme.OkBgL, Theme.OkBgD),
        "pending" => (vm.StatusTag, Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD),
        _ when vm.Holding => (vm.StatusTag, Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD),
        _ => (vm.StatusTag, Theme.PrimL, Theme.PrimD, Theme.PrimBgL, Theme.PrimBgD),
    };
}

/// <summary>點名詳細:大標頭(徽章/課程/狀態) / 倒數與動作 / 暫緩補簽 / 資訊 / per-account 簽到狀態。</summary>
public sealed class RollcallDetailPage : ContentPage
{
    public RollcallDetailPage(AppState state, RollcallVm vm)
    {
        Title = vm.Course;
        BindingContext = vm;

        // --- 大標頭 ---
        var (emblem, glyph) = Theme.Emblem(46);
        glyph.SetBinding(Label.TextProperty, new Binding(nameof(RollcallVm.KindEmblem), source: vm));
        var metaLabel = Theme.Dim("", 13);
        metaLabel.SetBinding(Label.TextProperty, new Binding(nameof(RollcallVm.MetaText), source: vm));
        var titleCol = new VerticalStackLayout
        {
            Spacing = 3,
            VerticalOptions = LayoutOptions.Center,
            Children = { Theme.Strong(vm.Course, 18), metaLabel },
        };
        var statusPill = new StatusPill(vm, () => RollcallRow.RollcallToneOf(vm), nameof(RollcallVm.StatusTag));
        statusPill.VerticalOptions = LayoutOptions.Center;

        var headerGrid = new Grid { ColumnSpacing = 12 };
        headerGrid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        headerGrid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        headerGrid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        headerGrid.Add(emblem, 0, 0);
        headerGrid.Add(titleCol, 1, 0);
        headerGrid.Add(statusPill, 2, 0);
        var header = Theme.Card(headerGrid);

        var actionRow = new Grid { ColumnSpacing = 8 };
        actionRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        actionRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        actionRow.Add(Theme.Primary("立即簽到", () => state.SignNow(vm)), 0, 0);
        actionRow.Add(Theme.Ghost("暫緩", () => state.DeferSignIn(vm)), 1, 0);

        var countingCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 12,
            Children = { new CountdownView(vm, "自動簽到", 14), actionRow },
        });
        countingCard.SetBinding(IsVisibleProperty, nameof(RollcallVm.IsCounting));

        var pendingCard = Theme.TintCard(new VerticalStackLayout
        {
            Spacing = 10,
            Children =
            {
                Theme.Text("已暫緩 — 這次點名還開著,隨時可以補簽。", 13, Theme.FontSemibold, Theme.WarnL, Theme.WarnD),
                Theme.Primary("立即補簽", () => state.SignNow(vm)),
            },
        }, Theme.WarnBgL, Theme.WarnBgD, Theme.WarnL, Theme.WarnD);
        pendingCard.SetBinding(IsVisibleProperty, nameof(RollcallVm.IsPending));

        var doneCard = Theme.TintCard(
            Theme.Text("✓ 已完成簽到", 14, Theme.FontSemibold, Theme.OkL, Theme.OkD),
            Theme.OkBgL, Theme.OkBgD, Theme.OkL, Theme.OkD);
        doneCard.SetBinding(IsVisibleProperty, nameof(RollcallVm.IsDone));

        var info = Theme.Card(new VerticalStackLayout
        {
            Spacing = 8,
            Children =
            {
                KeyValueBound("全班簽到率", nameof(RollcallVm.AttendanceRateText)), // 未達門檻時每秒重查,要活的
                KeyValue("偵測時間", vm.DetectedAt.ToString("yyyy/M/d HH:mm:ss")),
                KeyValue("平台", vm.BaseUrl),
            },
        });

        var accountRows = new VerticalStackLayout { Spacing = 4 };
        BindableLayout.SetItemsSource(accountRows, vm.Accounts);
        BindableLayout.SetItemTemplate(accountRows, new DataTemplate(() => new ParticipantRow()));

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    header,
                    countingCard,
                    pendingCard,
                    doneCard,
                    info,
                    Theme.Section("參與帳號"),
                    Theme.Card(accountRows),
                },
            },
        };
    }

    static Grid KeyValue(string key, string value) => KeyValueView(key, Theme.Body(value));

    /// 值會變的欄位:綁 VM 屬性(頁面 BindingContext = vm),而不是建構當下的快照。
    static Grid KeyValueBound(string key, string path)
    {
        var v = Theme.Body("");
        v.SetBinding(Label.TextProperty, path);
        return KeyValueView(key, v);
    }

    static Grid KeyValueView(string key, View value)
    {
        var g = new Grid { ColumnSpacing = 12 };
        g.ColumnDefinitions.Add(new ColumnDefinition(new GridLength(96)));
        g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        g.Add(Theme.Dim(key, 13), 0, 0);
        g.Add(value, 1, 0);
        return g;
    }
}

/// <summary>參與帳號列:狀態點(已簽綠/等待灰) + 名稱 + 方式/狀態文字。與 VM 綁定,簽到到達即變色。</summary>
sealed class ParticipantRow : Grid
{
    readonly Ellipse _dot;
    readonly Label _name, _state;
    RollcallAccountVm? _vm;
    System.ComponentModel.PropertyChangedEventHandler? _handler;

    public ParticipantRow()
    {
        Padding = new Thickness(0, 6);
        ColumnSpacing = 10;
        ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));

        _dot = Theme.Dot(Theme.DimL, Theme.DimD);
        _name = Theme.Body("");
        _name.VerticalOptions = LayoutOptions.Center;
        _state = Theme.Dim("");
        _state.HorizontalOptions = LayoutOptions.End;
        _state.VerticalOptions = LayoutOptions.Center;

        this.Add(_dot, 0, 0);
        this.Add(_name, 1, 0);
        this.Add(_state, 2, 0);
        Unloaded += (_, _) => Unhook();
    }

    void Unhook() { if (_vm is not null && _handler is not null) _vm.PropertyChanged -= _handler; _vm = null; _handler = null; }

    protected override void OnBindingContextChanged()
    {
        base.OnBindingContextChanged();
        Unhook();
        if (BindingContext is not RollcallAccountVm vm) return;
        _vm = vm;
        _handler = (_, a) => { if (a.PropertyName is nameof(RollcallAccountVm.Signed) or nameof(RollcallAccountVm.Method) or nameof(RollcallAccountVm.Label)) Render(vm); };
        vm.PropertyChanged += _handler;
        Render(vm);
    }

    void Render(RollcallAccountVm vm)
    {
        _name.Text = vm.Label;
        _state.Text = vm.StateText;
        var (l, d) = vm.Signed ? (Theme.OkL, Theme.OkD) : (Theme.DimL, Theme.DimD);
        _dot.SetAppTheme<Brush>(Shape.FillProperty, new SolidColorBrush(l), new SolidColorBrush(d));
    }
}
