using System.Collections.Specialized;
using System.ComponentModel;
using Microsoft.Maui.Controls.Shapes;

namespace Ui;

/// <summary>答題列表:進行中與近期紀錄(合併後一活動一列)。</summary>
public sealed class QuizListPage : ContentPage
{
    public QuizListPage(AppState state)
    {
        Title = "答題";

        var host = new VerticalStackLayout { Spacing = 10 };
        BindableLayout.SetItemsSource(host, state.Quizzes);
        BindableLayout.SetItemTemplate(host, new DataTemplate(() => new QuizRow()));

        var empty = Theme.Dim("尚未偵測到測驗。開始監控後,新的測驗會即時出現在這裡。", 13);
        void SyncEmpty() => empty.IsVisible = state.Quizzes.Count == 0;
        state.Quizzes.CollectionChanged += (_, _) => SyncEmpty();
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

sealed class QuizRow : Border
{
    readonly VerticalStackLayout _countdownHost = new();

    public QuizRow()
    {
        Padding = 14;
        StrokeThickness = 1;
        StrokeShape = new RoundRectangle { CornerRadius = 16 };
        this.Themed(BackgroundColorProperty, Theme.CardL, Theme.CardD).StrokeThemed(Theme.LineL, Theme.LineD);

        var course = Theme.Strong("", 15);
        course.SetBinding(Label.TextProperty, nameof(QuizVm.Course));
        var sub = Theme.Dim("");
        sub.SetBinding(Label.TextProperty, nameof(QuizVm.SubtitleText));
        var status = Theme.Text("", 12.5, Theme.FontSemibold, Theme.PrimL, Theme.PrimD);
        status.SetBinding(Label.TextProperty, nameof(QuizVm.StatusText));

        var conflictPill = Theme.TextPill("衝突待定案", Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD);
        conflictPill.SetBinding(IsVisibleProperty, nameof(QuizVm.HasConflicts));

        var header = new Grid();
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        header.Add(course, 0, 0);
        header.Add(conflictPill, 1, 0);

        Content = new VerticalStackLayout
        {
            Spacing = 4,
            Children = { header, sub, status, _countdownHost },
        };

        this.OnTap(() => BindingContext is QuizVm vm
            ? ((AppShell)Shell.Current).OpenQuizDetail(vm)
            : Task.CompletedTask);
    }

    protected override void OnBindingContextChanged()
    {
        base.OnBindingContextChanged();
        _countdownHost.Children.Clear();
        if (BindingContext is QuizVm vm)
            _countdownHost.Children.Add(new CountdownView(vm, "自動送出", 12) { Margin = new Thickness(0, 6, 0, 0) });
    }
}

/// <summary>
/// 答題詳細(複閱頁):動作列(衝突未定案前鎖送出)、倒數、帳號切換、逐題卡
/// (衝突高亮 + 定案 + 可展開推理串流)。答案 per-account,各帳號各自送出。
/// </summary>
public sealed class QuizDetailPage : ContentPage
{
    readonly AppState _state;
    readonly QuizVm _vm;
    readonly HorizontalStackLayout _accountChips = new() { Spacing = 8 };
    readonly VerticalStackLayout _questionsHost = new() { Spacing = 10 };
    // per-account 訂閱以具名委派存放,OnDisappearing 全數退訂(頁面被 pop 後不再握住長命 QuizVm)。
    readonly Dictionary<QuizAccountVm, PropertyChangedEventHandler> _accPc = [];
    readonly Dictionary<QuizAccountVm, NotifyCollectionChangedEventHandler> _accQ = [];
    QuizAccountVm? _selected;

    public QuizDetailPage(AppState state, QuizVm vm)
    {
        _state = state;
        _vm = vm;
        Title = vm.Course;
        BindingContext = vm;

        var header = Theme.Card(new VerticalStackLayout
        {
            Spacing = 4,
            Children = { Theme.Strong(vm.Course, 17), SubBound(), StatusBound() },
        });

        var conflictLabel = Theme.Text("", 13, Theme.FontSemibold, Theme.WarnL, Theme.WarnD);
        conflictLabel.SetBinding(Label.TextProperty, nameof(QuizVm.ConflictText));
        var conflictCard = Theme.TintCard(conflictLabel, Theme.WarnBgL, Theme.WarnBgD, Theme.WarnL, Theme.WarnD);
        conflictCard.SetBinding(IsVisibleProperty, nameof(QuizVm.HasConflicts));

        var submit = Theme.Primary("立即送出", () => state.SubmitNow(vm.Id));
        submit.SetBinding(IsEnabledProperty, nameof(QuizVm.CanSubmit));
        var actionRow = new Grid { ColumnSpacing = 8 };
        actionRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        actionRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        actionRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        actionRow.Add(submit, 0, 0);
        actionRow.Add(Theme.Ghost("暫緩", () => state.HoldAnswer(vm)), 1, 0);
        actionRow.Add(Theme.Danger("捨棄", () => state.DiscardAnswer(vm)), 2, 0);

        var actionsCard = Theme.Card(new VerticalStackLayout
        {
            Spacing = 12,
            Children = { new CountdownView(vm, "自動送出", 14), actionRow },
        });
        actionsCard.SetBinding(IsVisibleProperty, nameof(QuizVm.ActionsVisible));

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
                    conflictCard,
                    actionsCard,
                    Theme.Section("帳號(答案各自獨立)"),
                    new ScrollView { Orientation = ScrollOrientation.Horizontal, Content = _accountChips },
                    _questionsHost,
                },
            },
        };
    }

    Label SubBound()
    {
        var l = Theme.Dim("");
        l.SetBinding(Label.TextProperty, nameof(QuizVm.SubtitleText));
        return l;
    }

    Label StatusBound()
    {
        var l = Theme.Text("", 12.5, Theme.FontSemibold, Theme.PrimL, Theme.PrimD);
        l.SetBinding(Label.TextProperty, nameof(QuizVm.StatusText));
        return l;
    }

    protected override void OnAppearing()
    {
        base.OnAppearing();
        _vm.PerAccount.CollectionChanged += OnPerAccountChanged;
        HookAccounts();
        BuildAccountChips();
    }

    protected override void OnDisappearing()
    {
        base.OnDisappearing();
        _vm.PerAccount.CollectionChanged -= OnPerAccountChanged;
        foreach (var acc in _accPc.Keys.ToList()) Unhook(acc);
    }

    void OnPerAccountChanged(object? _, NotifyCollectionChangedEventArgs __) { HookAccounts(); BuildAccountChips(); }

    void HookAccounts()
    {
        foreach (var acc in _vm.PerAccount)
            if (!_accPc.ContainsKey(acc))
            {
                void OnPc(object? _, PropertyChangedEventArgs e) { if (e.PropertyName == nameof(QuizAccountVm.Submitted)) BuildAccountChips(); }
                void OnQ(object? _, NotifyCollectionChangedEventArgs __) { if (acc == _selected) RenderQuestions(); }
                acc.PropertyChanged += OnPc;
                acc.Questions.CollectionChanged += OnQ;
                _accPc[acc] = OnPc;
                _accQ[acc] = OnQ;
            }
    }

    void Unhook(QuizAccountVm acc)
    {
        if (_accPc.Remove(acc, out var pc)) acc.PropertyChanged -= pc;
        if (_accQ.Remove(acc, out var q)) acc.Questions.CollectionChanged -= q;
    }

    void BuildAccountChips()
    {
        _selected ??= _vm.PerAccount.FirstOrDefault();
        if (_selected != null && !_vm.PerAccount.Contains(_selected)) _selected = _vm.PerAccount.FirstOrDefault();

        _accountChips.Children.Clear();
        foreach (var acc in _vm.PerAccount)
        {
            var selected = acc == _selected;
            var text = acc.Submitted ? $"✓ {acc.Label}" : acc.Label;
            var pill = selected
                ? Theme.TextPill(text, Theme.PrimL, Theme.PrimD, Theme.PrimBgL, Theme.PrimBgD)
                : Theme.TextPill(text, Theme.DimL, Theme.DimD, Theme.Card2L, Theme.Card2D);
            pill.Padding = new Thickness(12, 7);
            var captured = acc;
            pill.OnTap(() =>
            {
                _selected = captured;
                BuildAccountChips();
                RenderQuestions();
                return Task.CompletedTask;
            });
            _accountChips.Children.Add(pill);
        }
        RenderQuestions();
    }

    void RenderQuestions()
    {
        _questionsHost.Children.Clear();
        if (_selected is null) return;

        if (_selected.SubmitResult is { } result)
            _questionsHost.Children.Add(Theme.TintCard(
                Theme.Text($"✓ {result}", 13, Theme.FontSemibold, Theme.OkL, Theme.OkD),
                Theme.OkBgL, Theme.OkBgD, Theme.OkL, Theme.OkD));

        var i = 1;
        var accountId = _selected.AccountId;
        foreach (var q in _selected.Questions)
            _questionsHost.Children.Add(new QuestionCard(_state, _vm, accountId, q, i++));
    }
}

