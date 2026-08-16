using Microsoft.Maui.Controls.Shapes;

namespace Ui;

/// <summary>帳號定義與驗證入口；畫面唯一輸入是完整 MonitoringSnapshot。</summary>
public sealed class AccountsPage : ContentPage
{
    readonly AppState _state;
    readonly VerticalStackLayout _accounts = new() { Spacing = 10 };
    readonly Label _empty = Theme.Dim("尚未新增帳號。", 13);
    readonly Button _addButton;
    bool _attached;

    public AccountsPage(AppState state)
    {
        _state = state;
        Title = "帳號";

        _addButton = Theme.Primary(
            "＋ 新增帳號",
            () => Navigation.PushAsync(new AddAccountPage(state)));

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 16,
                Spacing = 12,
                Children =
                {
                    new StatusBanner(state),
                    _empty,
                    _accounts,
                    _addButton,
                    Theme.Section("其他"),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 10,
                        Children =
                        {
                            NavRow(
                                "設定",
                                "倒數、門檻、全局時間表、時區與 LLM",
                                () => Navigation.PushAsync(new SettingsPage(state))),
                        },
                    }),
                },
            },
        };
    }

    protected override void OnAppearing()
    {
        base.OnAppearing();
        if (_attached) return;
        _attached = true;
        _state.MonitoringChanged += RenderAccounts;
        _state.CommandStateChanged += RenderAccounts;
        RenderAccounts();
    }

    protected override void OnDisappearing()
    {
        base.OnDisappearing();
        if (!_attached) return;
        _attached = false;
        _state.MonitoringChanged -= RenderAccounts;
        _state.CommandStateChanged -= RenderAccounts;
    }

    void RenderAccounts()
    {
        _accounts.Children.Clear();
        var snapshot = _state.Monitoring;
        var accounts = snapshot?.Accounts ?? [];
        _empty.IsVisible = accounts.Length == 0;
        _addButton.IsEnabled = !_state.IsCommandPending("account:add");
        foreach (var account in accounts)
            _accounts.Children.Add(BuildAccountCard(snapshot!, account));
    }

    View BuildAccountCard(MonitoringSnapshotContract snapshot, AccountSnapshotContract account)
    {
        var school = _state.Schools.FirstOrDefault(item => item.Key == account.SchoolRef)?.Label
                     ?? account.SchoolRef;
        var title = new HorizontalStackLayout { Spacing = 8 };
        title.Children.Add(Theme.Strong(account.Label, 15));
        if (account.Role == "teacher")
            title.Children.Add(Theme.TextPill(
                "教師 · QR 輔助",
                Theme.WarnL,
                Theme.WarnD,
                Theme.WarnBgL,
                Theme.WarnBgD));

        var (status, fgLight, fgDark, bgLight, bgDark) = account.LoginState switch
        {
            "logging_in" => ("驗證中…", Theme.WarnL, Theme.WarnD, Theme.WarnBgL, Theme.WarnBgD),
            "online" => ("已驗證", Theme.OkL, Theme.OkD, Theme.OkBgL, Theme.OkBgD),
            "error" => ("驗證失敗", Theme.DangerL, Theme.DangerD, Theme.DangerBgL, Theme.DangerBgD),
            _ => ("尚未驗證", Theme.DimL, Theme.DimD, Theme.Card2L, Theme.Card2D),
        };
        var statusPill = Theme.TextPill(status, fgLight, fgDark, bgLight, bgDark);
        statusPill.HorizontalOptions = LayoutOptions.End;

        var header = new Grid();
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        header.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        header.Add(title, 0, 0);
        header.Add(statusPill, 1, 0);

        var commandKey = $"account:{account.AccountId}:auth";
        var inUse = account.InUseTargets.Length > 0;
        var busy = account.LoginInFlight || _state.IsCommandPending(commandKey);
        var canManage = !inUse && !busy;
        var buttons = new FlexLayout { Wrap = Microsoft.Maui.Layouts.FlexWrap.Wrap };

        void AddButton(Button button)
        {
            button.Margin = new Thickness(0, 0, 8, 0);
            buttons.Children.Add(button);
        }

        var verify = Theme.Ghost(
            account.LoginState == "stored" ? "驗證" : "重新驗證",
            () => _state.Login(account.AccountId));
        verify.IsEnabled = canManage;
        AddButton(verify);

        if (account.LoginState == "error")
        {
            var cookie = Theme.Ghost(
                "Cookie 登入",
                () => Navigation.PushAsync(new CookieImportPage(_state, account.AccountId, account.Label)));
            cookie.IsEnabled = canManage;
            AddButton(cookie);
        }

        var delete = Theme.Danger("刪除帳號", () => DeleteAccount(snapshot, account));
        delete.IsEnabled = canManage &&
                           !_state.IsCommandPending($"account:{account.AccountId}:delete");
        AddButton(delete);

        var body = new VerticalStackLayout
        {
            Spacing = 8,
            Children =
            {
                header,
                Theme.Dim(account.Username, 12.5),
                Theme.Dim(school, 12.5),
            },
        };
        if (account.Role == "teacher")
            body.Children.Add(Theme.Dim(
                string.IsNullOrWhiteSpace(account.TeacherCourseId)
                    ? "主持課程：自動選擇"
                    : $"主持課程：{account.TeacherCourseId}",
                12.5));
        if (account.InUseTargets.Length > 0)
            body.Children.Add(Theme.Text(
                $"正在由：{string.Join("、", account.InUseTargets.Select(target => target.Name))} 使用",
                12.5,
                Theme.FontSemibold,
                Theme.WarnL,
                Theme.WarnD));
        if (account.LoginError is { } error)
            body.Children.Add(Theme.Text(
                error.Message,
                12.5,
                Theme.FontRegular,
                Theme.DangerL,
                Theme.DangerD));
        body.Children.Add(buttons);
        return Theme.Card(body);
    }

    async Task DeleteAccount(
        MonitoringSnapshotContract snapshot,
        AccountSnapshotContract account)
    {
        var groups = snapshot.Targets
            .Where(target => target.Target.Kind == "group" &&
                             target.GroupDefinition?.MemberAccountIds.Contains(
                                 account.AccountId,
                                 StringComparer.Ordinal) == true)
            .Select(target => target.Name)
            .ToArray();
        var detail = groups.Length == 0
            ? "此動作無法復原。"
            : $"同時會從下列群組移除：{string.Join("、", groups)}。";
        if (!await DisplayAlertAsync(
                "刪除帳號",
                $"確定刪除「{account.Label}」？{detail}",
                "刪除",
                "取消"))
            return;
        await _state.DeleteAccount(
            account.AccountId,
            snapshot.ConfigRevision,
            removeFromGroups: true);
    }

    static View NavRow(string title, string subtitle, Func<Task> onTap)
    {
        var grid = new Grid { ColumnSpacing = 8 };
        grid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        grid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        grid.Add(new VerticalStackLayout
        {
            Spacing = 2,
            Children = { Theme.Strong(title, 14), Theme.Dim(subtitle, 12) },
        }, 0, 0);
        var chevron = Theme.Dim("›", 20);
        chevron.VerticalOptions = LayoutOptions.Center;
        grid.Add(chevron, 1, 0);
        grid.OnTap(onTap);
        return grid;
    }
}

