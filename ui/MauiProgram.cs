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

		// Mock 只能由 Debug + -p:UseMockCore=true 編入；Release 即使傳入該 property 仍固定 NativeCore。
#if USE_MOCK_CORE
		builder.Services.AddSingleton<ICore, MockCore>();
#else
		builder.Services.AddSingleton<ICore, NativeCore>();
#endif
		builder.Services.AddSingleton<ScheduleCoordinator>();
		builder.Services.AddSingleton<AppState>();
		builder.Services.AddSingleton<AppShell>();

#if DEBUG
		builder.Logging.AddDebug();
#endif

		return builder.Build();
	}
}
