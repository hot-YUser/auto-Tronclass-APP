using Microsoft.Maui.Controls.Shapes;

namespace Ui;

/// <summary>
/// 設計 tokens(深淺色成對)+ 小型控件工廠。原生控件鉻件(Entry 底線、Switch 軌道…)
/// 由 Resources/Styles 的隱式樣式處理;版面級的卡片/膠囊/語意色在這裡。
/// </summary>
public static class Theme
{
    // 文字
    public static readonly Color TextL = Color.FromArgb("#1B1F23"), TextD = Color.FromArgb("#F2F4F5");
    public static readonly Color DimL  = Color.FromArgb("#68727B"), DimD  = Color.FromArgb("#9AA3AB");
    // 面
    public static readonly Color CardL  = Colors.White,              CardD  = Color.FromArgb("#1E2124");
    public static readonly Color Card2L = Color.FromArgb("#EDF1F2"), Card2D = Color.FromArgb("#26292D");
    public static readonly Color LineL  = Color.FromArgb("#E4E8EA"), LineD  = Color.FromArgb("#31363B");
    // 語意
    public static readonly Color PrimL   = Color.FromArgb("#0D9488"), PrimD   = Color.FromArgb("#2DD4BF");
    public static readonly Color OkL     = Color.FromArgb("#16A34A"), OkD     = Color.FromArgb("#4ADE80");
    public static readonly Color WarnL   = Color.FromArgb("#B45309"), WarnD   = Color.FromArgb("#FBBF24");
    public static readonly Color DangerL = Color.FromArgb("#DC2626"), DangerD = Color.FromArgb("#F87171");
    // 語意底(淡色墊)
    public static readonly Color PrimBgL   = Color.FromArgb("#D9F5F1"), PrimBgD   = Color.FromArgb("#123A36");
    public static readonly Color OkBgL     = Color.FromArgb("#DCFCE7"), OkBgD     = Color.FromArgb("#12301F");
    public static readonly Color WarnBgL   = Color.FromArgb("#FEF3C7"), WarnBgD   = Color.FromArgb("#3A2E14");
    public static readonly Color DangerBgL = Color.FromArgb("#FEE2E2"), DangerBgD = Color.FromArgb("#3A1A1A");

    public const string FontRegular = "OpenSansRegular", FontSemibold = "OpenSansSemibold";

    public static T Themed<T>(this T e, BindableProperty p, Color light, Color dark) where T : BindableObject
    { e.SetAppThemeColor(p, light, dark); return e; }

    public static Border StrokeThemed(this Border b, Color light, Color dark)
    { b.SetAppTheme<Brush>(Border.StrokeProperty, new SolidColorBrush(light), new SolidColorBrush(dark)); return b; }

    // ---------- 文字 ----------
    public static Label Title(string t)   => Text(t, 22, FontSemibold, TextL, TextD);
    public static Label Section(string t) => Text(t, 13, FontSemibold, DimL, DimD);
    public static Label Body(string t)    => Text(t, 14, FontRegular, TextL, TextD);
    public static Label Strong(string t, double size = 14) => Text(t, size, FontSemibold, TextL, TextD);
    public static Label Dim(string t, double size = 12.5)  => Text(t, size, FontRegular, DimL, DimD);

    public static Label Text(string t, double size, string font, Color light, Color dark)
    {
        var l = new Label { Text = t, FontSize = size, FontFamily = font, LineBreakMode = LineBreakMode.WordWrap };
        return l.Themed(Label.TextColorProperty, light, dark);
    }

    // ---------- 容器 ----------
    public static Border Card(View content, double padding = 16)
    {
        var b = new Border
        {
            Content = content,
            Padding = padding,
            StrokeThickness = 1,
            StrokeShape = new RoundRectangle { CornerRadius = 16 },
        };
        b.Themed(VisualElement.BackgroundColorProperty, CardL, CardD);
        return b.StrokeThemed(LineL, LineD);
    }

