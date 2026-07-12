using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using OneAI.ViewModels;
using OneAI.Native;

namespace OneAI.Views;

/// <summary>One row in the command palette: scenario / session / action.</summary>
public class CmdEntry
{
    public string Icon { get; set; } = "";
    public string Title { get; set; } = "";
    public string Subtitle { get; set; } = "";
    public Func<Task>? Activate { get; set; }
}

public sealed partial class CommandPalette : ContentDialog
{
    private readonly ChatViewModel _vm;
    private List<CmdEntry> _all = new();

    public CommandPalette(ChatViewModel vm)
    {
        InitializeComponent();
        _vm = vm;
        Loaded += (_, _) => { QueryBox.Focus(FocusState.Programmatic); Rebuild(); };
    }

    private void Rebuild()
    {
        _all = new();
        foreach (var sc in _vm.AgentStore.Scenarios)
        {
            var captured = sc;
            _all.Add(new CmdEntry
            {
                Icon = sc.Icon, Title = sc.Name, Subtitle = $"{sc.Agents.Count} 个智能体",
                Activate = () => { _vm.PendingScenario = null; return StartScenario(captured); },
            });
        }
        foreach (var s in _vm.Sessions)
        {
            var captured = s;
            _all.Add(new CmdEntry
            {
                Icon = "💬", Title = string.IsNullOrEmpty(s.Title) ? "新对话" : s.Title,
                Subtitle = $"{s.MessageCount} 条",
                Activate = () => { _ = _vm.LoadSession(captured.Id); return Task.CompletedTask; },
            });
        }
        _all.Add(new CmdEntry { Icon = "➕", Title = "新对话(单 Agent)", Subtitle = "", Activate = () => { _ = _vm.NewConversation(); return Task.CompletedTask; } });
        ApplyFilter();
    }

    private async Task StartScenario(Scenario sc)
    {
        if (sc.TopicFields is { Count: > 0 }) _vm.PendingScenario = sc;
        else await _vm.NewConversation(sc);
    }

    private void ApplyFilter()
    {
        var q = (QueryBox.Text ?? "").Trim().ToLowerInvariant();
        var filtered = string.IsNullOrEmpty(q)
            ? _all
            : _all.Where(e => e.Title.ToLowerInvariant().Contains(q)).ToList();
        Results.ItemsSource = filtered;
    }

    private void QueryBox_TextChanged(object sender, TextChangedEventArgs e) => ApplyFilter();

    private async void Result_Click(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is CmdEntry entry && entry.Activate is { } act)
        {
            Hide();
            await act();
        }
    }

    private void QueryBox_KeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == Windows.System.VirtualKey.Enter)
        {
            e.Handled = true;
            if (Results.ItemsSource is IList<CmdEntry> list && list.Count > 0)
            {
                Hide();
                _ = list[0].Activate?.Invoke();
            }
        }
    }
}
