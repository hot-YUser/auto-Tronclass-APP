using System.Text;
using System.Text.Json;
using TronClass.Interop;

namespace Ui;

/// Shows the active account and performs the real login for it; streams core events into a log.
public class DashboardPage : ContentPage
{
    private readonly Label _active = new() { FontAttributes = FontAttributes.Bold };
    private readonly Button _login = new() { Text = "Login active account" };
    private readonly Label _result = new() { FontSize = 18, FontAttributes = FontAttributes.Bold };
    private readonly Label _log = new() { FontFamily = "Monospace", FontSize = 12, TextColor = Colors.Gray };
    private readonly StringBuilder _logText = new();
    private string? _activeId;

    public DashboardPage()
    {
        Title = "Dashboard";
        _login.Clicked += OnLogin;
        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 24,
                Spacing = 12,
                MaximumWidthRequest = 640,
                Children =
                {
                    new Label { Text = "Dashboard", FontSize = 24, FontAttributes = FontAttributes.Bold },
                    _active, _login, _result,
                    new Label { Text = "Events (live from core)", FontAttributes = FontAttributes.Bold, Margin = new Thickness(0, 12, 0, 0) },
                    _log,
                },
            },
        };
        Core.Instance.EventReceived += OnEvent;
    }

    protected override void OnAppearing()
    {
        base.OnAppearing();
        RenderActive();
    }

    private void RenderActive()
    {
        _activeId = null;
        string? label = null, school = null;
        if (Core.Instance.LastAccounts is { } acc)
        {
            _activeId = acc.TryGetProperty("active", out var a) && a.ValueKind == JsonValueKind.String ? a.GetString() : null;
            foreach (var it in acc.GetProperty("accounts").EnumerateArray())
            {
                if (it.GetProperty("id").GetString() == _activeId)
                {
                    label = it.GetProperty("label").GetString();
                    school = it.GetProperty("school_ref").GetString();
                }
            }
        }
        _active.Text = _activeId is null ? "(no active account)" : $"Active: {label} @ {school}";
        _login.IsEnabled = _activeId is not null;
    }

    private void OnEvent(JsonElement ev)
    {
        MainThread.BeginInvokeOnMainThread(() =>
        {
            if (ev.GetProperty("event").GetString() == "Accounts")
                RenderActive();
            if (Format(ev) is { } line)
            {
                _logText.Insert(0, line + "\n");
                if (_logText.Length > 4000) _logText.Length = 4000;
                _log.Text = _logText.ToString();
            }
        });
    }

    private static string? Format(JsonElement ev) => ev.GetProperty("event").GetString() switch
    {
        "Tick" => $"· tick {ev.GetProperty("n").GetInt64()}",
        "StateChanged" => $"state: {ev.GetProperty("state").GetString()}",
        "LogLine" => $"log: {ev.GetProperty("text").GetString()}",
        "Error" => $"ERROR [{ev.GetProperty("code").GetString()}]: {ev.GetProperty("message").GetString()}",
        _ => null,
    };

    private async void OnLogin(object? sender, EventArgs e)
    {
        if (_activeId is null) return;
        _login.IsEnabled = false;
        _result.Text = "logging in…";
        try
        {
            var r = await Core.Instance.LoginAsync(_activeId);
            _result.Text = r.GetProperty("ok").GetBoolean()
                ? $"✓ {r.GetProperty("detail").GetString()}"
                : $"✗ {r.GetProperty("reason").GetString()}";
        }
        catch (Exception ex)
        {
            _result.Text = $"✗ {ex.Message}";
        }
        finally
        {
            _login.IsEnabled = true;
        }
    }
}
