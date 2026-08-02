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
    private string _status = "待命中"; // shown in the ongoing notification; updated on state/activity events

    public override IBinder? OnBind(Intent? intent) => null;

    public override StartCommandResult OnStartCommand(Intent? intent, StartCommandFlags flags, int startId)
    {
        CreateChannel();

        // API 34+ enforces this: the service must declare a foregroundServiceType (the [Service]
        // attribute emits android:foregroundServiceType="dataSync"), hold FOREGROUND_SERVICE_DATA_SYNC,
        // AND pass the type into startForeground — else it throws. The 3-arg overload is API 29+.
        var notification = BuildNotification(_status);
        if (OperatingSystem.IsAndroidVersionAtLeast(29))
            StartForeground(NotificationId, notification, global::Android.Content.PM.ForegroundService.TypeDataSync);
        else
            StartForeground(NotificationId, notification);

        // The UI boots the core; here we just resolve the same singleton (via the MAUI DI container)
        // and reflect its state in the notification. BootAsync is idempotent — a safety net if the
        // service outlives the UI.
        _core = IPlatformApplication.Current?.Services.GetService<ICore>();
        if (_core is not null)
        {
            _core.EventReceived -= OnCoreEvent; // dedup: OnStartCommand can run again (e.g. activity restart)
            _core.EventReceived += OnCoreEvent;
            _ = _core.BootAsync(DataPaths.Resolve()); // Android → 沙盒；與 UI 端同一決策點
        }

        return StartCommandResult.Sticky;
    }

    public override void OnDestroy()
    {
        if (_core is not null) _core.EventReceived -= OnCoreEvent;
        base.OnDestroy();
    }

    // Reflect the CORE's real state/activity in the ongoing notification — and only re-notify when the
    // visible text actually changes (never per-Tick, which would rewrite the notification every second).
    private void OnCoreEvent(JsonElement ev)
    {
        if (!ev.TryGetProperty("event", out var evName)) return;
        var name = evName.GetString();
        if (name == "Tick") return; // heartbeat = process-alive proof only, not a notification update

        var text = name switch
        {
            "StateChanged" => ev.TryGetProperty("state", out var s) ? s.GetString() switch
            {
                "monitoring" => "監控中",
                "logging_in" => "登入中…",
                "idle" or "starting" => "待命中",
                _ => null,
            } : null,
            "RollcallDetected" => "偵測到點名，處理中…",
            "SignedIn" => "已簽到 ✓ 繼續監控中",
            "QuizPrepared" => "偵測到測驗，備答中…",
            "QuizSubmitted" => "已送出測驗 ✓ 繼續監控中",
            _ => null,
        };
        if (text is null || text == _status) return; // only update when the visible status actually changes
        _status = text;
        var mgr = (NotificationManager)GetSystemService(NotificationService)!;
        mgr.Notify(NotificationId, BuildNotification(_status));
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
