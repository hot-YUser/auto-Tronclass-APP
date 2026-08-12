using System.Collections.Specialized;
using System.ComponentModel;
using Microsoft.Maui.Controls.Shapes;

namespace Ui;

/// <summary>
/// 置中小卡 modal 的共同外殼。頁面底色走隱式 Page 樣式(不透明,兩平台一致——
/// MAUI modal 頁在 Windows 不支援半透明背景,為了全平台同一套視覺,一律實色)。
/// 子類在建構子尾端呼叫 <see cref="SetCard"/>。
/// </summary>
public abstract class ModalPageBase : ContentPage
{
    protected void SetCard(View card)
    {
        var container = new Border
        {
            Content = card,
            Padding = 20,
            StrokeThickness = 1,
            StrokeShape = new RoundRectangle { CornerRadius = 20 },
            HorizontalOptions = LayoutOptions.Fill,
            VerticalOptions = LayoutOptions.Center,
            MaximumWidthRequest = 420,
            Shadow = new Shadow { Brush = Colors.Black, Opacity = 0.18f, Radius = 30, Offset = new Point(0, 8) },
        };
        container.Themed(BackgroundColorProperty, Theme.CardL, Theme.CardD).StrokeThemed(Theme.LineL, Theme.LineD);
        Content = new Grid { Padding = 24, Children = { container } };
    }

    // 預設擋 Android 硬體返回(有時限流程要明確操作);英雄彈窗覆寫成「收合」。
    protected override bool OnBackButtonPressed() => true;

    protected static Grid Row(params View[] cells)
    {
        var g = new Grid { ColumnSpacing = 8 };
        for (var i = 0; i < cells.Length; i++)
        {
            g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
            g.Add(cells[i], i, 0);
        }
        return g;
    }

    protected static Grid Header(string caption, Func<Task> onClose)
    {
        var g = new Grid();
        g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        var section = Theme.Section(caption);
        section.VerticalOptions = LayoutOptions.Center;
        g.Add(section, 0, 0);
        var close = Theme.Link("✕", onClose);
        SemanticProperties.SetDescription(close, "收合彈窗");
        g.Add(close, 1, 0);
        return g;
    }

    protected static Label Centered(Label l) { l.HorizontalOptions = LayoutOptions.Center; return l; }
}

/// <summary>英雄時刻 · 點名:大倒數 + 立即簽/暫緩/詳細;全帳號簽妥顯示成功態後自關。</summary>
public sealed class HeroRollcallPage : ModalPageBase
{
    readonly Func<Task> _collapse;
    readonly Action _subscribe, _unsubscribe;

