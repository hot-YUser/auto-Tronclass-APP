using Microsoft.Maui.Controls.Shapes;

namespace Ui;

/// <summary>帳號分頁:多帳號列表(切換/登入/刪除/cookie 後備)、新增入口、設定入口、鎖定保險庫。</summary>
public sealed class AccountsPage : ContentPage
{
    public AccountsPage(AppState state)
    {
        Title = "帳號";

        var host = new VerticalStackLayout { Spacing = 10 };
        BindableLayout.SetItemsSource(host, state.Accounts);
        BindableLayout.SetItemTemplate(host, new DataTemplate(() => new AccountCard(state, this)));

        var empty = Theme.Dim("尚未新增帳號。", 13);
        void SyncEmpty() => empty.IsVisible = state.Accounts.Count == 0;
        state.Accounts.CollectionChanged += (_, _) => SyncEmpty();
        SyncEmpty();

        var settingsRow = NavRow("設定", "倒數秒數、防假點名門檻、LLM 金鑰",
            () => Navigation.PushAsync(new SettingsPage(state)));
        var lockRow = NavRow("鎖定保險庫", "回到主密碼畫面", state.LockVault);

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    empty,
                    host,
                    Theme.Primary("＋ 新增帳號", () => Navigation.PushAsync(new AddAccountPage(state))),
                    Theme.Section("其他"),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 10,
                        Children = { settingsRow, Theme.Divider(), lockRow },
                    }),
                },
            },
        };
    }

    static View NavRow(string title, string sub, Func<Task> onTap)
    {
        var g = new Grid { ColumnSpacing = 8 };
        g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        g.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        g.Add(new VerticalStackLayout
        {
            Spacing = 2,
            Children = { Theme.Strong(title, 14), Theme.Dim(sub, 12) },
        }, 0, 0);
        var chevron = Theme.Dim("›", 20);
        chevron.VerticalOptions = LayoutOptions.Center;
        g.Add(chevron, 1, 0);
        g.OnTap(onTap);
        return g;
    }
}

/// <summary>單一帳號卡:狀態燈、使用中標記、切換/登入/刪除;登入失敗亮出 cookie 後備。</summary>
sealed class AccountCard : Border
{
    readonly AppState _state;
    readonly Page _page;
    AccountVm? _hookedVm;
    System.ComponentModel.PropertyChangedEventHandler? _handler;

    public AccountCard(AppState state, Page page)
    {
        _state = state;
        _page = page;
        Padding = 14;
        StrokeThickness = 1;
        StrokeShape = new RoundRectangle { CornerRadius = 16 };
        this.Themed(BackgroundColorProperty, Theme.CardL, Theme.CardD).StrokeThemed(Theme.LineL, Theme.LineD);
        Unloaded += (_, _) => Unhook();
    }

    void Unhook()
    {
        if (_hookedVm is not null && _handler is not null) _hookedVm.PropertyChanged -= _handler;
        _hookedVm = null; _handler = null;
    }

    protected override void OnBindingContextChanged()
    {
        base.OnBindingContextChanged();
        Unhook();
        if (BindingContext is not AccountVm vm) return;
        _handler = (_, a) => { if (a.PropertyName is nameof(AccountVm.State) or nameof(AccountVm.IsActive)) Render(vm); };
        vm.PropertyChanged += _handler;
        _hookedVm = vm;
        Render(vm);
    }