    /// <summary>語意淡色墊卡(衝突警示、暫緩提示、完成提示)。</summary>
    public static Border TintCard(View content, Color bgL, Color bgD, Color strokeL, Color strokeD, double padding = 14)
    {
        var b = new Border
        {
            Content = content,
            Padding = padding,
            StrokeThickness = 1,
            StrokeShape = new RoundRectangle { CornerRadius = 14 },
        };
        b.Themed(VisualElement.BackgroundColorProperty, bgL, bgD);
        return b.StrokeThemed(strokeL, strokeD);
    }

    public static Border Pill(View content, Color bgL, Color bgD, Thickness? padding = null)
    {
        var b = new Border
        {
            Content = content,
            Padding = padding ?? new Thickness(10, 4),
            StrokeThickness = 0,
            StrokeShape = new RoundRectangle { CornerRadius = 99 },
            HorizontalOptions = LayoutOptions.Start,
            VerticalOptions = LayoutOptions.Center,
        };
        return b.Themed(VisualElement.BackgroundColorProperty, bgL, bgD);
    }

    public static Border TextPill(string t, Color fgL, Color fgD, Color bgL, Color bgD) =>
        Pill(Text(t, 11.5, FontSemibold, fgL, fgD), bgL, bgD);

    public static Ellipse Dot(Color light, Color dark, double size = 9)
    {
        var e = new Ellipse { WidthRequest = size, HeightRequest = size, VerticalOptions = LayoutOptions.Center };
        e.SetAppTheme<Brush>(Shape.FillProperty, new SolidColorBrush(light), new SolidColorBrush(dark));
        return e;
    }

    public static BoxView Divider()
    {
        var b = new BoxView { HeightRequest = 1, Margin = new Thickness(0, 2) };
        return b.Themed(VisualElement.BackgroundColorProperty, LineL, LineD);
    }

    // ---------- 按鈕 ----------
    public static Button Primary(string text, Func<Task> onTap)
    {
        var b = new Button { Text = text }; // 隱式樣式已給主色底/圓角
        b.Clicked += async (_, _) => await onTap();
        return b;
    }

    public static Button Ghost(string text, Func<Task> onTap)
    {
        var b = new Button { Text = text, BackgroundColor = Colors.Transparent, BorderWidth = 1 };
        b.Themed(Button.TextColorProperty, PrimL, PrimD);
        b.Themed(Button.BorderColorProperty, LineL, LineD);
        b.Clicked += async (_, _) => await onTap();
        return b;
    }

    public static Button Danger(string text, Func<Task> onTap)
    {
        var b = new Button { Text = text, BackgroundColor = Colors.Transparent, BorderWidth = 1 };
        b.Themed(Button.TextColorProperty, DangerL, DangerD);
        b.Themed(Button.BorderColorProperty, LineL, LineD);
        b.Clicked += async (_, _) => await onTap();
        return b;
    }

    /// <summary>低調文字鈕(彈窗「詳細」「✕」之類)。</summary>
    public static Button Link(string text, Func<Task> onTap)
    {
        var b = new Button { Text = text, BackgroundColor = Colors.Transparent, BorderWidth = 0, Padding = new Thickness(8, 4), MinimumHeightRequest = 36, MinimumWidthRequest = 36 };
        b.Themed(Button.TextColorProperty, DimL, DimD);
        b.Clicked += async (_, _) => await onTap();
        return b;
    }

    public static void OnTap(this View v, Func<Task> onTap)
    {
        var g = new TapGestureRecognizer();
        g.Tapped += async (_, _) => await onTap();
        v.GestureRecognizers.Add(g);
    }

    /// <summary>
    /// 把「訂閱長命發布者」的生命週期綁到這個視圖的 attach/detach:Loaded 時 subscribe、Unloaded 時 unsubscribe,
    /// 冪等(避免重複)。用於短命視圖訂長命單例/VM 事件(否則單例握著 handler → 整頁無法 GC)。
    /// 若已在畫面上(Loaded 不會再觸發)則立即 subscribe 一次。
    /// </summary>
    public static void WhileAttached(this VisualElement v, Action subscribe, Action unsubscribe)
    {
        var on = false;
        void Sub() { if (!on) { on = true; subscribe(); } }
        void Unsub() { if (on) { on = false; unsubscribe(); } }
        v.Loaded += (_, _) => Sub();
        v.Unloaded += (_, _) => Unsub();
        if (v.IsLoaded) Sub();
    }
}
