namespace Ui;

/// <summary>
/// 四分頁 Shell(tab 樣式交給 MAUI 原生自動適配)+ 單一堆疊的 modal 協調:
/// 同時只掛一個 modal;驗證碼會**搶佔**當前彈窗(把它退回佇列稍後重顯),
/// 一般英雄彈窗則排隊;驗證碼同帳號去重(更新圖不疊窗)。
/// 所有狀態變更與導覽呼叫都在同一把 <see cref="_lock"/> 內完成:導覽失敗時
/// <c>_current</c>/佇列一致回滾並 Notify,事件邊界不留 unobserved task,
/// close 的 pop 完成後才 push 下一個(動畫競態不疊舊窗)。
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
        tabs.Items.Add(Tab("監控", "tab_home.png", () => new HomePage(state)));
        tabs.Items.Add(_tabRollcall = Tab("點名", "tab_rollcall.png", () => new RollcallListPage(state)));
        tabs.Items.Add(_tabQuiz = Tab("答題", "tab_quiz.png", () => new QuizListPage(state)));
        tabs.Items.Add(Tab("帳號", "tab_accounts.png", () => new AccountsPage(state)));
        Items.Add(tabs);

        state.HeroRollcall += vm => Fire(() => ShowModal(new HeroRollcallPage(state, vm, CloseModal, () => OpenRollcallDetail(vm))));
        state.HeroQuiz += vm => Fire(() => ShowModal(new HeroQuizPage(state, vm, CloseModal, () => OpenQuizDetail(vm))));
        state.CaptchaRequested += OnCaptcha;
    }

    static ShellContent Tab(string title, string icon, Func<Page> create) =>
        new() { Title = title, Icon = icon, ContentTemplate = new DataTemplate(create) };

    protected override void OnAppearing()
    {
        base.OnAppearing();
        if (_booted && _state.BootReady) return;
        _booted = true;
        Dispatcher.Dispatch(async () =>
        {
            await _state.BootAsync();
            if (!_state.BootReady) _booted = false;
        });
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

    void OnCaptcha(string accountId, ImageSource img) => Fire(() => ShowCaptchaAsync(accountId, img));

    /// <summary>
    /// 驗證碼入口。同帳號去重在 lock 內重評估:併發/連續重發只換圖、不疊窗
    /// (去重判斷與狀態變更不可分開,否則兩個請求可能同時判定「不存在」而疊兩窗)。
    /// </summary>
    async Task ShowCaptchaAsync(string accountId, ImageSource img)
    {
        await _lock.WaitAsync();
        try
        {
            var existing = Captchas().FirstOrDefault(c => c.AccountId == accountId);
            if (existing is not null) { existing.SetImage(img); return; }
            await ShowModalCore(new CaptchaModalPage(_state, accountId, img, CloseModal), preempt: true);
        }
        finally { _lock.Release(); }
    }

    IEnumerable<CaptchaModalPage> Captchas()
    {
        if (_current is CaptchaModalPage c) yield return c;
        foreach (var p in _queue) if (p is CaptchaModalPage q) yield return q;
    }

    async Task ShowModal(ContentPage page, bool preempt = false)
    {
        await _lock.WaitAsync();
        try { await ShowModalCore(page, preempt); }
        finally { _lock.Release(); }
    }

    /// <summary>
    /// 須已持 <see cref="_lock"/>。導覽失敗時把 <c>_current</c>/佇列滾回與平台棧一致
    /// (不變量:<c>_current</c> 若非 null 必為平台棧頂)並 Notify,不吞例外。
    /// </summary>
    async Task ShowModalCore(ContentPage page, bool preempt)
    {
        if (ReferenceEquals(_current, page) || _queue.Contains(page)) return; // 已在場(重入保護)

        ContentPage? displaced = null;
        var displacedPopped = false;
        try
        {
            if (_current is null)
            {
                _current = page;
                await Navigation.PushModalAsync(page);
            }
            else if (preempt)
            {
                displaced = _current; // 被搶佔者退回佇列前端,稍後重顯
                _current = page;
                await Navigation.PopModalAsync(animated: false);
                displacedPopped = true;
                _queue.Insert(0, displaced);
                await Navigation.PushModalAsync(page);
            }
            else
            {
                _queue.Add(page);
            }
        }
        catch (Exception error)
        {
            if (displaced is not null)
            {
                if (displacedPopped)
                {
                    // 平台已把 displaced 彈掉而新窗推不上去:重推 displaced 維持不變量;
                    // 再失敗則交回佇列前端,由下次 Drain 重顯。
                    _queue.Remove(displaced);
                    _current = displaced;
                    try { await Navigation.PushModalAsync(displaced); }
                    catch { _current = null; _queue.Insert(0, displaced); }
                }
                else _current = displaced; // pop 失敗:平台仍顯示 displaced,狀態不變
            }
            else
            {
                if (_current == page) _current = null; // 推送失敗:不再宣稱它在顯示
                _queue.Remove(page);
            }
            _state.Notify("error", $"彈窗顯示失敗:{error.Message}");
        }
    }

    async Task CloseModal(ContentPage page)
    {
        await _lock.WaitAsync();
        try
        {
            if (ReferenceEquals(_current, page))
            {
                try
                {
                    _current = null;
                    await Navigation.PopModalAsync(); // 等 pop(含動畫)完成,Drain 的 push 不會疊上舊窗
                }
                catch (Exception error)
                {
                    _current = page; // pop 失敗:回滾成原狀,維持「_current == 平台棧頂」
                    _state.Notify("error", $"彈窗關閉失敗:{error.Message}");
                }
            }
            else _queue.Remove(page); // 已排隊未顯示(或已被關過):直接撤下
        }
        finally { _lock.Release(); }
        await Drain();
    }

    async Task Drain()
    {
        await _lock.WaitAsync();
        try
        {
            if (_current is null && _queue.Count > 0)
            {
                var next = _queue[0];
                _queue.RemoveAt(0);
                _current = next;
                try { await Navigation.PushModalAsync(next); }
                catch (Exception error)
                {
                    _current = null; // 回滾成 Drain 前的狀態,下一次關閉會再嘗試
                    _queue.Insert(0, next);
                    _state.Notify("error", $"彈窗顯示失敗:{error.Message}");
                }
            }
        }
        finally { _lock.Release(); }
    }

    /// <summary>事件邊界 fire-and-forget:操作內部已 rollback+Notify,這裡確保 task 永不 unobserved。</summary>
    static void Fire(Func<Task> action) => _ = FireAsync(action);

    static async Task FireAsync(Func<Task> action)
    {
        try { await action(); }
        catch (Exception error) { System.Diagnostics.Debug.WriteLine($"AppShell:{error}"); } // 不應到達:內部全處理
    }
}