/// <summary>
/// 題卡。衝突變體:高亮 + 明示「不會自動覆蓋你的作答」,由使用者 SetAnswer 定案
/// (採用 LLM 建議,或自行輸入)。UI 絕不自行改答案值,一律等 AnswerUpdated 事件。
/// </summary>
sealed class QuestionCard : ContentView
{
    readonly AppState _state;
    readonly QuizVm _quiz;
    readonly string _accountId;
    readonly QuestionVm _q;
    readonly int _index;
    bool _expanded;

    public QuestionCard(AppState state, QuizVm quiz, string accountId, QuestionVm q, int index)
    {
        _state = state;
        _quiz = quiz;
        _accountId = accountId;
        _q = q;
        _index = index;
        Render();

        void OnQ(object? _, System.ComponentModel.PropertyChangedEventArgs a)
        {
            if (a.PropertyName is nameof(QuestionVm.Conflict) or nameof(QuestionVm.Answer) or nameof(QuestionVm.Source))
                Render();
        }
        // 送出/暫緩/捨棄後,衝突互動退回唯讀
        void OnQuiz(object? _, System.ComponentModel.PropertyChangedEventArgs a)
        {
            if (a.PropertyName == nameof(QuizVm.ActionsVisible)) Render();
        }
        // 綁 attach/detach:RenderQuestions 反覆重建題卡,舊卡離開畫面即退訂,不再累積於長命 quiz/question。
        this.WhileAttached(
            () => { q.PropertyChanged += OnQ; quiz.PropertyChanged += OnQuiz; },
            () => { q.PropertyChanged -= OnQ; quiz.PropertyChanged -= OnQuiz; });
    }

