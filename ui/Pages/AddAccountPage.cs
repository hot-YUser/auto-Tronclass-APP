using System.Text.Json;
using TronClass.Interop;

namespace Ui;

/// Adds an account: pick a registered school OR type a raw base_url, plus credentials. The
/// password goes straight to the vault (via the AddAccount command) — never into config.
public class AddAccountPage : ContentPage
{
    private readonly Entry _label = new() { Placeholder = "label (e.g. My University)" };
    private readonly Picker _school = new() { Title = "School (optional — from registry)" };
    private readonly Entry _baseUrl = new() { Placeholder = "or base_url (https://ilearn.example.edu)" };
    private readonly Entry _user = new() { Placeholder = "username" };
    private readonly Entry _pass = new() { Placeholder = "password", IsPassword = true };
    private readonly Label _status = new();
    private readonly List<string> _schoolKeys = new();

    public AddAccountPage()
    {
        Title = "Add account";
        var save = new Button { Text = "Add" };
        save.Clicked += OnSave;

        PopulateSchools();

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 24,
                Spacing = 12,
                MaximumWidthRequest = 560,
                Children =
                {
                    new Label { Text = "New account", FontSize = 24, FontAttributes = FontAttributes.Bold },
                    _label, _school, _baseUrl, _user, _pass, save, _status,
                },
            },
        };
    }

    private void PopulateSchools()
    {
        _school.Items.Clear();
        _schoolKeys.Clear();
        if (Core.Instance.LastProviders is { } prov)
        {
            foreach (var s in prov.GetProperty("schools").EnumerateArray())
            {
                _school.Items.Add(s.GetProperty("label").GetString());
                _schoolKeys.Add(s.GetProperty("key").GetString()!);
            }
        }
        // Seed ships empty → the Picker is usually empty and base_url is the path used.
        _school.IsVisible = _school.Items.Count > 0;
    }

    private async void OnSave(object? sender, EventArgs e)
    {
        // A picked school wins; otherwise the raw base_url the user typed.
        var school = _school.SelectedIndex >= 0 ? _schoolKeys[_school.SelectedIndex] : (_baseUrl.Text ?? "").Trim();
        if (string.IsNullOrWhiteSpace(school))
        {
            _status.Text = "✗ pick a school or enter a base_url";
            return;
        }

        var reply = await Core.Instance.AddAccountAsync(_label.Text ?? school, school, _user.Text ?? "", _pass.Text ?? "");
        if (reply.GetProperty("ok").GetBoolean())
            await Navigation.PopAsync();
        else
            _status.Text = "✗ " + (reply.TryGetProperty("error", out var er) ? er.GetString() : "failed");
    }
}
