using System.Text;
using System.Text.Json;
using TronClass.Interop;

namespace Ui;

public partial class MainPage : ContentPage
{
    private readonly StringBuilder _log = new();

    public MainPage()
    {
        InitializeComponent();
        Core.Instance.EventReceived += OnCoreEvent;
        Core.Instance.Start();
    }

    // Raised on a core worker thread — marshal to the UI thread before touching controls.
    private void OnCoreEvent(JsonElement ev)
    {
        var line = FormatEvent(ev);
        MainThread.BeginInvokeOnMainThread(() =>
        {
            _log.Insert(0, line + "\n");
            if (_log.Length > 4000) _log.Length = 4000;
            LogLabel.Text = _log.ToString();
        });
    }

    private static string FormatEvent(JsonElement ev) => ev.GetProperty("event").GetString() switch
    {
        "Tick" => $"· tick {ev.GetProperty("n").GetInt64()}",
        "StateChanged" => $"state: {ev.GetProperty("state").GetString()}",
        "LogLine" => $"log: {ev.GetProperty("text").GetString()}",
        "Error" => $"ERROR [{ev.GetProperty("code").GetString()}]: {ev.GetProperty("message").GetString()}",
        var other => other ?? "?",
    };

    private async void OnLoginClicked(object? sender, EventArgs e)
    {
        LoginButton.IsEnabled = false;
        ResultLabel.Text = "logging in…";
        try
        {
            // await resumes on the UI thread; the event log keeps ticking meanwhile — proof the
            // UI thread is not blocked while the core does async work over the FFI seam.
            var reply = await Core.Instance.LoginAsync(BaseUrlEntry.Text, UserEntry.Text, PassEntry.Text);
            ResultLabel.Text = reply.GetProperty("ok").GetBoolean()
                ? $"✓ {reply.GetProperty("detail").GetString()}"
                : $"✗ {reply.GetProperty("reason").GetString()}";
        }
        catch (Exception ex)
        {
            ResultLabel.Text = $"✗ {ex.Message}";
        }
        finally
        {
            LoginButton.IsEnabled = true;
        }
    }
}