    void Render()
    {
        var editable = _q.Conflict && _quiz.ActionsVisible;
        var stack = new VerticalStackLayout { Spacing = 8 };

        var header = new Grid();
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        header.Add(Theme.Dim($"第 {_index} 題", 12.5), 0, 0);
        header.Add(editable
            ? Theme.TextPill("衝突待定案", Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD)
            : Theme.TextPill(_q.SourceText, Theme.PrimL, Theme.PrimD, Theme.PrimBgL, Theme.PrimBgD), 1, 0);

        stack.Children.Add(header);
        stack.Children.Add(Theme.Body(_q.Stem));

        if (!editable)
        {
            stack.Children.Add(Theme.Text(_q.Answer, 16, Theme.FontSemibold, Theme.PrimL, Theme.PrimD));
        }
        else
        {
            stack.Children.Add(Theme.Text(
                "你在 TronClass 已有作答,且與 LLM 建議不同。請選擇最終答案——不會自動覆蓋你的作答。",
                12.5, Theme.FontRegular, Theme.WarnL, Theme.WarnD));

            var llmRow = new Grid { ColumnSpacing = 8 };
            llmRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
            llmRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
            var tag = Theme.Dim("LLM 建議", 12.5);
            tag.VerticalOptions = LayoutOptions.Center;
            llmRow.Add(tag, 0, 0);
            llmRow.Add(Theme.Text(_q.Answer, 16, Theme.FontSemibold, Theme.WarnL, Theme.WarnD), 1, 0);
            stack.Children.Add(llmRow);

            stack.Children.Add(Theme.Primary("採用 LLM 建議", () => _state.SetAnswer(_quiz.Id, _accountId, _q.SubjectId, _q.Answer)));

            var manual = new Entry { Placeholder = "或自行輸入最終答案" };
            var confirmRow = new Grid { ColumnSpacing = 8 };
            confirmRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
            confirmRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
            confirmRow.Add(manual, 0, 0);
            confirmRow.Add(Theme.Ghost("定案", async () =>
            {
                var text = manual.Text?.Trim() ?? "";
                if (text.Length > 0) await _state.SetAnswer(_quiz.Id, _accountId, _q.SubjectId, text);
            }), 1, 0);
            stack.Children.Add(confirmRow);
        }

        // 推理串流(共享 ReasoningVm,逐字增長)
        if (_q.Reasoning is { } reasoning)
        {
            var toggle = Theme.Dim(_expanded ? "▾ 推理過程" : "▸ 推理過程", 12.5);
            var body = Theme.Dim("", 12);
            body.BindingContext = reasoning;
            body.SetBinding(Label.TextProperty, nameof(ReasoningVm.Text));
            body.IsVisible = _expanded;
            toggle.OnTap(() =>
            {
                _expanded = !_expanded;
                toggle.Text = _expanded ? "▾ 推理過程" : "▸ 推理過程";
                body.IsVisible = _expanded;
                return Task.CompletedTask;
            });
            stack.Children.Add(toggle);
            stack.Children.Add(body);
        }

        Content = editable
            ? Theme.TintCard(stack, Theme.WarnBgL, Theme.WarnBgD, Theme.WarnL, Theme.WarnD, 16)
            : Theme.Card(stack);
    }
}