    void Render(AccountVm vm)
    {
        var school = _state.Schools.FirstOrDefault(s => s.Key == vm.SchoolRef)?.Label ?? vm.SchoolRef;

        var titleRow = new HorizontalStackLayout { Spacing = 8 };
        titleRow.Children.Add(Theme.Strong(vm.Label, 15));
        if (vm.IsActive)
            titleRow.Children.Add(Theme.TextPill("使用中", Theme.PrimL, Theme.PrimD, Theme.PrimBgL, Theme.PrimBgD));

        var (fgL, fgD, bgL, bgD) = vm.State switch
        {
            "online" => (Theme.OkL, Theme.OkD, Theme.OkBgL, Theme.OkBgD),
            "login_failed" => (Theme.DangerL, Theme.DangerD, Theme.DangerBgL, Theme.DangerBgD),
            _ => (Theme.DimL, Theme.DimD, Theme.Card2L, Theme.Card2D),
        };
        var statusPill = Theme.TextPill(vm.StateText, fgL, fgD, bgL, bgD);
        statusPill.HorizontalOptions = LayoutOptions.End;

        var header = new Grid();
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        header.Add(titleRow, 0, 0);
        header.Add(statusPill, 1, 0);

        var buttons = new FlexLayout { Wrap = Microsoft.Maui.Layouts.FlexWrap.Wrap };
        void AddBtn(Button b) { b.Margin = new Thickness(0, 0, 8, 0); buttons.Children.Add(b); }

        if (!vm.IsActive) AddBtn(Theme.Ghost("切換", () => _state.SwitchAccount(vm.Id)));
        AddBtn(Theme.Ghost("登入", () => _state.Login(vm.Id)));
        if (vm.LoginFailed)
            AddBtn(Theme.Ghost("Cookie 登入", () => _page.Navigation.PushAsync(new CookieImportPage(_state, vm))));
        AddBtn(Theme.Danger("刪除", async () =>
        {
            if (await _page.DisplayAlertAsync("刪除帳號", $"確定刪除「{vm.Label}」?此動作無法復原。", "刪除", "取消"))
                await _state.DeleteAccount(vm.Id);
        }));

        var stack = new VerticalStackLayout { Spacing = 8, Children = { header, Theme.Dim(vm.Username, 12.5) } };
        if (!string.IsNullOrEmpty(school)) stack.Children.Add(Theme.Dim(school, 12.5));
        if (vm.Error is { Length: > 0 } err)
            stack.Children.Add(Theme.Text(err, 12.5, Theme.FontRegular, Theme.DangerL, Theme.DangerD));
        stack.Children.Add(buttons);
        Content = stack;
    }
}

/// <summary>新增帳號:名稱、學校(登錄表 + 自訂網址)、帳號、密碼。</summary>
public sealed class AddAccountPage : ContentPage
{
    public AddAccountPage(AppState state)
    {
        Title = "新增帳號";

        var label = new Entry { Placeholder = "名稱(例如:我的東海)" };
        var username = new Entry { Placeholder = "帳號 / 學號信箱" };
        var password = new Entry { Placeholder = "密碼", IsPassword = true };

        const string customOption = "自訂網址…";
        var schoolNames = state.Schools.Select(s => s.Label).Append(customOption).ToList();
        var picker = new Picker { Title = "學校 / 平台", ItemsSource = schoolNames };
        var defaultIdx = state.Schools.FindIndex(s => s.Key == state.DefaultSchoolKey);
        if (defaultIdx >= 0) picker.SelectedIndex = defaultIdx;

        var customUrl = new Entry { Placeholder = "https://…(TronClass 站台網址)", IsVisible = false };
        picker.SelectedIndexChanged += (_, _) =>
            customUrl.IsVisible = picker.SelectedIndex == schoolNames.Count - 1;

        var error = Theme.Text("", 12.5, Theme.FontRegular, Theme.DangerL, Theme.DangerD);
        error.IsVisible = false;

        var submit = Theme.Primary("新增", async () =>
        {
            var school = picker.SelectedIndex >= 0 && picker.SelectedIndex < state.Schools.Count
                ? state.Schools[picker.SelectedIndex].Key
                : customUrl.Text?.Trim() ?? "";
            if (string.IsNullOrWhiteSpace(label.Text) || string.IsNullOrWhiteSpace(username.Text) ||
                string.IsNullOrEmpty(password.Text) || school.Length == 0)
            {
                error.Text = "每個欄位都要填。";
                error.IsVisible = true;
                return;
            }
            error.IsVisible = false;
            if (await state.AddAccount(label.Text.Trim(), school, username.Text.Trim(), password.Text))
                await Navigation.PopAsync();
        });

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 10,
                        Children = { label, picker, customUrl, username, password, error },
                    }),
                    submit,
                },
            },
        };
    }
}

/// <summary>瀏覽器 cookie 登入後備(密碼登入失敗時)。</summary>
public sealed class CookieImportPage : ContentPage
{
    public CookieImportPage(AppState state, AccountVm vm)
    {
        Title = "Cookie 登入";

        var editor = new Editor
        {
            Placeholder = "貼上 cookies(JSON)",
            HeightRequest = 180,
            AutoSize = EditorAutoSizeOption.Disabled,
        };

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 10,
                        Children =
                        {
                            Theme.Strong(vm.Label, 15),
                            Theme.Dim("密碼登入失敗時的後備:在瀏覽器登入 TronClass 後,將該站 cookies 以 JSON 匯出貼到下方。", 13),
                            editor,
                        },
                    }),
                    Theme.Primary("匯入並登入", async () =>
                    {
                        var json = editor.Text?.Trim() ?? "";
                        if (json.Length == 0) return;
                        if (await state.ImportCookies(vm.Id, json))
                            await Navigation.PopAsync();
                    }),
                },
            },
        };
    }
}
