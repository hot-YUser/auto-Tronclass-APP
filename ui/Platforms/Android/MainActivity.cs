using Android.App;
using Android.Content;
using Android.Content.PM;
using Android.OS;

namespace Ui;

[Activity(Theme = "@style/Maui.SplashTheme", MainLauncher = true, LaunchMode = LaunchMode.SingleTop, ConfigurationChanges = ConfigChanges.ScreenSize | ConfigChanges.Orientation | ConfigChanges.UiMode | ConfigChanges.ScreenLayout | ConfigChanges.SmallestScreenSize | ConfigChanges.Density)]
public class MainActivity : MauiAppCompatActivity
{
    protected override void OnCreate(Bundle? savedInstanceState)
    {
        base.OnCreate(savedInstanceState);

        // Start the foreground service so the core's monitor loop survives backgrounding.
        // This is the Android half of "the platform keeps the process alive" (docs 50 step 0).
        var intent = new Intent(this, typeof(CoreForegroundService));
        if (OperatingSystem.IsAndroidVersionAtLeast(26))
            StartForegroundService(intent);
        else
            StartService(intent);
    }
}
