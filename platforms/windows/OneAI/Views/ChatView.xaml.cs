using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using OneAI.Services;
using OneAI.ViewModels;
using Windows.ApplicationModel.DataTransfer;
using Windows.ApplicationModel;

namespace OneAI.Views;

public sealed partial class ChatView : UserControl
{
    public ChatViewModel Vm { get; private set; } = null!;
    /// <summary>Shared artifact canvas state; long code blocks are promoted here.</summary>
    public ArtifactStore Artifacts { get; } = new();
    private ArtifactCanvas? _canvas;

    public ChatView()
    {
        InitializeComponent();
        // Code blocks inside the assistant DataTemplate open artifacts on this
        // shared store (per-instance wiring from the template isn't practical).
        MarkdownTextBlock.ArtifactStore = Artifacts;
    }

    public void SetVm(ChatViewModel vm)
    {
        Vm = vm;
        Vm.PropertyChanged += (s, e) =>
        {
            if (e.PropertyName == nameof(Vm.StreamTick) || e.PropertyName == nameof(Vm.Items))
                _ = DispatcherQueue.TryEnqueue(() =>
                {
                    if (Vm.Items.Count > 0) Messages.ScrollIntoView(Vm.Items[^1]);
                });
        };
        // Show/hide the docked canvas when an artifact is opened/closed.
        Artifacts.PropertyChanged += (s, e) =>
        {
            if (e.PropertyName == nameof(Artifacts.Visible))
                _ = DispatcherQueue.TryEnqueue(UpdateCanvas);
        };
        Bindings.Update(); // re-resolve x:Bind now that Vm is set
    }

    private void UpdateCanvas()
    {
        if (Artifacts.Visible)
        {
            CanvasHost.Visibility = Visibility.Visible;
            if (_canvas is null || _canvas.Store != Artifacts)
            {
                _canvas = new ArtifactCanvas(Artifacts);
                CanvasHost.Child = _canvas;
            }
        }
        else CanvasHost.Visibility = Visibility.Collapsed;
    }

    private void Send_Click(object sender, RoutedEventArgs e)
    {
        var task = Vm.Input?.Trim() ?? "";
        if (!string.IsNullOrEmpty(task) && !Vm.Running)
        {
            Vm.Input = "";
            _ = Vm.RunTask(task);
        }
    }

    private void Stop_Click(object sender, RoutedEventArgs e) => _ = Vm.Stop();
    private void Mic_Click(object sender, RoutedEventArgs e) { /* placeholder — voice dictation deferred */ }

    private void InputBox_KeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == Windows.System.VirtualKey.Enter)
        {
            var ctrl = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(Windows.System.VirtualKey.Shift);
            bool shift = ctrl.HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
            if (!shift) { e.Handled = true; Send_Click(sender!, e); }
        }
    }

    private async void Retry_Click(object sender, RoutedEventArgs e) => await Vm.RetryLast();
    private async void Debrief_Click(object sender, RoutedEventArgs e) => await Vm.EndScenarioDebrief();

    private async void Settings_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new SettingsDialog { XamlRoot = this.XamlRoot };
        dlg.SetVm(Vm);
        await dlg.ShowAsync();
    }

    // ── Topic intake ───────────────────────────────────────────────────
    // Collect each field's TextBox value (keyed by field id, stashed in Tag) by
    // walking the intake page's visual tree, then hand them to the VM.
    private void CancelScenario_Click(object sender, RoutedEventArgs e) => Vm.CancelPendingScenario();

    private void StartScenario_Click(object sender, RoutedEventArgs e)
    {
        var values = new Dictionary<string, string>();
        foreach (var box in FindDescendants<TextBox>(this).Where(b => b.Tag is string))
            values[(string)box.Tag] = box.Text;
        _ = Vm.ConfirmStartScenario(values);
    }

    // FieldBox_Loaded is a no-op anchor (keeps the template's Loaded wire-up
    // valid); values are read from the live TextBoxes at submit time.
    private void FieldBox_Loaded(object sender, RoutedEventArgs e) { }

    private static IEnumerable<T> FindDescendants<T>(DependencyObject root) where T : DependencyObject
    {
        int count = VisualTreeHelper.GetChildrenCount(root);
        for (int i = 0; i < count; i++)
        {
            var child = VisualTreeHelper.GetChild(root, i);
            if (child is T t) yield return t;
            foreach (var d in FindDescendants<T>(child)) yield return d;
        }
    }

    private void CopyAnswer_Click(object sender, RoutedEventArgs e)
    {
        if (sender is MenuFlyoutItem m && m.Tag is string text)
        {
            var dp = new DataPackage { RequestedOperation = DataPackageOperation.Copy };
            dp.SetText(text);
            Clipboard.SetContent(dp);
        }
    }

    private void ShareAnswer_Click(object sender, RoutedEventArgs e)
    {
        if (sender is MenuFlyoutItem m && m.Tag is string text)
        {
            var dm = DataTransferManager.GetForCurrentView();
            dm.DataRequested += (s, args) =>
            {
                var dp = new DataPackage { RequestedOperation = DataPackageOperation.Copy };
                dp.SetText(text);
                args.Request.Data = dp;
                args.Request.Properties.Title = "OneAI 回答";
            };
            DataTransferManager.ShowShareUI();
        }
    }
}
