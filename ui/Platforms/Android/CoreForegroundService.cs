using Android.App;
using Android.Content;
using Android.OS;
using Android.Runtime;
using AndroidX.Core.App;
using Microsoft.Extensions.DependencyInjection;
using System.Text.Json;
using TronClass.Interop;

namespace Ui;

/// <summary>
/// Keeps the process alive when the app is backgrounded, so the core's monitor loop keeps
/// running (docs 10 — domain-alive vs process-alive). It owns nothing of the core's logic; it
/// only holds the process open and mirrors the heartbeat to logcat + the ongoing notification,
/// which is how "still ticking in the background" is observable without the UI.
/// </summary>
[Service(Exported = false, ForegroundServiceType = global::Android.Content.PM.ForegroundService.TypeDataSync)]
public class CoreForegroundService : Service
{
    private const string ChannelId = "tronclass_monitor";
    private const int NotificationId = 1;
    private ICore? _core;

    public override IBinder? OnBind(Intent? intent) => null;

    public override StartCommandResult OnStartCommand(Intent? intent, StartCommandFlags flags, int startId)
    {
        CreateChannel();

        // API 34+ enforces this: the service must declare a foregroundServiceType (the [Service]
        // attribute emits android:foregroundServiceType="dataSync"), hold FOREGROUND_SERVICE_DATA_SYNC,
        // AND pass the type into startForeground — else it throws. The 3-arg overload is API 29+.
        var notification = BuildNotification("監控中 · idle");
        if (OperatingSystem.IsAndroidVersionAtLeast(29))
            StartForeground(NotificationId, notification, global::Android.Content.PM.ForegroundService.TypeDataSync);
        else
            StartForeground(NotificationId, notification);

        // The UI boots the core; here we just resolve the same singleton (via the MAUI DI container)
        // and mirror its heartbeat. BootAsync is idempotent — a safety net if the service outlives the UI.
        _core = IPlatformApplication.Current?.Services.GetService<ICore>();
        if (_core is not null)
        {
            _core.EventReceived += OnCoreEvent;
            _ = _core.BootAsync(FileSystem.AppDataDirectory);
        }

        return StartCommandResult.Sticky;
    }

    public override void OnDestroy()
    {
        if (_core is not null) _core.EventReceived -= OnCoreEvent;
        base.OnDestroy();
    }

    private void OnCoreEvent(JsonElement ev)
    {
        if (!ev.TryGetProperty("event", out var evName) || evName.GetString() != "Tick") return;
        var n = ev.GetProperty("n").GetInt64();
        global::Android.Util.Log.Info("tronclass", $"heartbeat tick {n}");
        var mgr = (NotificationManager)GetSystemService(NotificationService)!;
        mgr.Notify(NotificationId, BuildNotification($"監控中 · tick {n}"));
    }

    private Notification BuildNotification(string text)
    {
        // Set on the builder as statements: the chained setters are annotated nullable, and
        // the builder mutates in place, so this avoids the noisy null-deref warnings.
        var b = new NotificationCompat.Builder(this, ChannelId);
        b.SetContentTitle("自動 Tronclass");
        b.SetContentText(text);
        b.SetSmallIcon(global::Android.Resource.Drawable.IcDialogInfo);
        b.SetOngoing(true);
        return b.Build()!;
    }

    private void CreateChannel()
    {
        if (!OperatingSystem.IsAndroidVersionAtLeast(26)) return;
        var channel = new NotificationChannel(ChannelId, "Monitoring", NotificationImportance.Low);
        ((NotificationManager)GetSystemService(NotificationService)!).CreateNotificationChannel(channel);
    }
}
