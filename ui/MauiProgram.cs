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

		// The FFI seam. NativeCore drives the real Rust core; swap to MockCore for design-time
		// preview / hot-reload without a native build (the UI is identical either way).
		// MockCore 只在 Debug 建置存在 —— 預覽本來就只在 Debug 做,Release 不編死碼進去。
		builder.Services.AddSingleton<ICore, NativeCore>();
		builder.Services.AddSingleton<AppState>();
		builder.Services.AddSingleton<AppShell>();

#if DEBUG
		builder.Logging.AddDebug();
#endif

		return builder.Build();
	}
}
