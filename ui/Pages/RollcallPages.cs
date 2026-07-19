using Microsoft.Maui.Controls.Shapes;

namespace Ui;

/// <summary>點名列表:進行中與近期紀錄(合併後一活動一列,新的在最上)。</summary>
public sealed class RollcallListPage : ContentPage
{
    public RollcallListPage(AppState state)
    {
        Title = "點名";

        var host = new VerticalStackLayout { Spacing = 10 };
        BindableLayout.SetItemsSource(host, state.Rollcalls);
        BindableLayout.SetItemTemplate(host, new DataTemplate(() => new RollcallRow()));

        var empty = Theme.Dim("尚未偵測到點名。開始監控後,新的點名會即時出現在這裡。", 13);
        void SyncEmpty() => empty.IsVisible = state.Rollcalls.Count == 0;
        state.Rollcalls.CollectionChanged += (_, _) => SyncEmpty();
        SyncEmpty();

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children = { new StatusBanner(state), empty, host },
            },
        };
    }
}

/// <summary>列表列:課程/型別/時間/簽到率 + 狀態 + 迷你倒數(與詳細頁、彈窗綁同一 VM)。</summary>
sealed class RollcallRow : Border
{
    readonly VerticalStackLayout _countdownHost = new();

    public RollcallRow()
    {
        Padding = 14;
        StrokeThickness = 1;
        StrokeShape = new RoundRectangle { CornerRadius = 16 };
        this.Themed(BackgroundColorProperty, Theme.CardL, Theme.CardD).StrokeThemed(Theme.LineL, Theme.LineD);

        var course = Theme.Strong("", 15);
        course.SetBinding(Label.TextProperty, nameof(RollcallVm.Course));
        var sub = Theme.Dim("");
        sub.SetBinding(Label.TextProperty, nameof(RollcallVm.SubtitleText));
        var status = Theme.Text("", 12.5, Theme.FontSemibold, Theme.PrimL, Theme.PrimD);
        status.SetBinding(Label.TextProperty, nameof(RollcallVm.StatusText));

        Content = new VerticalStackLayout
        {
            Spacing = 4,
            Children = { course, sub, status, _countdownHost },
        };

        this.OnTap(() => BindingContext is RollcallVm vm
            ? ((AppShell)Shell.Current).OpenRollcallDetail(vm)
            : Task.CompletedTask);
    }

    protected override void OnBindingContextChanged()
    {
        base.OnBindingContextChanged();
        _countdownHost.Children.Clear();
        if (BindingContext is RollcallVm vm)
            _countdownHost.Children.Add(new CountdownView(vm, "自動簽到", 12) { Margin = new Thickness(0, 6, 0, 0) });
    }
}

/// <summary>點名詳細:資訊 / 倒數與動作 / 暫緩補簽 / per-account 簽到狀態。</summary>
public sealed class RollcallDetailPage : ContentPage
{
    public RollcallDetailPage(AppState state, RollcallVm vm)
    {
        Title = vm.Course;
        BindingContext = vm;

        var info = Theme.Card(new VerticalStackLayout
        {
            Spacing = 8,
            Children =
            {
                new HorizontalStackLayout
                {
                    Spacing = 8,
                    Children = { Theme.TextPill(vm.KindText, Theme.PrimL, Theme.PrimD, Theme.PrimBgL, Theme.PrimBgD) },
                },
                KeyValue("課程", vm.Course),
                KeyValue("全班簽到率", $"{vm.AttendanceRate:0.#}%"),
                KeyValue("偵測時間", vm.DetectedAt.ToString("HH:mm:ss")),
                KeyValue("平台", vm.BaseUrl),
            },
        });

        var actionRow = new Grid { ColumnSpacing = 8 };
        actionRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        actionRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        actionRow.Add(Theme.Primary("立即簽到", () => state.SignNow(vm.Id)), 0, 0);
        actionRow.Add(Theme.Ghost("暫緩", () => state.DeferSignIn(vm.Id)), 1, 0);

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
                Theme.Primary("立即補簽", () => state.SignNow(vm.Id)),
            },
        }, Theme.WarnBgL, Theme.WarnBgD, Theme.WarnL, Theme.WarnD);
        pendingCard.SetBinding(IsVisibleProperty, nameof(RollcallVm.IsPending));

        var doneCard = Theme.TintCard(
            Theme.Text("✓ 已完成簽到", 14, Theme.FontSemibold, Theme.OkL, Theme.OkD),
            Theme.OkBgL, Theme.OkBgD, Theme.OkL, Theme.OkD);
        doneCard.SetBinding(IsVisibleProperty, nameof(RollcallVm.IsDone));

        var accountRows = new VerticalStackLayout { Spacing = 8 };
        BindableLayout.SetItemsSource(accountRows, vm.Accounts);
        BindableLayout.SetItemTemplate(accountRows, new DataTemplate(() =>
        {
            var name = Theme.Body("");
            name.SetBinding(Label.TextProperty, nameof(RollcallAccountVm.Label));
            var st = Theme.Dim("");
            st.SetBinding(Label.TextProperty, nameof(RollcallAccountVm.StateText));
            st.HorizontalOptions = LayoutOptions.End;
            var g = new Grid();
            g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
            g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
            g.Add(name, 0, 0);
            g.Add(st, 1, 0);
            return g;
        }));

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    info,
                    countingCard,
                    pendingCard,
                    doneCard,
                    Theme.Section("參與帳號"),
                    Theme.Card(accountRows),
                },
            },
        };
    }

    static Grid KeyValue(string key, string value)
    {
        var g = new Grid { ColumnSpacing = 12 };
        g.ColumnDefinitions.Add(new ColumnDefinition(new GridLength(96)));
        g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        g.Add(Theme.Dim(key, 13), 0, 0);
        g.Add(Theme.Body(value), 1, 0);
        return g;
    }
}
