using TronClass.Interop;

namespace Ui;

/// <summary>
/// A deliberately minimal placeholder — the ONLY page in this handoff scaffold. It proves the seam
/// works (boots <see cref="ICore"/>, sends a command, streams events) and gives Fable a running app to
/// replace. The real four-tab UI (docs/60-ui-ia.md) is greenfield — build it fresh. This page is throwaway.
/// </summary>
public sealed class MainPage : ContentPage
{
    public MainPage(ICore core)
    {
        var log = new Label { FontSize = 12, LineBreakMode = LineBreakMode.WordWrap };

        var monitor = new Button { Text = "StartMonitoring（跑 MockCore 腳本）" };
        monitor.Clicked += async (_, _) => await core.SendAsync("StartMonitoring");

        Content = new ScrollView
        {
            Content = new VerticalStackLayout
            {
                Padding = 24,
                Spacing = 12,
                Children =
                {
                    new Label { Text = "自動 Tronclass", FontSize = 30, FontAttributes = FontAttributes.Bold },
                    new Label { Text = "UI 地基 · 等 Fable 接手（跑在 MockCore 上，非最終畫面）", FontSize = 13 },
                    monitor,
                    log,
                }
            }
        };

        core.EventReceived += e => MainThread.BeginInvokeOnMainThread(() =>
        {
            var name = e.TryGetProperty("event", out var ev) ? ev.GetString() : "reply";
            log.Text = $"[{name}] {e}\n{log.Text}";
        });

        _ = core.BootAsync(FileSystem.AppDataDirectory);
    }
}
