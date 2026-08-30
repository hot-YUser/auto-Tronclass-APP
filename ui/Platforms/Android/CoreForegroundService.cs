using Android.App;
using Android.Content;
using Android.OS;
using Android.Runtime;
using AndroidX.Core.App;
using Microsoft.Extensions.DependencyInjection;
using System.Runtime.Versioning;
using System.Text.Json;
using TronClass.Interop;

namespace Ui;

/// <summary>排程或使用者動作啟動後，先完成 cold-process clock handshake 才保留 dataSync FGS。</summary>
[Service(Exported = false, ForegroundServiceType = global::Android.Content.PM.ForegroundService.TypeDataSync)]
public sealed class CoreForegroundService : Service
{
    const string ChannelId = "tronclass_monitor";
    const int NotificationId = 1;
    const int DiagnosticNotificationId = 2;
    readonly ForegroundServiceHandshakeState _handshake = new();
    ICore? _core;
    Action<JsonElement>? _coreEventHandler;
    string _status = "正在初始化排程…";
    int _currentStartId;

    public override IBinder? OnBind(Intent? intent) => null;

    public override StartCommandResult OnStartCommand(Intent? intent, StartCommandFlags flags, int startId)
    {
        // Claim the new generation before publishing anything. This cancels every old continuation and
        // makes already-queued old event callbacks fail their lease check.
        var generation = _handshake.Begin();
        _currentStartId = startId;
        if (!_handshake.TryRun(generation, () =>
        {
            _status = "正在初始化排程…";
            CreateChannel();
            var notification = BuildNotification(_status);
            if (OperatingSystem.IsAndroidVersionAtLeast(29))
                StartForeground(
                    NotificationId,
                    notification,
                    global::Android.Content.PM.ForegroundService.TypeDataSync);
            else
                StartForeground(NotificationId, notification);
        })) return StartCommandResult.NotSticky;

        if (_core is not null && _coreEventHandler is not null)
            _core.EventReceived -= _coreEventHandler;
        _coreEventHandler = null;

        var core = IPlatformApplication.Current?.Services.GetService<ICore>();
        var schedule = IPlatformApplication.Current?.Services.GetService<ScheduleCoordinator>();
        _core = core;
        if (core is null || schedule is null)
        {
            FailAndStop(generation, startId, "無法取得核心服務。");
            return StartCommandResult.NotSticky;
        }

        // NativeCore drains events on the ThreadPool. Capture this generation so an invocation list
        // copied before unsubscribe still cannot mutate a newer service start.
        _coreEventHandler = coreEvent => OnCoreEvent(generation, coreEvent);
        core.EventReceived += _coreEventHandler;

        var scheduled = intent is null || intent.Action == AndroidScheduleAlarms.ServiceAction;
        _ = HandshakeAsync(startId, generation, scheduled, core, schedule);
        return scheduled ? StartCommandResult.Sticky : StartCommandResult.NotSticky;
    }

    async Task HandshakeAsync(
        int startId,
        ForegroundServiceHandshakeState.Lease generation,
        bool scheduled,
        ICore core,
        ScheduleCoordinator schedule)
    {
        try
        {
            await _handshake.RunAsync(generation, token => schedule.BootAsync(DataPaths.Resolve()).WaitAsync(token));
            tokenThrow(generation);
            await _handshake.RunAsync(generation, token => schedule.OnResumeAsync().WaitAsync(token));
            tokenThrow(generation);
            var snapshot = await GetSnapshotAsync(generation, core);
            tokenThrow(generation);
            if (snapshot.PlatformBlock is { } block)
            {
                var clear = await _handshake.RunAsync(
                    generation,
                    token => core.SendAsync("ClearPlatformLimit", ("reason", block.Reason)).WaitAsync(token));
                if (!ReplyOk(clear)) throw new InvalidOperationException(ReplyError(clear));
                tokenThrow(generation);
            }

            var mode = AndroidScheduleAlarms.CurrentWakeMode(this);
            var modeReply = await _handshake.RunAsync(
                generation,
                token => core.SendAsync("ClearPlatformLimit", ("reason", $"wake_mode:{mode}")).WaitAsync(token));
            if (!ReplyOk(modeReply)) throw new InvalidOperationException(ReplyError(modeReply));
            tokenThrow(generation);

            snapshot = await GetSnapshotAsync(generation, core);
            tokenThrow(generation);
            if (!scheduled && !NeedsForegroundService(snapshot))
            {
                await _handshake.DelayAsync(generation, TimeSpan.FromSeconds(3));
                tokenThrow(generation);
                snapshot = await GetSnapshotAsync(generation, core);
                tokenThrow(generation);
            }

            if (!NeedsForegroundService(snapshot))
            {
                _handshake.TryStop(generation, () => StopForegroundAndSelf(startId));
                return;
            }

            _handshake.TrySucceed(generation, () =>
            {
                _status = scheduled ? "排程監控中" : "監控中";
                UpdateNotification();
            });
        }
        catch (System.OperationCanceledException) when (
            generation.Cancellation.IsCancellationRequested || !_handshake.IsCurrent(generation))
        {
            // A newer start, timeout, stop, or destroy owns the service now. Cancellation is not failure.
        }
        catch (Exception error)
        {
            FailAndStop(generation, startId, $"排程啟動失敗：{error.Message}");
        }
    }

