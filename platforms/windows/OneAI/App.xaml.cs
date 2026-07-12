using Microsoft.UI.Xaml;

namespace OneAI;

public partial class App : Application
{
    /// <summary>Live main window — some helpers (FileSavePicker hwnd) need a
    /// window reference from non-window contexts.</summary>
    public static MainWindow? MainWindowRef { get; private set; }

    public App() => this.InitializeComponent();

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        MainWindowRef = new MainWindow();
        MainWindowRef.Activate();
    }
}
