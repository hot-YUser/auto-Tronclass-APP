using System.Text.Json;
using TronClass.Interop;

namespace Ui;

/// Lists accounts (from the cached Accounts event), with switch/delete, plus add and dashboard.
public class AccountsPage : ContentPage
{
    private readonly VerticalStackLayout _list = new() { Spacing = 8 };

    public AccountsPage()
    {
        Title = "Accounts";
        var add = new Button { Text = "Add account" };
        add.Clicked += async (_, _) => await Navigation.PushAsync(new AddAccountPage());
        var dash = new Button { Text = "Open dashboard" };
        dash.Clicked += async (_, _) => await Navigation.PushAsync(new DashboardPage());

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 24,
                Spacing = 12,
                MaximumWidthRequest = 640,
                Children =
                {
                    new Label { Text = "Accounts", FontSize = 24, FontAttributes = FontAttributes.Bold },
                    add, _list, dash,
                },
            },
        };
        Core.Instance.EventReceived += OnEvent;
    }

    protected override void OnAppearing()
    {
        base.OnAppearing();
        Render();
    }

    private void OnEvent(JsonElement ev)
    {
        if (ev.GetProperty("event").GetString() == "Accounts")
            MainThread.BeginInvokeOnMainThread(Render);
    }

    private void Render()
    {
        _list.Children.Clear();
        if (Core.Instance.LastAccounts is not { } acc)
            return;

        var active = acc.TryGetProperty("active", out var a) && a.ValueKind == JsonValueKind.String ? a.GetString() : null;
        var any = false;
        foreach (var item in acc.GetProperty("accounts").EnumerateArray())
        {
            any = true;
            var id = item.GetProperty("id").GetString()!;
            var label = item.GetProperty("label").GetString();
            var user = item.GetProperty("username").GetString();
            var school = item.GetProperty("school_ref").GetString();
            var isActive = id == active;

            var row = new HorizontalStackLayout { Spacing = 8 };
            row.Children.Add(new Label
            {
                Text = $"{(isActive ? "● " : "○ ")}{label} — {user} @ {school}",
                VerticalOptions = LayoutOptions.Center,
            });
            var use = new Button { Text = "Use" };
            use.Clicked += async (_, _) => await Core.Instance.SwitchAccountAsync(id);
            var del = new Button { Text = "Delete" };
            del.Clicked += async (_, _) => await Core.Instance.DeleteAccountAsync(id);
            row.Children.Add(use);
            row.Children.Add(del);
            _list.Children.Add(row);
        }

        if (!any)
            _list.Children.Add(new Label { Text = "(no accounts yet — add one)", TextColor = Colors.Gray });
    }
}
