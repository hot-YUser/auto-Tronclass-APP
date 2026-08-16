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
    ICore? _core;
    ScheduleCoordinator? _schedule;
    string _status = "正在初始化排程…";
    int _handshakeGeneration;
    int _stopping;
    int _handshakeReady;

    public override IBinder? OnBind(Intent? intent) => null;

    public override StartCommandResult OnStartCommand(Intent? intent, StartCommandFlags flags, int startId)
    {
        CreateChannel();
        var notification = BuildNotification(_status);
        if (OperatingSystem.IsAndroidVersionAtLeast(29))
            StartForeground(
                NotificationId,
                notification,
                global::Android.Content.PM.ForegroundService.TypeDataSync);
        else
            StartForeground(NotificationId, notification);

        _core = IPlatformApplication.Current?.Services.GetService<ICore>();
        _schedule = IPlatformApplication.Current?.Services.GetService<ScheduleCoordinator>();
        if (_core is null || _schedule is null)
        {
            FailAndStop(startId, "無法取得核心服務。");
            return StartCommandResult.NotSticky;
        }
        _core.EventReceived -= OnCoreEvent;
        _core.EventReceived += OnCoreEvent;
        Volatile.Write(ref _handshakeReady, 0);
        Volatile.Write(ref _stopping, 0);
        var scheduled = intent is null || intent.Action == AndroidScheduleAlarms.ServiceAction;
        var generation = Interlocked.Increment(ref _handshakeGeneration);
        _ = HandshakeAsync(startId, generation, scheduled);
        return scheduled ? StartCommandResult.Sticky : StartCommandResult.NotSticky;
    }

    async Task HandshakeAsync(int startId, int generation, bool scheduled)
    {
        try
        {
            await _schedule!.BootAsync(DataPaths.Resolve());
            await _schedule.OnResumeAsync();
            var snapshot = await GetSnapshotAsync();
            if (snapshot.PlatformBlock is { } block)
            {
                var clear = await _core!.SendAsync(
                    "ClearPlatformLimit",
                    ("reason", block.Reason));
                if (!ReplyOk(clear)) throw new InvalidOperationException(ReplyError(clear));
            }
            var mode = AndroidScheduleAlarms.CurrentWakeMode(this);
            var modeReply = await _core!.SendAsync(
                "ClearPlatformLimit",
                ("reason", $"wake_mode:{mode}"));
            if (!ReplyOk(modeReply)) throw new InvalidOperationException(ReplyError(modeReply));
            snapshot = await GetSnapshotAsync();
            if (generation != Volatile.Read(ref _handshakeGeneration)) return;

            if (!scheduled && !NeedsForegroundService(snapshot))
            {
                // StartTarget 緊接在使用者授權的 FGS start 後送出；給該命令一個有界交會窗，
                // 避免 handshake 比命令快而先自停，卻不把空服務長期留著消耗配額。
                await Task.Delay(TimeSpan.FromSeconds(3));
                snapshot = await GetSnapshotAsync();
            }
            Volatile.Write(ref _handshakeReady, 1);
            if (!NeedsForegroundService(snapshot))
            {
                StopForegroundAndSelf(startId);
                return;
            }
            _status = scheduled ? "排程監控中" : "監控中";
            UpdateNotification();
        }
        catch (Exception error)
        {
            FailAndStop(startId, $"排程啟動失敗：{error.Message}");
        }
    }

    async Task<MonitoringSnapshotContract> GetSnapshotAsync()
    {
        var reply = await _core!.SendAsync("GetMonitoringSnapshot");
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
        _core ??= IPlatformApplication.Current?.Services.GetService<ICore>();
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
    }

    public override void OnDestroy()
    {
        if (_core is not null) _core.EventReceived -= OnCoreEvent;
        Interlocked.Increment(ref _handshakeGeneration);
        StopForeground(StopForegroundFlags.Remove);
        base.OnDestroy();
    }

    void OnCoreEvent(JsonElement coreEvent)
    {
        if (!coreEvent.TryGetProperty("event", out var eventName)) return;
        switch (eventName.GetString())
        {
            case "MonitoringSnapshot":
                try
                {
                    var snapshot = MonitoringSnapshotContract.Parse(
                        WireShape.Required(coreEvent, "snapshot"));
                    if (Volatile.Read(ref _handshakeReady) != 0 &&
                        !NeedsForegroundService(snapshot))
                    {
                        StopForegroundAndSelf();
                        return;
                    }
                    _status = snapshot.SessionState switch
                    {
                        "starting" => "正在準備監控…",
                        "stopping" => "正在完成已授權工作…",
                        "platform_blocked" => "平台限制已暫停新偵測",
                        _ => $"監控中 · {snapshot.Targets.Count(target => target.RuntimeState == "monitoring")} 個目標",
                    };
                    UpdateNotification();
                }
                catch (FormatException error)
                {
                    FailAndStop(null, $"核心快照無效：{error.Message}");
                }
                break;
            case "RollcallDetected":
                SetStatus("偵測到點名，處理中…");
                break;
            case "SignedIn":
                SetStatus("已簽到，繼續監控中");
                break;
            case "QuizPrepared":
                SetStatus("偵測到測驗，備答中…");
                break;
            case "QuizSubmitted":
                SetStatus("已送出測驗，繼續監控中");
                break;
        }
    }

    void SetStatus(string status)
    {
        if (_status == status) return;
        _status = status;
        UpdateNotification();
    }

    void UpdateNotification()
    {
        var manager = (NotificationManager?)GetSystemService(NotificationService);
        manager?.Notify(NotificationId, BuildNotification(_status));
    }

    void FailAndStop(int? startId, string message)
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
    }

    void StopForegroundAndSelf(int? startId = null)
    {
        if (Interlocked.Exchange(ref _stopping, 1) != 0) return;
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
