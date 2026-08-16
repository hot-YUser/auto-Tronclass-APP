using Android.App;
using Android.Content;
using Android.Content.PM;
using Android.OS;
using Microsoft.Extensions.DependencyInjection;

namespace Ui;

[Activity(Theme = "@style/Maui.SplashTheme", MainLauncher = true, LaunchMode = LaunchMode.SingleTop, ConfigurationChanges = ConfigChanges.ScreenSize | ConfigChanges.Orientation | ConfigChanges.UiMode | ConfigChanges.ScreenLayout | ConfigChanges.SmallestScreenSize | ConfigChanges.Density)]
public class MainActivity : MauiAppCompatActivity
{
    protected override void OnCreate(Bundle? savedInstanceState)
    {
        base.OnCreate(savedInstanceState);

        // Android 13+ (API 33): POST_NOTIFICATIONS is a RUNTIME permission — declaring it in the
        // manifest is not enough; without requesting it the ongoing foreground-service notification is
        // silently suppressed (why the persistent "monitoring" notification never showed). Ask once.
        if (OperatingSystem.IsAndroidVersionAtLeast(33) &&
            CheckSelfPermission(Android.Manifest.Permission.PostNotifications) != Permission.Granted)
        {
            RequestPermissions(new[] { Android.Manifest.Permission.PostNotifications }, 1001);
        }
    }

    protected override void OnResume()
    {
        base.OnResume();
        AndroidScheduleAlarms.RescheduleStored(this, fromBoot: false);
        if (IPlatformApplication.Current?.Services.GetService<AppState>() is { } state)
            _ = ResumeCoreAsync(state);
    }

    static async Task ResumeCoreAsync(AppState state)
    {
        await state.BootAsync();
        await state.ResumeScheduleAsync();
    }
}
