namespace Ui;

/// <summary>
/// 四分頁 Shell(tab 樣式交給 MAUI 原生自動適配)+ 單一堆疊的 modal 協調:
/// 同時只掛一個 modal;驗證碼會**搶佔**當前彈窗(把它退回佇列稍後重顯),
/// 一般英雄彈窗則排隊;驗證碼同帳號去重(更新圖不疊窗)。
/// </summary>
public sealed class AppShell : Shell
{
    readonly AppState _state;
    readonly List<ContentPage> _queue = [];
    readonly SemaphoreSlim _lock = new(1, 1);
    readonly ShellContent _tabRollcall, _tabQuiz;
    ContentPage? _current;
    bool _booted;

    public AppShell(AppState state)
    {
        _state = state;
        Title = "自動 Tronclass";

        var tabs = new TabBar();
        tabs.Items.Add(Tab("首頁", "tab_home.png", () => new HomePage(state)));
        tabs.Items.Add(_tabRollcall = Tab("點名", "tab_rollcall.png", () => new RollcallListPage(state)));
        tabs.Items.Add(_tabQuiz = Tab("答題", "tab_quiz.png", () => new QuizListPage(state)));
        tabs.Items.Add(Tab("帳號", "tab_accounts.png", () => new AccountsPage(state)));
        Items.Add(tabs);

        state.HeroRollcall += vm => _ = ShowModal(new HeroRollcallPage(state, vm, CloseModal, () => OpenRollcallDetail(vm)));
        state.HeroQuiz += vm => _ = ShowModal(new HeroQuizPage(state, vm, CloseModal, () => OpenQuizDetail(vm)));
        state.CaptchaRequested += OnCaptcha;
    }

    static ShellContent Tab(string title, string icon, Func<Page> create) =>
        new() { Title = title, Icon = icon, ContentTemplate = new DataTemplate(create) };

    protected override void OnAppearing()
    {
        base.OnAppearing();
        if (_booted) return;
        _booted = true;
        Dispatcher.Dispatch(async () => await _state.BootAsync());
    }

    public Task OpenRollcallDetail(RollcallVm vm)
    {
        CurrentItem = _tabRollcall;
        return Navigation.PushAsync(new RollcallDetailPage(_state, vm));
    }

    public Task OpenQuizDetail(QuizVm vm)
    {
        CurrentItem = _tabQuiz;
        return Navigation.PushAsync(new QuizDetailPage(_state, vm));
    }

    // ---------------- modal 協調 ----------------

    void OnCaptcha(string accountId, ImageSource img)
    {
        // 去重:同帳號的驗證碼已在顯示或排隊 → 更新圖、不疊窗
        var existing = Captchas().FirstOrDefault(c => c.AccountId == accountId);
        if (existing is not null) { existing.SetImage(img); return; }
        _ = ShowModal(new CaptchaModalPage(_state, accountId, img, CloseModal), preempt: true);
    }

    IEnumerable<CaptchaModalPage> Captchas()
    {
        if (_current is CaptchaModalPage c) yield return c;
        foreach (var p in _queue) if (p is CaptchaModalPage q) yield return q;
    }

    async Task ShowModal(ContentPage page, bool preempt = false)
    {
        await _lock.WaitAsync();
        try
        {
            if (ReferenceEquals(_current, page) || _queue.Contains(page)) return; // 已在場(重入保護)
            if (_current is null)
            {
                _current = page;
                await Navigation.PushModalAsync(page);
            }
            else if (preempt)
            {
                var displaced = _current; // 被搶佔者退回佇列前端,稍後重顯
                _current = page;
                await Navigation.PopModalAsync(animated: false);
                _queue.Insert(0, displaced);
                await Navigation.PushModalAsync(page);
            }
            else
            {
                _queue.Add(page);
            }
        }
        finally { _lock.Release(); }
    }

    async Task CloseModal(ContentPage page)
    {
        await _lock.WaitAsync();
        try
        {
            if (ReferenceEquals(_current, page)) { _current = null; await Navigation.PopModalAsync(); }
            else _queue.Remove(page); // 已排隊未顯示(或已被關過):直接撤下
        }
        finally { _lock.Release(); }
        await Drain();
    }

    async Task Drain()
    {
        ContentPage? next = null;
        await _lock.WaitAsync();
        try
        {
            if (_current is null && _queue.Count > 0) { next = _queue[0]; _queue.RemoveAt(0); _current = next; }
        }
        finally { _lock.Release(); }
        if (next is not null) await Navigation.PushModalAsync(next);
    }
}
