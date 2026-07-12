using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
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
    }

    private void NewChat_Click(object sender, RoutedEventArgs e) => _ = Vm.NewConversation();

    private void Scenario_Click(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is Scenario sc) StartScenario(sc);
    }

    /// <summary>Scenarios with topic-intake fields route through the inline
    /// PendingScenario page (rendered in the chat detail); scenarios without
    /// fields start immediately.</summary>
    private void StartScenario(Scenario sc)
    {
        if (sc.TopicFields is { Count: > 0 }) Vm.PendingScenario = sc;
        else _ = Vm.NewConversation(sc);
    }

    private async void EditScenario_Click(object sender, RoutedEventArgs e)
    {
        if (sender is Button b && b.Tag is string id)
        {
            var sc = Vm.AgentStore.Scenarios.FirstOrDefault(s => s.Id == id);
            if (sc == null) return;
            var dlg = new ScenarioEditor(sc, Vm.AgentStore) { XamlRoot = this.Content.XamlRoot };
            await dlg.ShowAsync();
        }
    }

    private async void NewScenario_Click(object sender, RoutedEventArgs e)
    {
        var sc = new Scenario
        {
            Id = Guid.NewGuid().ToString("N").Substring(0, 8),
            Name = "新场景",
            Icon = "👥",
            TurnPolicy = TurnPolicy.Scripted,
        };
        var dlg = new ScenarioEditor(sc, Vm.AgentStore) { XamlRoot = this.Content.XamlRoot };
        await dlg.ShowAsync();
    }

    private async void Session_Click(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is SessionInfo s && s.Id != Vm.CurrentSessionId)
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

    private async void CtrlK_Invoked(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        var dlg = new CommandPalette(Vm) { XamlRoot = this.Content.XamlRoot };
        await dlg.ShowAsync();
    }
}
