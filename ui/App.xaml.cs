using Microsoft.Extensions.DependencyInjection;

namespace Ui;

public partial class App : Application
{
	public App()
	{
		InitializeComponent();
	}

	protected override Window CreateWindow(IActivationState? activationState)
	{
		// Flow: Unlock → Accounts → (Add) / Dashboard, via a plain navigation stack.
		return new Window(new NavigationPage(new UnlockPage()));
	}
}