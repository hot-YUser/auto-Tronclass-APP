using System.ComponentModel;

namespace Ui;

/// <summary>
/// 倒數條:只渲染 core 推來的 <see cref="ICountdownVm.RemainingSecs"/>(UI 不自己計時),
/// 每秒一發之間用 ProgressTo 補間成平滑。RemainingSecs 為 null 時整條收合。
/// 訂閱綁定 attach/detach 生命週期,離開畫面即退訂(避免長命 VM 握住此視圖)。
/// </summary>
public sealed class CountdownView : ContentView
{
    public CountdownView(ICountdownVm vm, string verb, double fontSize = 13)
    {
        var label = Theme.Text("", fontSize, Theme.FontSemibold, Theme.PrimL, Theme.PrimD);
        var bar = new ProgressBar();

        void Update(bool animate)
        {
            IsVisible = vm.RemainingSecs.HasValue;
            if (vm.RemainingSecs is not int s) return;
            label.Text = $"{s} 秒後{verb}";
            var target = vm.TotalSecs > 0 ? (double)s / vm.TotalSecs : 0;
            if (animate) bar.ProgressTo(target, 950, Easing.Linear);
            else bar.Progress = target;
        }

        void OnChanged(object? _, PropertyChangedEventArgs a)
        {
            if (a.PropertyName is nameof(ICountdownVm.RemainingSecs)) Update(animate: true);
        }

        this.WhileAttached(
            () => { vm.PropertyChanged += OnChanged; Update(animate: false); },
            () => vm.PropertyChanged -= OnChanged);

        Content = new VerticalStackLayout { Spacing = 6, Children = { label, bar } };
    }
}

/// <summary>
/// 錯誤/提示橫幅:訂 AppState.Toast,4 秒自散、點擊即散。每個分頁放一份(只有可見的那份被看到)。
/// 訂閱綁定 attach/detach 生命週期,頁面關閉即退訂(否則單例 Toast 握住整頁 → 洩漏)。
/// </summary>
public sealed class StatusBanner : ContentView
{
    CancellationTokenSource? _cts;

    public StatusBanner(AppState state)
    {
        IsVisible = false;
        var label = new Label
        {
            FontSize = 13,
            FontFamily = Theme.FontSemibold,
            TextColor = Colors.White,
            LineBreakMode = LineBreakMode.WordWrap,
        };
        var border = Theme.Pill(label, Theme.DangerL, Theme.DangerD, new Thickness(14, 10));
        border.HorizontalOptions = LayoutOptions.Fill;
        Content = border;

        ((View)Content).OnTap(() => { IsVisible = false; return Task.CompletedTask; });

        void Handler(string severity, string message) => Show(border, label, severity, message);
        this.WhileAttached(() => state.Toast += Handler, () => state.Toast -= Handler);
    }

    async void Show(Border border, Label label, string severity, string message)
    {
        var (l, d) = severity switch
        {
            "error" or "fatal" => (Theme.DangerL, Theme.DangerD),
            "warn" or "warning" => (Theme.WarnL, Theme.WarnD),
            _ => (Theme.PrimL, Theme.PrimD),
        };
        border.Themed(VisualElement.BackgroundColorProperty, l, d);
        label.Text = message;
        IsVisible = true;

        _cts?.Cancel();
        _cts?.Dispose();
        _cts = new CancellationTokenSource();
        try
        {
            await Task.Delay(4000, _cts.Token);
            IsVisible = false;
        }
        catch (TaskCanceledException) { /* 新訊息接手 */ }
    }
}
