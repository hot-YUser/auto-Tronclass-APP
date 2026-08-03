namespace Ui;

/// <summary>
/// Android 前景服務的唯一啟停邊界。Windows 是零成本 no-op；Android 只因使用者啟動監控而開始，
/// 並在 StopMonitoring 成功或 core 回到 idle 時停止。
/// </summary>
static class MonitoringServiceLifetime
{
    public static void Start()
    {
#if ANDROID
        var context = global::Android.App.Application.Context;
        var intent = new global::Android.Content.Intent(context, typeof(CoreForegroundService));
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
