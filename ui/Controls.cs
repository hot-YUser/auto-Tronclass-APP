using System.Collections;
using System.Collections.Specialized;
using System.ComponentModel;

namespace Ui;

/// <summary>空狀態:置中標題 + 說明,收在一張卡裡(比一行灰字更完整、耐看)。</summary>
public sealed class EmptyState : ContentView
{
    public EmptyState(string title, string subtitle)
    {
        var t = Theme.Strong(title, 15);
        t.HorizontalOptions = LayoutOptions.Center;
        var s = Theme.Dim(subtitle, 12.5);
        s.HorizontalOptions = LayoutOptions.Center;
        s.HorizontalTextAlignment = TextAlignment.Center;
        Content = Theme.Card(new VerticalStackLayout
        {
            Spacing = 6,
            Padding = new Thickness(12, 22),
            Children = { t, s },
        });
    }
}

/// <summary>
/// 狀態膠囊:文字 + 語意色隨 VM 狀態原地換色(不重建)。<c>tone</c> 回傳當下的
/// 標籤與色組;<c>triggers</c> 是會改變它的屬性名(空 = 任何變動都重算)。
/// 訂閱綁 attach/detach,離開畫面即退訂。
/// </summary>
public sealed class StatusPill : ContentView
{
    public StatusPill(INotifyPropertyChanged vm,
        Func<(string tag, Color fgL, Color fgD, Color bgL, Color bgD)> tone, params string[] triggers)
    {
        var label = Theme.Text("", 11.5, Theme.FontSemibold, Theme.PrimL, Theme.PrimD);
        var pill = Theme.Pill(label, Theme.PrimBgL, Theme.PrimBgD, new Thickness(11, 4));
        Content = pill;

        void Sync() { var (t, fl, fd, bl, bd) = tone(); label.Text = t; pill.Recolor(label, fl, fd, bl, bd); }
        void OnPc(object? _, PropertyChangedEventArgs e) { if (triggers.Length == 0 || triggers.Contains(e.PropertyName)) Sync(); }
        this.WhileAttached(() => { vm.PropertyChanged += OnPc; Sync(); }, () => vm.PropertyChanged -= OnPc);
    }
}

/// <summary>
/// 逐帳號結果膠囊列(換行排列)。<c>cell</c> 由每個項目算出(文字, 是否完成);完成＝綠、
/// 未完成＝灰。集合變動、或項目的 <c>triggers</c> 屬性變動時重建。訂閱綁 attach/detach。
/// </summary>
public sealed class ChipsView : ContentView
{
    readonly FlexLayout _flex = new() { Wrap = Microsoft.Maui.Layouts.FlexWrap.Wrap };
    readonly IEnumerable _items;
    readonly Func<object, (string text, bool done)> _cell;
    readonly string[] _triggers;
    readonly Dictionary<INotifyPropertyChanged, PropertyChangedEventHandler> _hooks = [];

    public ChipsView(IEnumerable items, Func<object, (string text, bool done)> cell, params string[] triggers)
    {
        _items = items; _cell = cell; _triggers = triggers;
        Content = _flex;

        var coll = items as INotifyCollectionChanged;
        void OnColl(object? _, NotifyCollectionChangedEventArgs __) => Rebuild();
        this.WhileAttached(
            () => { if (coll != null) coll.CollectionChanged += OnColl; Rebuild(); },
            () => { if (coll != null) coll.CollectionChanged -= OnColl; UnhookAll(); });
    }

    void UnhookAll()
    {
        foreach (var (k, h) in _hooks) k.PropertyChanged -= h;
        _hooks.Clear();
    }

    void Rebuild()
    {
        // 先解除並移除「已不在集合裡」的 item hooks(與 UnhookAll 同一路徑):集合縮減時,
        // 長命 VM 不被本視圖握住、也不再因它的 PropertyChanged 白做重建(hooks 有界)。
        var present = new HashSet<object>(_items.Cast<object>());
        foreach (var (npc, h) in _hooks.ToList())
            if (!present.Contains(npc))
            {
                npc.PropertyChanged -= h;
                _hooks.Remove(npc);
            }

        // 再訂閱新面孔
        foreach (var obj in _items)
            if (obj is INotifyPropertyChanged npc && !_hooks.ContainsKey(npc))
            {
                void H(object? _, PropertyChangedEventArgs e) { if (_triggers.Length == 0 || _triggers.Contains(e.PropertyName)) Rebuild(); }
                npc.PropertyChanged += H;
                _hooks[npc] = H;
            }

        _flex.Children.Clear();
        var any = false;
        foreach (var obj in _items)
        {
            any = true;
            var (text, done) = _cell(obj);
            var chip = done
                ? Theme.TextPill(text, Theme.OkL, Theme.OkD, Theme.OkBgL, Theme.OkBgD)
                : Theme.TextPill(text, Theme.DimL, Theme.DimD, Theme.Card2L, Theme.Card2D);
            chip.Margin = new Thickness(0, 2, 6, 0);
            _flex.Children.Add(chip);
        }
        IsVisible = any;
    }
}

/// <summary>
/// 倒數條:只渲染 core 推來的 <see cref="ICountdownVm.RemainingSecs"/>(UI 不自己計時),
/// 每秒一發之間用 ProgressTo 補間成平滑。RemainingSecs 為 null 時整條收合。
/// 訂閱綁定 attach/detach 生命週期,離開畫面即退訂(避免長命 VM 握住此視圖)。
/// </summary>
public sealed class CountdownView : ContentView
{
    /// <param name="showRate">未達門檻時是否把簽到率數字寫進這行字。英雄彈窗用大字顯示簽到率,
    /// 這裡就傳 false 免得同一個數字出現兩次。</param>
    public CountdownView(ICountdownVm vm, string verb, double fontSize = 13, bool showRate = true)
    {
        var label = Theme.Text("", fontSize, Theme.FontSemibold, Theme.PrimL, Theme.PrimD);
        var bar = new ProgressBar();

        void Update(bool animate)
        {
            // 未達門檻:core 不倒數,此時渲染「即時簽到率 → 門檻」的進度(原本整條收合、畫面很空)。
            if (vm is IGateVm { Holding: true } gate)
            {
                IsVisible = true;
                label.Text = showRate
                    ? $"全班簽到率 {gate.AttendanceRate:0.#}% · 未達 {gate.GatePercent:0.#}% 門檻"
                    : $"未達 {gate.GatePercent:0.#}% 門檻 · 持續偵測中";
                var pct = gate.GatePercent > 0 ? Math.Clamp(gate.AttendanceRate / gate.GatePercent, 0, 1) : 0;
                if (animate) bar.ProgressTo(pct, 950, Easing.Linear);
                else bar.Progress = pct;
                return;
            }
            IsVisible = vm.RemainingSecs.HasValue;
            if (vm.RemainingSecs is not int s) return;
            label.Text = $"{s} 秒後{verb}";
            var target = vm.TotalSecs > 0 ? (double)s / vm.TotalSecs : 0;
            if (animate) bar.ProgressTo(target, 950, Easing.Linear);
            else bar.Progress = target;
        }

        void OnChanged(object? _, PropertyChangedEventArgs a)
        {
            if (a.PropertyName is nameof(ICountdownVm.RemainingSecs)
                or nameof(IGateVm.Holding) or nameof(IGateVm.AttendanceRate)) Update(animate: true);
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
