namespace Ui;

/// <summary>
/// Android 前景服務的使用者互動入口；Windows 為零成本 no-op。排程喚醒由
/// <see cref="AndroidScheduleAlarms"/> 的 explicit receiver 負責。
/// </summary>
static class MonitoringServiceLifetime
{
    public static void Start()
    {
#if ANDROID
        var context = global::Android.App.Application.Context;
        var intent = new global::Android.Content.Intent(context, typeof(CoreForegroundService));
        intent.SetAction(AndroidScheduleAlarms.ManualServiceAction);
        if (OperatingSystem.IsAndroidVersionAtLeast(26))
            global::AndroidX.Core.Content.ContextCompat.StartForegroundService(context, intent);
        else
            context.StartService(intent);
#endif
    }

    public static void Stop()
    {
#if ANDROID
        var context = global::Android.App.Application.Context;
        context.StopService(new global::Android.Content.Intent(context, typeof(CoreForegroundService)));
#endif
    }
}
