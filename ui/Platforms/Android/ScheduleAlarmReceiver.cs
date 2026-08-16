using Android.App;
using Android.Content;
using Android.OS;
using Android.Provider;
using AndroidX.Core.App;
using AndroidX.Core.Content;
using System.Runtime.Versioning;

namespace Ui;

/// <summary>保存並排定唯一最早 UTC boundary；精準權限決定能否自動啟動 FGS。</summary>
public static class AndroidScheduleAlarms
{
    public const string AlarmAction = "com.autotronclass.app.SCHEDULE_BOUNDARY";
    public const string ServiceAction = "com.autotronclass.app.SCHEDULED_MONITOR";
    public const string ManualServiceAction = "com.autotronclass.app.MANUAL_MONITOR";
    public const string ExpectedBoundaryExtra = "expected_boundary_ms";
    const string PreferencesName = "schedule_alarm";
    const string BoundaryKey = "boundary_ms";
    const string ActiveKey = "active_now";
    const int AlarmRequestCode = 4101;
    const int NoticeId = 4102;
    const string NoticeChannel = "tronclass_schedule_action";

    public static string Schedule(Context context, DateTimeOffset? boundaryUtc, bool activeNow)
    {
        var manager = (AlarmManager?)context.GetSystemService(Context.AlarmService);
        if (manager is null) return "unavailable";
        var pending = AlarmPendingIntent(context);
        manager.Cancel(pending);
        pending.Cancel();
        var preferences = context.GetSharedPreferences(PreferencesName, FileCreationMode.Private)!;
        var editor = preferences.Edit()!;
        if (boundaryUtc is null)
        {
            editor.Remove(BoundaryKey);
            editor.PutBoolean(ActiveKey, activeNow);
            editor.Apply();
            return CurrentWakeMode(context);
        }

        var trigger = boundaryUtc.Value.ToUnixTimeMilliseconds();
        editor.PutLong(BoundaryKey, trigger);
        editor.PutBoolean(ActiveKey, activeNow);
        editor.Apply();
        pending = AlarmPendingIntent(context);
        try
        {
            if (CanScheduleExact(context))
            {
                manager.SetExactAndAllowWhileIdle(AlarmType.RtcWakeup, trigger, pending);
                return "exact";
            }
            manager.SetAndAllowWhileIdle(AlarmType.RtcWakeup, trigger, pending);
            return "inexact_user_action_required";
        }
        catch (Java.Lang.SecurityException)
        {
            manager.SetAndAllowWhileIdle(AlarmType.RtcWakeup, trigger, pending);
            return "inexact_user_action_required";
        }
        catch (Exception error)
        {
            global::Android.Util.Log.Warn(nameof(AndroidScheduleAlarms), $"無法排程 boundary：{error.Message}");
            return "unavailable";
        }
    }

    public static string RescheduleStored(Context context, bool fromBoot)
    {
        var preferences = context.GetSharedPreferences(PreferencesName, FileCreationMode.Private)!;
        var boundaryMs = preferences.GetLong(BoundaryKey, 0);
        if (boundaryMs <= 0) return CurrentWakeMode(context);
        var active = preferences.GetBoolean(ActiveKey, false);
        var boundary = DateTimeOffset.FromUnixTimeMilliseconds(boundaryMs);
        var hasFutureBoundary = boundary > DateTimeOffset.UtcNow;
        if (fromBoot)
        {
            var action = AndroidSchedulePolicy.AfterBoot(hasFutureBoundary, active);
            if (action.NotifyUser)
                NotifyOpenApp(
                    context,
                    active && hasFutureBoundary
                        ? "重新開機時已在監控時段內"
                        : "排程時間已過",
                    active && hasFutureBoundary
                        ? "Android 不允許由開機廣播直接啟動 dataSync；請點擊開啟 App。未來邊界已重新排定。"
                        : "請開啟 App 重新計算監控時間表。");
            if (!action.ScheduleFutureBoundary)
                return Schedule(context, null, activeNow: false);
        }
        else if (!hasFutureBoundary)
        {
            NotifyOpenApp(context, "排程時間已過", "請開啟 App 重新計算監控時間表。");
            return Schedule(context, null, activeNow: false);
        }
        return Schedule(context, boundary, active);
    }

    public static string CurrentWakeMode(Context context) =>
        AndroidSchedulePolicy.WakeMode(
            OperatingSystem.IsAndroidVersionAtLeast(31),
            CanScheduleExact(context));

    public static bool CanScheduleExact(Context context)
    {
        if (!OperatingSystem.IsAndroidVersionAtLeast(31)) return true;
        var manager = (AlarmManager?)context.GetSystemService(Context.AlarmService);
        return manager?.CanScheduleExactAlarms() == true;
    }

    public static void OpenExactAlarmSettings(Context context)
    {
        if (!OperatingSystem.IsAndroidVersionAtLeast(31)) return;
        var intent = new Intent(Settings.ActionRequestScheduleExactAlarm);
        intent.SetData(global::Android.Net.Uri.Parse($"package:{context.PackageName}"));
        intent.AddFlags(ActivityFlags.NewTask);
        context.StartActivity(intent);
    }