/// <summary>新增後立即驗證；登入失敗仍保留剛建立的帳號卡。</summary>
public sealed class AddAccountPage : ContentPage
{
    public AddAccountPage(AppState state)
    {
        Title = "新增帳號";

        var label = new Entry { Placeholder = "名稱（例如：我的東海）" };
        var username = new Entry { Placeholder = "帳號 / 學號信箱" };
        var password = new Entry { Placeholder = "密碼", IsPassword = true };
        const string customOption = "自訂網址…";
        var schoolNames = state.Schools.Select(school => school.Label).Append(customOption).ToList();
        var schoolPicker = new Picker { Title = "學校 / 平台", ItemsSource = schoolNames };
        var defaultIndex = state.Schools.FindIndex(school => school.Key == state.DefaultSchoolKey);
        if (defaultIndex >= 0) schoolPicker.SelectedIndex = defaultIndex;
        var customUrl = new Entry
        {
            Placeholder = "https://…（TronClass 站台網址）",
            IsVisible = false,
        };
        schoolPicker.SelectedIndexChanged += (_, _) =>
            customUrl.IsVisible = schoolPicker.SelectedIndex == schoolNames.Count - 1;

        var isTeacher = new Switch();
        var courseId = new Entry
        {
            Placeholder = "課程 ID（選填，留空自動選擇）",
            Keyboard = Keyboard.Numeric,
            IsVisible = false,
        };
        isTeacher.Toggled += (_, args) => courseId.IsVisible = args.Value;
        var teacherRow = new Grid { ColumnSpacing = 12 };
        teacherRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        teacherRow.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        teacherRow.Add(new VerticalStackLayout
        {
            Spacing = 2,
            Children =
            {
                Theme.Body("教師帳號 QR 輔助"),
                Theme.Dim("教師帳號只協助 QR，不會建立個人監控目標。", 12),
            },
        }, 0, 0);
        teacherRow.Add(isTeacher, 1, 0);

        var error = Theme.Text("", 12.5, Theme.FontRegular, Theme.DangerL, Theme.DangerD);
        error.IsVisible = false;
        Button? submit = null;
        submit = Theme.Primary("新增並驗證", async () =>
        {
            var school = schoolPicker.SelectedIndex >= 0 &&
                         schoolPicker.SelectedIndex < state.Schools.Count
                ? state.Schools[schoolPicker.SelectedIndex].Key
                : customUrl.Text?.Trim() ?? "";
            if (string.IsNullOrWhiteSpace(label.Text) ||
                string.IsNullOrWhiteSpace(username.Text) ||
                string.IsNullOrEmpty(password.Text) ||
                school.Length == 0)
            {
                error.Text = "名稱、平台、帳號與密碼都要填。";
                error.IsVisible = true;
                return;
            }
            error.IsVisible = false;
            submit!.IsEnabled = false;
            var accountId = await state.AddAndVerifyAccount(
                label.Text.Trim(),
                school,
                username.Text.Trim(),
                password.Text,
                isTeacher.IsToggled,
                isTeacher.IsToggled ? courseId.Text : null);
            if (accountId is not null)
                await Navigation.PopAsync();
            else
                submit.IsEnabled = true;
        });

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
                        Children = { label, schoolPicker, customUrl, username, password, error },
                    }),
                    Theme.Section("教師帳號（選用）"),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 10,
                        Children = { teacherRow, courseId },
                    }),
                    submit,
                },
            },
        };
    }
}

/// <summary>瀏覽器 Cookie 登入後備。</summary>
public sealed class CookieImportPage : ContentPage
{
    public CookieImportPage(AppState state, string accountId, string label)
    {
        Title = "Cookie 登入";
        var editor = new Editor
        {
            Placeholder = "貼上 cookies（JSON）",
            HeightRequest = 180,
            AutoSize = EditorAutoSizeOption.Disabled,
        };
        var submit = Theme.Primary("匯入並驗證", async () =>
        {
            var json = editor.Text?.Trim() ?? "";
            if (json.Length > 0 && await state.ImportCookies(accountId, json))
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
                    new StatusBanner(state),
                    Theme.Card(new VerticalStackLayout
                    {
                        Spacing = 10,
                        Children =
                        {
                            Theme.Strong(label, 15),
                            Theme.Dim("在瀏覽器登入 TronClass 後，將該站 cookies 以 JSON 匯出並貼到下方。", 13),
                            editor,
                        },
                    }),
                    submit,
                },
            },
        };
    }
}
