using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging;
using TronClass.Interop;

namespace Ui;

public static class MauiProgram
{
	public static MauiApp CreateMauiApp()
	{
		var builder = MauiApp.CreateBuilder();
		builder
			.UseMauiApp<App>()
			.ConfigureFonts(fonts =>
			{
				fonts.AddFont("OpenSans-Regular.ttf", "OpenSansRegular");
				fonts.AddFont("OpenSans-Semibold.ttf", "OpenSansSemibold");
			});

		// The FFI seam. Swap MockCore → NativeCore to drive the real Rust core (the UI is unchanged).
		builder.Services.AddSingleton<ICore, MockCore>();
		builder.Services.AddSingleton<AppState>();
		builder.Services.AddSingleton<AppShell>();

#if DEBUG
		builder.Logging.AddDebug();
#endif

		return builder.Build();
	}
}