    internal static long StoredBoundary(Context context) =>
        context.GetSharedPreferences(PreferencesName, FileCreationMode.Private)!.GetLong(BoundaryKey, 0);

    internal static void NotifyOpenApp(Context context, string title, string message)
    {
        CreateNoticeChannel(context);
        var open = new Intent(context, typeof(MainActivity));
        open.AddFlags(ActivityFlags.SingleTop | ActivityFlags.ClearTop);
        var content = PendingIntent.GetActivity(
            context,
            0,
            open,
            PendingIntentFlags.Immutable | PendingIntentFlags.UpdateCurrent);
        var notification = new NotificationCompat.Builder(context, NoticeChannel);
        notification.SetSmallIcon(global::Android.Resource.Drawable.IcDialogInfo);
        notification.SetContentTitle(title);
        notification.SetContentText(message);
        notification.SetStyle(new NotificationCompat.BigTextStyle().BigText(message));
        notification.SetContentIntent(content);
        notification.SetAutoCancel(true);
        notification.SetPriority(NotificationCompat.PriorityHigh);
        NotificationManagerCompat.From(context)?.Notify(NoticeId, notification.Build());
    }

    static PendingIntent AlarmPendingIntent(Context context)
    {
        var intent = new Intent(context, typeof(ScheduleAlarmReceiver));
        intent.SetAction(AlarmAction);
        var boundary = StoredBoundary(context);
        intent.PutExtra(ExpectedBoundaryExtra, boundary);
        return PendingIntent.GetBroadcast(
            context,
            AlarmRequestCode,
            intent,
            PendingIntentFlags.Immutable | PendingIntentFlags.UpdateCurrent)!;
    }

    static void CreateNoticeChannel(Context context)
    {
        if (!OperatingSystem.IsAndroidVersionAtLeast(26)) return;
        var manager = (NotificationManager?)context.GetSystemService(Context.NotificationService);
        manager?.CreateNotificationChannel(new NotificationChannel(
            NoticeChannel,
            "排程需要操作",
            NotificationImportance.High));
    }
}

[BroadcastReceiver(Enabled = true, Exported = false)]
[IntentFilter([AndroidScheduleAlarms.AlarmAction])]
public sealed class ScheduleAlarmReceiver : BroadcastReceiver
{
    public override void OnReceive(Context? context, Intent? intent)
    {
        if (context is null || intent?.Action != AndroidScheduleAlarms.AlarmAction) return;
        var pending = GoAsync();
        if (pending is null) return;
        _ = HandleAsync(context.ApplicationContext ?? context, intent, pending);
    }

    static async Task HandleAsync(Context context, Intent intent, BroadcastReceiver.PendingResult pending)
    {
        try
        {
            var expected = intent.GetLongExtra(AndroidScheduleAlarms.ExpectedBoundaryExtra, 0);
            if (expected <= 0 || expected != AndroidScheduleAlarms.StoredBoundary(context)) return;
            if (AndroidSchedulePolicy.AtBoundary(
                    AndroidScheduleAlarms.CanScheduleExact(context)) ==
                AlarmBoundaryAction.NotifyUser)
            {
                AndroidScheduleAlarms.NotifyOpenApp(
                    context,
                    "監控排程已到",
                    "未允許精準鬧鐘；請點擊開啟 App 後開始監控。");
                return;
            }
            var service = new Intent(context, typeof(CoreForegroundService));
            service.SetAction(AndroidScheduleAlarms.ServiceAction);
            service.PutExtra(AndroidScheduleAlarms.ExpectedBoundaryExtra, expected);
            try
            {
                if (OperatingSystem.IsAndroidVersionAtLeast(26))
                    ContextCompat.StartForegroundService(context, service);
                else
                    context.StartService(service);
            }
            catch (ForegroundServiceStartNotAllowedException error) when (OperatingSystem.IsAndroidVersionAtLeast(31))
            {
                AndroidScheduleAlarms.NotifyOpenApp(
                    context,
                    "Android 阻擋背景啟動",
                    error.Message ?? "Foreground service start not allowed");
            }
            catch (Java.Lang.SecurityException error)
            {
                AndroidScheduleAlarms.NotifyOpenApp(
                    context,
                    "缺少背景啟動權限",
                    error.Message ?? "SecurityException");
            }
        }
        finally
        {
            pending.Finish();
        }
        await Task.CompletedTask;
    }
}

[BroadcastReceiver(Enabled = true, Exported = true)]
[IntentFilter(
    [
        Intent.ActionBootCompleted,
        Intent.ActionMyPackageReplaced,
        Intent.ActionTimeChanged,
        Intent.ActionTimezoneChanged,
        "android.app.action.SCHEDULE_EXACT_ALARM_PERMISSION_STATE_CHANGED",
    ])]
public sealed class ScheduleBootReceiver : BroadcastReceiver
{
    public override void OnReceive(Context? context, Intent? intent)
    {
        if (context is null) return;
        var fromBoot = intent?.Action == Intent.ActionBootCompleted;
        AndroidScheduleAlarms.RescheduleStored(context.ApplicationContext ?? context, fromBoot);
    }
}