    public HeroRollcallPage(AppState state, RollcallVm vm, Func<ContentPage, Task> close, Func<Task> goDetail)
    {
        _collapse = () => close(this);
        var stack = new VerticalStackLayout { Spacing = 12 };

        var big = Centered(Theme.Text("", 46, Theme.FontSemibold, Theme.PrimL, Theme.PrimD));
        var rate = Theme.Dim("", 13);
        rate.VerticalOptions = LayoutOptions.Center;
        void UpdateBig()
        {
            // 未達門檻時沒有倒數:大字改放「即時簽到率」(否則這塊是空的);
            // 達標後大字讓回倒數,簽到率退回下面那行小字。
            if (vm.Holding)
            {
                big.IsVisible = true;
                big.Text = vm.AttendanceRateText;
            }
            else
            {
                big.IsVisible = vm.RemainingSecs.HasValue;
                big.Text = vm.RemainingSecs?.ToString() ?? "";
            }
            rate.Text = $"全班簽到率 {vm.AttendanceRateText}";
            rate.IsVisible = !vm.Holding; // 未達門檻時它就是上面那個大字,不重複顯示
        }

        var chips = new FlexLayout
        {
            Wrap = Microsoft.Maui.Layouts.FlexWrap.Wrap,
            JustifyContent = Microsoft.Maui.Layouts.FlexJustify.Center,
        };
        void BuildChips()
        {
            chips.Children.Clear();
            foreach (var part in vm.Accounts)
            {
                var pill = part.Signed
                    ? Theme.TextPill($"✓ {part.Label}", Theme.OkL, Theme.OkD, Theme.OkBgL, Theme.OkBgD)
                    : Theme.TextPill(part.Label, Theme.DimL, Theme.DimD, Theme.Card2L, Theme.Card2D);
                pill.Margin = new Thickness(3);
                chips.Children.Add(pill);
            }
        }

        var meta = new HorizontalStackLayout { Spacing = 8, HorizontalOptions = LayoutOptions.Center };
        meta.Children.Add(Theme.TextPill(vm.KindText, Theme.PrimL, Theme.PrimD, Theme.PrimBgL, Theme.PrimBgD));
        meta.Children.Add(rate);

        stack.Children.Add(Header("偵測到點名", _collapse));
        stack.Children.Add(Centered(Theme.Strong(vm.Course, 20)));
        stack.Children.Add(meta);
        stack.Children.Add(big);
        stack.Children.Add(new CountdownView(vm, "自動簽到", showRate: false)); // 簽到率由上面的大字負責
        stack.Children.Add(chips);
        stack.Children.Add(Theme.Primary("立即簽到", () => state.SignNow(vm)));
        stack.Children.Add(Row(
            Theme.Ghost("暫緩", () => state.DeferSignIn(vm)),
            Theme.Ghost("詳細", async () => { await close(this); await goDetail(); })));

        void ShowSuccess()
        {
            stack.Children.Clear();
            stack.Children.Add(Centered(Theme.Text("✓ 已完成簽到", 22, Theme.FontSemibold, Theme.OkL, Theme.OkD)));
            stack.Children.Add(Centered(Theme.Dim(vm.Course, 14)));
        }

        var accHandlers = new Dictionary<RollcallAccountVm, PropertyChangedEventHandler>();
        void HookAcc(RollcallAccountVm p)
        {
            if (accHandlers.ContainsKey(p)) return;
            void H(object? _, PropertyChangedEventArgs a) { if (a.PropertyName == nameof(RollcallAccountVm.Signed)) BuildChips(); }
            p.PropertyChanged += H; accHandlers[p] = H;
        }
        void OnAccounts(object? _, NotifyCollectionChangedEventArgs __)
        {
            // 頁面開啟期間帳號可能被移除:先解除+移除已不存在的訂閱,不留 stale VM/事件到 OnDisappearing。
            foreach (var gone in accHandlers.Keys.Where(p => !vm.Accounts.Contains(p)).ToList())
            {
                gone.PropertyChanged -= accHandlers[gone];
                accHandlers.Remove(gone);
            }
            foreach (var p in vm.Accounts) HookAcc(p);
            BuildChips();
        }
        async void OnVm(object? _, PropertyChangedEventArgs a)
        {
            if (a.PropertyName is nameof(RollcallVm.RemainingSecs)
                or nameof(RollcallVm.Holding) or nameof(RollcallVm.AttendanceRate)) { UpdateBig(); return; }
            if (a.PropertyName != nameof(RollcallVm.Status)) return;
            // async void 邊界:例外不得逸出(比照 HeroQuizPage.CloseAfterShowing 的處理)。
            try
            {
                if (vm.IsPending) await close(this);
                else if (vm.IsDone) { ShowSuccess(); await Task.Delay(800); await close(this); }
            }
            catch { /* close 內部已 rollback+Notify */ }
        }

        _subscribe = () =>
        {
            vm.PropertyChanged += OnVm;
            vm.Accounts.CollectionChanged += OnAccounts;
            foreach (var p in vm.Accounts) HookAcc(p);
            BuildChips(); UpdateBig();
            // Captcha may have pre-empted this modal while the rollcall reached a terminal state.
            // Re-check on every appearance; otherwise the queued page reopens with stale controls.
            if (vm.IsPending) _ = close(this);
            else if (vm.IsDone) { ShowSuccess(); _ = CloseCompleted(); }
        };
        async Task CloseCompleted()
        {
            try
            {
                await Task.Delay(800);
                await close(this);
            }
            catch { /* close 內部已 rollback+Notify(比照 HeroQuizPage.CloseAfterShowing) */ }
        }
        _unsubscribe = () =>
        {
            vm.PropertyChanged -= OnVm;
            vm.Accounts.CollectionChanged -= OnAccounts;
            foreach (var kv in accHandlers) kv.Key.PropertyChanged -= kv.Value;
            accHandlers.Clear();
        };

        SetCard(stack);
    }

    protected override void OnAppearing() { base.OnAppearing(); _subscribe(); }
    protected override void OnDisappearing() { base.OnDisappearing(); _unsubscribe(); }
    protected override bool OnBackButtonPressed() { _ = _collapse(); return true; }
}

/// <summary>英雄時刻 · 答題:衝突警示(衝突未定案前鎖送出)+ 倒數 + 送出/暫緩/捨棄/詳細。</summary>
public sealed class HeroQuizPage : ModalPageBase
{
    readonly Func<Task> _collapse;
    readonly Action _subscribe, _unsubscribe;