    static void tokenThrow(ForegroundServiceHandshakeState.Lease generation) =>
        generation.Cancellation.ThrowIfCancellationRequested();

    async Task<MonitoringSnapshotContract> GetSnapshotAsync(
        ForegroundServiceHandshakeState.Lease generation,
        ICore core)
    {
        var reply = await _handshake.RunAsync(
            generation,
            token => core.SendAsync("GetMonitoringSnapshot").WaitAsync(token));
        if (!ReplyOk(reply)) throw new InvalidOperationException(ReplyError(reply));
        return MonitoringSnapshotContract.Parse(
            WireShape.Required(WireShape.Required(reply, "data"), "snapshot"));
    }

    static bool NeedsForegroundService(MonitoringSnapshotContract snapshot) =>
        snapshot.SessionState is "starting" or "running" or "stopping" ||
        snapshot.Targets.Any(target =>
            target.RuntimeState is "starting" or "monitoring" or "stopping" ||
            target.AccountResults.Any(result => result.Phase is "pending" or "authorized"));

    [SupportedOSPlatform("android35.0")]
    public override void OnTimeout(
        int startId,
        global::Android.Content.PM.ForegroundService fgsType)
    {
        // Stale timeout for an older startId must not kill a newer generation.
        if (startId != _currentStartId) return;
        _core ??= IPlatformApplication.Current?.Services.GetService<ICore>();
        _handshake.TryStopCurrent(() =>
        {
            if (_core is not null)
            {
                var suspend = _core.SendAsync(
                    "SuspendForPlatformLimit",
                    ("reason", "android_data_sync_timeout"));
                _ = suspend.ContinueWith(
                    static task => _ = task.Exception,
                    CancellationToken.None,
                    TaskContinuationOptions.OnlyOnFaulted,
                    TaskScheduler.Default);
            }
            try
            {
                ShowDiagnostic(
                    "背景監控已由 Android 暫停",
                    "已達 dataSync 背景時數；只停止新偵測，不會假裝撤回已送出的請求。請開啟 App 恢復。");
            }
            finally
            {
                StopForegroundAndSelf(startId);
            }
        });
    }

    public override void OnDestroy()
    {
        _handshake.Destroy();
        if (_core is not null && _coreEventHandler is not null)
            _core.EventReceived -= _coreEventHandler;
        _coreEventHandler = null;
        _core = null;
        StopForeground(StopForegroundFlags.Remove);
        base.OnDestroy();
    }

