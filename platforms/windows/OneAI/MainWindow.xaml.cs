using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using OneAI.ViewModels;
using OneAI.Native;
using OneAI.Views;

namespace OneAI;

public sealed partial class MainWindow : Window
{
    public ChatViewModel Vm { get; }

    public MainWindow()
    {
        InitializeComponent();
        Vm = new ChatViewModel();
        Chat.SetVm(Vm);
        _ = InitAsync();
    }

    private async Task InitAsync()
    {
        await Vm.EnsureApp();
        await Vm.RefreshSessions();
        if (Vm.Sessions.Count > 0) await Vm.LoadSession(Vm.Sessions[0].Id);
        else await Vm.NewConversation();
        if (Vm.Sessions.Count > 0) Nav.SelectedItem = Vm.Sessions[0];
    }

    private void NewChat_Click(object sender, RoutedEventArgs e)
    {
        _ = Vm.NewConversation();
        Nav.SelectedItem = null;
    }

    private async void Nav_SelectionChanged(NavigationView sender, NavigationViewSelectionChangedEventArgs args)
    {
        if (args.SelectedItem is SessionInfo s && s.Id != Vm.CurrentSessionId)
            await Vm.LoadSession(s.Id);
    }

    private async void DeleteSession_Click(object sender, RoutedEventArgs e)
    {
        if (sender is Button b && b.Tag is string id)
        {
            var dlg = new ContentDialog
            {
                XamlRoot = this.Content.XamlRoot,
                Title = "删除会话",
                Content = "确定删除这个会话?历史无法恢复。",
                PrimaryButtonText = "删除",
                SecondaryButtonText = "取消",
                DefaultButton = ContentDialogButton.Secondary,
            };
            if (await dlg.ShowAsync() == ContentDialogResult.Primary)
                await Vm.DeleteSession(id);
        }
    }

    private async void Settings_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new SettingsDialog { XamlRoot = this.Content.XamlRoot };
        dlg.SetVm(Vm);
        await dlg.ShowAsync();
    }
}
