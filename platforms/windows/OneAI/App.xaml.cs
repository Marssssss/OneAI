using Microsoft.UI.Xaml;

namespace OneAI;

public partial class App : Application
{
    /// <summary>Live main window — some helpers (FileSavePicker hwnd) need a
    /// window reference from non-window contexts.</summary>
    public static MainWindow? MainWindowRef { get; private set; }

    public App() => InitializeComponent();

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        // Capture the UI thread's DispatcherQueue NOW — before the Window's
        // InitializeComponent() loads its XAML. WinUI 3's
        // DispatcherQueue.GetForCurrentThread() returns null later on the SAME
        // thread after a Window's XAML load (a known WinUI 3 quirk), even
        // though the queue object is still valid. So we hold the live
        // reference and pass it down to MainWindow → ChatViewModel, which uses
        // it to marshal streaming callbacks to the UI thread.
        var dq = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread();
        MainWindowRef = new MainWindow(dq);
        MainWindowRef.Activate();
    }
}
