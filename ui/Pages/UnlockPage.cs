using System.Text.Json;
using Microsoft.Maui.Storage;
using TronClass.Interop;

namespace Ui;

/// First screen: boots the core, then creates (first run) or unlocks the vault.
public class UnlockPage : ContentPage
{
    private readonly Entry _pw = new() { Placeholder = "master password", IsPassword = true };
    private readonly Button _go = new() { Text = "Unlock" };
    private readonly Label _status = new();
    private bool _createMode;

    public static string DataDir => FileSystem.AppDataDirectory;

    public UnlockPage()
    {
        Title = "Vault";
        _go.Clicked += OnGo;
        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 24,
                Spacing = 12,
                MaximumWidthRequest = 560,
                Children =
                {
                    new Label { Text = "TronClass", FontSize = 26, FontAttributes = FontAttributes.Bold },
                    new Label { Text = "Unlock the encrypted vault to continue.", TextColor = Colors.Gray },
                    _pw, _go, _status,
                },
            },
        };
        Core.Instance.EventReceived += OnEvent;
    }

    protected override async void OnAppearing()
    {
        base.OnAppearing();
        await Core.Instance.BootAsync(DataDir);
        ApplyVaultState();
    }

    private void OnEvent(JsonElement ev)
    {
        if (ev.GetProperty("event").GetString() == "VaultState")
            MainThread.BeginInvokeOnMainThread(ApplyVaultState);
    }

    private void ApplyVaultState()
    {
        var exists = Core.Instance.LastVaultState?.GetProperty("exists").GetBoolean() ?? false;
        _createMode = !exists;
        _go.Text = exists ? "Unlock" : "Create vault";
        Title = exists ? "Unlock vault" : "Create vault";
    }

    private async void OnGo(object? sender, EventArgs e)
    {
        _go.IsEnabled = false;
        _status.Text = "…";
        var reply = _createMode
            ? await Core.Instance.CreateVaultAsync(_pw.Text ?? "")
            : await Core.Instance.UnlockAsync(_pw.Text ?? "");

        if (reply.GetProperty("ok").GetBoolean())
        {
            Core.Instance.EventReceived -= OnEvent;
            await Navigation.PushAsync(new AccountsPage());
        }
        else
        {
            _status.Text = "✗ " + (reply.TryGetProperty("error", out var er) ? er.GetString() : "failed");
            _go.IsEnabled = true;
        }
    }
}