    public HeroQuizPage(AppState state, QuizVm vm, Func<ContentPage, Task> close, Func<Task> goDetail)
    {
        _collapse = () => close(this);
        var stack = new VerticalStackLayout { Spacing = 12, BindingContext = vm };

        var conflictLabel = Theme.Text("", 13, Theme.FontSemibold, Theme.WarnL, Theme.WarnD);
        conflictLabel.SetBinding(Label.TextProperty, nameof(QuizVm.ConflictText));
        var conflictCard = Theme.TintCard(conflictLabel, Theme.WarnBgL, Theme.WarnBgD, Theme.WarnL, Theme.WarnD);
        conflictCard.SetBinding(IsVisibleProperty, nameof(QuizVm.HasConflicts));

        var big = Centered(Theme.Text("", 46, Theme.FontSemibold, Theme.PrimL, Theme.PrimD));
        void UpdateBig()
        {
            big.IsVisible = vm.RemainingSecs.HasValue;
            big.Text = vm.RemainingSecs?.ToString() ?? "";
        }

        var submit = Theme.Primary("立即送出", () => state.SubmitNow(vm));
        submit.SetBinding(IsEnabledProperty, nameof(QuizVm.CanSubmit));

        stack.Children.Add(Header("偵測到測驗", _collapse));
        stack.Children.Add(Centered(Theme.Strong(vm.Course, 20)));
        stack.Children.Add(Centered(Theme.Dim($"{vm.QuestionCount} 題 · 答案已備妥", 13)));
        stack.Children.Add(conflictCard);
        stack.Children.Add(big);
        stack.Children.Add(new CountdownView(vm, "自動送出"));
        stack.Children.Add(submit);
        stack.Children.Add(Row(
            Theme.Ghost("暫緩", () => state.HoldAnswer(vm)),
            Theme.Danger("捨棄", () => state.DiscardAnswer(vm)),
            Theme.Ghost("詳細", async () => { await close(this); await goDetail(); })));

        // 結束態:全滅結束不得宣稱「已送出」——用中性色與「活動已結束」。
        void ShowEnded()
        {
            stack.Children.Clear();
            var allFailed = vm.SubmittedCount == 0;
            stack.Children.Add(Centered(Theme.Text(
                allFailed ? "活動已結束" : "✓ 已送出", 22, Theme.FontSemibold,
                allFailed ? Theme.DimL : Theme.OkL, allFailed ? Theme.DimD : Theme.OkD)));
            stack.Children.Add(Centered(Theme.Dim(vm.Course, 14)));
        }

        async void OnVm(object? _, PropertyChangedEventArgs a)
        {
            if (a.PropertyName == nameof(QuizVm.RemainingSecs)) { UpdateBig(); return; }
            if (a.PropertyName != nameof(QuizVm.Status)) return;
            // async void 邊界:例外不得逸出(比照 CloseAfterShowing 的處理)。
            try
            {
                if (vm.Status is "held" or "discarded") await close(this);
                else if (vm.Status == "done") { ShowEnded(); await Task.Delay(800); await close(this); }
            }
            catch { /* close 內部已 rollback+Notify */ }
        }
        _subscribe = () =>
        {
            vm.PropertyChanged += OnVm;
            UpdateBig();
            // 備答完成當下就已終結的測驗(例如全部帳號失敗):不給死題上的動作鈕,直接顯示結束態並關閉。
            if (vm.Status == "done") { ShowEnded(); _ = CloseAfterShowing(); }
        };
        _unsubscribe = () => vm.PropertyChanged -= OnVm;

        async Task CloseAfterShowing()
        {
            try
            {
                await Task.Delay(800);
                await close(this);
            }
            catch { /* close 內部已 rollback+Notify */ }
        }

        SetCard(stack);
    }

    protected override void OnAppearing() { base.OnAppearing(); _subscribe(); }
    protected override void OnDisappearing() { base.OnDisappearing(); _unsubscribe(); }
    protected override bool OnBackButtonPressed() { _ = _collapse(); return true; }
}

/// <summary>圖形驗證碼:顯示 core 推來的圖,使用者輸入後 SubmitCaptcha(UI 不做 OCR)。</summary>
public sealed class CaptchaModalPage : ModalPageBase
{
    readonly Func<Task> _cancel;
    readonly Image _image;
    public string AccountId { get; }

    public CaptchaModalPage(AppState state, string accountId, ImageSource image, Func<ContentPage, Task> close)
    {
        AccountId = accountId;
        _cancel = () => close(this);
        var label = state.Accounts.FirstOrDefault(a => a.Id == accountId)?.Label ?? accountId;
        var entry = new Entry { Placeholder = "驗證碼" };
        _image = new Image { Source = image, HeightRequest = 90, Aspect = Aspect.AspectFit };

        // 單 flight:Enter 連發/送出鈕連點只送一次,不會雙送雙關;例外走既有 Notify,不吞。
        var submitting = false;
        async Task Submit()
        {
            if (submitting) return;
            var text = entry.Text?.Trim() ?? "";
            if (text.Length == 0) return;
            submitting = true;
            try
            {
                await state.SubmitCaptcha(accountId, text);
                await close(this);
            }
            catch (Exception error)
            {
                state.Notify("error", $"驗證碼送出失敗:{error.Message}");
            }
            finally { submitting = false; }
        }
        entry.Completed += async (_, _) => await Submit();

        SetCard(new VerticalStackLayout
        {
            Spacing = 12,
            Children =
            {
                Theme.Title("輸入驗證碼"),
                Theme.Dim($"帳號「{label}」登入需要驗證碼。", 13),
                _image,
                entry,
                Theme.Primary("送出", Submit),
                Theme.Link("取消", _cancel),
            },
        });
    }

    /// <summary>同帳號重發驗證碼時只換圖,不疊新窗。</summary>
    public void SetImage(ImageSource image) => _image.Source = image;

    protected override bool OnBackButtonPressed() { _ = _cancel(); return true; }
}
