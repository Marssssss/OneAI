using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using OneAI.ViewModels;
using Windows.ApplicationModel.DataTransfer;
using Windows.ApplicationModel;
using System.Text;

namespace OneAI.Views;

public sealed partial class ChatView : UserControl
{
    public ChatViewModel Vm { get; private set; } = null!;

    public ChatView()
    {
        InitializeComponent();
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
        Bindings.Update(); // re-resolve x:Bind now that Vm is set
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
