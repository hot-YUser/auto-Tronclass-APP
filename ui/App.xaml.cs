namespace Ui;

public partial class App : Application
{
	private readonly MainPage _main;

	public App(MainPage main)
	{
		InitializeComponent();
		_main = main;
	}

	// Handoff scaffold: a single placeholder page on MockCore. The real four-tab shell is Fable's job.
	protected override Window CreateWindow(IActivationState? activationState) => new(_main);
}
