using TronClass.Interop;

namespace Ui;

public partial class App : Application
{
	private readonly AppShell _shell;
	private readonly ICore _core;

	public App(AppShell shell, ICore core)
	{
		InitializeComponent();
		_shell = shell;
		_core = core;
	}

	protected override Window CreateWindow(IActivationState? activationState)
	{
		var window = new Window(_shell) { Title = "自動 Tronclass" };
#if WINDOWS
		// 關窗 = Windows 程序結束:此刻才有序釋放 native runtime(恰一次 core_free,見 NativeCore.Dispose)。
		// Android 不得 core_free——FGS 可能保住 process,core 必須存活(adjudication 明列)。
		window.Destroying += (_, _) => (_core as IDisposable)?.Dispose();
#endif
		return window;
	}
}