    void OnCoreEvent(
        ForegroundServiceHandshakeState.Lease generation,
        JsonElement coreEvent)
    {
        if (!_handshake.IsCurrent(generation) ||
            !coreEvent.TryGetProperty("event", out var eventName)) return;

        switch (eventName.GetString())
        {
            case "MonitoringSnapshot":
                try
                {
                    var snapshot = MonitoringSnapshotContract.Parse(
                        WireShape.Required(coreEvent, "snapshot"));
                    if (_handshake.IsReady(generation) &&
                        !NeedsForegroundService(snapshot))
                    {
                        _handshake.TryStop(generation, () => StopForegroundAndSelf());
                        return;
                    }
                    _handshake.TryRun(generation, () =>
                    {
                        _status = snapshot.SessionState switch
                        {
                            "starting" => "正在準備監控…",
                            "stopping" => "正在完成已授權工作…",
                            "platform_blocked" => "平台限制已暫停新偵測",
                            _ => $"監控中 · {snapshot.Targets.Count(target => target.RuntimeState == "monitoring")} 個目標",
                        };
                        UpdateNotification();
                    });
                }
                catch (FormatException error)
                {
                    FailAndStop(generation, null, $"核心快照無效：{error.Message}");
                }
                break;
            case "RollcallDetected":
                SetStatus(generation, "偵測到點名，處理中…");
                break;
            case "SignedIn":
                SetStatus(generation, "已簽到，繼續監控中");
                break;
            case "QuizPrepared":
                SetStatus(generation, "偵測到測驗，備答中…");
                break;
            case "QuizSubmitted":
                SetStatus(generation, "已送出測驗，繼續監控中");
                break;
        }
    }

    void SetStatus(ForegroundServiceHandshakeState.Lease generation, string status) =>
        _handshake.TryRun(generation, () =>
        {
            if (_status == status) return;
            _status = status;
            UpdateNotification();
        });

    void UpdateNotification()
    {
        var manager = (NotificationManager?)GetSystemService(NotificationService);
        manager?.Notify(NotificationId, BuildNotification(_status));
    }

    void FailAndStop(
        ForegroundServiceHandshakeState.Lease generation,
        int? startId,
        string message) =>
        _handshake.TryStop(generation, () =>
        {
            global::Android.Util.Log.Warn(nameof(CoreForegroundService), message);
            try
            {
                ShowDiagnostic("無法啟動排程監控", message);
            }
            finally
            {
                StopForegroundAndSelf(startId);
            }
        });

    void StopForegroundAndSelf(int? startId = null)
    {
        StopForeground(StopForegroundFlags.Remove);
        if (startId is { } id) StopSelf(id);
        else StopSelf();
    }

    void ShowDiagnostic(string title, string message)
    {
        var openApp = new Intent(this, typeof(MainActivity));
        openApp.AddFlags(ActivityFlags.SingleTop | ActivityFlags.ClearTop);
        var pending = PendingIntent.GetActivity(
            this,
            0,
            openApp,
            PendingIntentFlags.Immutable | PendingIntentFlags.UpdateCurrent);
        var notification = new NotificationCompat.Builder(this, ChannelId);
        notification.SetSmallIcon(global::Android.Resource.Drawable.IcDialogInfo);
        notification.SetContentTitle(title);
        notification.SetContentText(message);
        notification.SetStyle(new NotificationCompat.BigTextStyle().BigText(message));
        notification.SetContentIntent(pending);
        notification.SetAutoCancel(true);
        notification.SetPriority(NotificationCompat.PriorityHigh);
        NotificationManagerCompat.From(this)?.Notify(
            DiagnosticNotificationId,
            notification.Build());
    }

    Notification BuildNotification(string text)
    {
        var openApp = new Intent(this, typeof(MainActivity));
        openApp.AddFlags(ActivityFlags.SingleTop | ActivityFlags.ClearTop);
        var pending = PendingIntent.GetActivity(
            this,
            0,
            openApp,
            PendingIntentFlags.Immutable | PendingIntentFlags.UpdateCurrent);
        var builder = new NotificationCompat.Builder(this, ChannelId);
        builder.SetContentTitle("自動 Tronclass");
        builder.SetContentText(text);
        builder.SetSmallIcon(global::Android.Resource.Drawable.IcDialogInfo);
        builder.SetContentIntent(pending);
        builder.SetOngoing(true);
        return builder.Build()!;
    }

    void CreateChannel()
    {
        if (!OperatingSystem.IsAndroidVersionAtLeast(26)) return;
        var channel = new NotificationChannel(
            ChannelId,
            "監控",
            NotificationImportance.Low);
        ((NotificationManager)GetSystemService(NotificationService)!)
            .CreateNotificationChannel(channel);
    }

    static bool ReplyOk(JsonElement reply) =>
        reply.TryGetProperty("ok", out var ok) && ok.ValueKind == JsonValueKind.True;

    static string ReplyError(JsonElement reply) =>
        reply.TryGetProperty("error", out var error) && error.ValueKind == JsonValueKind.String
            ? error.GetString() ?? "核心拒絕命令。"
            : "核心拒絕命令。";
}
