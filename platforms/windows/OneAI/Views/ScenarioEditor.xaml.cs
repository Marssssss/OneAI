using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using OneAI.ViewModels;
using OneAI.Services;

namespace OneAI.Views;

public sealed partial class ScenarioEditor : ContentDialog
{
    private readonly ScenarioStore _store;
    private Scenario _scenario;
    private readonly List<AgentCard> _agentCards = new();
    private readonly List<FieldCard> _fieldCards = new();
    public static readonly string[] Palette = { "#4D6BFE", "#3B8C5A", "#B68C2E", "#E5484D", "#8A8A8A", "#9B59B6" };

    public ScenarioEditor(Scenario scenario, ScenarioStore store)
    {
        InitializeComponent();
        _scenario = scenario;
        _store = store;
        Loaded += (_, _) => Populate();
    }

    private void Populate()
    {
        NameBox.Text = _scenario.Name;
        IconCombo.SelectedIndex = Math.Max(0, IconCombo.Items.IndexOf(_scenario.Icon));
        if (IconCombo.SelectedIndex < 0) IconCombo.SelectedIndex = 0;
        foreach (var a in _scenario.Agents) AgentsPanel.Children.Add(MakeAgentCard(a));
        foreach (var f in _scenario.TopicFields ?? new()) FieldsPanel.Children.Add(MakeFieldCard(f));

        // Turn policy
        PolicyScripted.IsChecked = _scenario.TurnPolicy == TurnPolicy.Scripted;
        PolicyRoundRobin.IsChecked = _scenario.TurnPolicy == TurnPolicy.RoundRobin;
        PolicyModerator.IsChecked = _scenario.TurnPolicy == TurnPolicy.Moderator;
        ScriptOrderBox.Text = _scenario.ScriptOrder is null ? "" : string.Join(",", _scenario.ScriptOrder);
        RebuildMemberCombos();

        // Debrief
        if (_scenario.Debrief is { } d)
        {
            DebriefToggle.IsOn = true;
            DebriefButtonBox.Text = d.ButtonLabel;
            DebriefSummaryBox.Text = d.SummaryPrompt;
            DebriefPanel.Visibility = Visibility.Visible;
        }
        else DebriefToggle.IsOn = false;

        // Opener
        OpenerLineBox.Text = _scenario.OpenerLine ?? "";
        SyncOpenerCombo();
    }

    private void RebuildMemberCombos()
    {
        // One display string per agent card, IN ORDER — so SelectedIndex maps
        // directly to _agentCards[index]. Don't filter by name (would misalign
        // the index from the card list).
        var display = _agentCards.Select(c => $"{c.NameBox.Text} ({c.Id})").ToList();
        ModeratorCombo.ItemsSource = display;
        DebriefMemberCombo.ItemsSource = display;
        var opener = new List<string> { "(用户先发言)" };
        opener.AddRange(display);
        OpenerCombo.ItemsSource = opener;

        // Restore selection by id.
        if (!string.IsNullOrEmpty(_scenario.ModeratorId))
        {
            int mi = _agentCards.FindIndex(c => c.Id == _scenario.ModeratorId);
            if (mi >= 0) ModeratorCombo.SelectedIndex = mi;
        }
        if (_scenario.Debrief is { } dd)
        {
            int di = _agentCards.FindIndex(c => c.Id == dd.DebriefMemberId);
            if (di >= 0) DebriefMemberCombo.SelectedIndex = di;
        }
        SyncOpenerCombo();
    }

    private void SyncOpenerCombo()
    {
        if (string.IsNullOrEmpty(_scenario.OpenerAgentId)) OpenerCombo.SelectedIndex = 0;
        else
        {
            int oi = _agentCards.FindIndex(c => c.Id == _scenario.OpenerAgentId);
            OpenerCombo.SelectedIndex = oi >= 0 ? oi + 1 : 0; // +1 for "(用户先发言)"
        }
    }

    private FrameworkElement MakeAgentCard(Agent a)
    {
        var card = new AgentCard(a);
        card.Deleted += () => OnAgentDeleted(card);
        _agentCards.Add(card);
        return card;
    }

    private void OnAgentDeleted(AgentCard card)
    {
        _agentCards.Remove(card);
        RebuildMemberCombos();
    }

    private FrameworkElement MakeFieldCard(TopicField f)
    {
        var card = new FieldCard(f, this);
        card.Deleted += () => OnFieldDeleted(card);
        _fieldCards.Add(card);
        return card;
    }

    private void OnFieldDeleted(FieldCard card) => _fieldCards.Remove(card);

    // Called by FieldCard to know the current cast (for visibility options).
    public IEnumerable<(string Id, string Name)> CurrentCast() =>
        _agentCards.Select(c => (c.Id, c.NameBox.Text));

    private void AddAgent_Click(object sender, RoutedEventArgs e)
    {
        var a = new Agent { Id = Guid.NewGuid().ToString("N").Substring(0, 8), Name = "新角色", Color = "#4D6BFE" };
        var card = new AgentCard(a);
        card.Deleted += () => OnAgentDeleted(card);
        _agentCards.Add(card);
        AgentsPanel.Children.Add(card);
        RebuildMemberCombos();
        // Existing field cards need their visibility options refreshed for the new member.
        foreach (var fc in _fieldCards) fc.RefreshVisibility();
    }

    private void AddField_Click(object sender, RoutedEventArgs e)
    {
        var f = new TopicField { Id = Guid.NewGuid().ToString("N").Substring(0, 8) };
        var card = new FieldCard(f, this);
        card.Deleted += () => OnFieldDeleted(card);
        _fieldCards.Add(card);
        FieldsPanel.Children.Add(card);
    }

    private void Debrief_Toggled(object sender, RoutedEventArgs e) =>
        DebriefPanel.Visibility = DebriefToggle.IsOn ? Visibility.Visible : Visibility.Collapsed;

    protected override void OnPrimaryButtonClick(ContentDialogButtonClickEventArgs args)
    {
        args.Cancel = true; // validate then close
        // Read all controls back into _scenario.
        var sc = new Scenario
        {
            Id = _scenario.Id,
            Name = string.IsNullOrWhiteSpace(NameBox.Text) ? "新场景" : NameBox.Text.Trim(),
            Icon = (string)IconCombo.SelectedItem,
            Agents = _agentCards.Select(c => new Agent
            {
                Id = c.Id,
                Name = c.NameBox.Text,
                Role = c.RoleBox.Text,
                SystemPrompt = c.PromptBox.Text,
                Model = string.IsNullOrWhiteSpace(c.ModelBox.Text) ? null : c.ModelBox.Text,
                Color = (string)c.ColorCombo.SelectedItem,
                Avatar = c.AvatarBox.Text,
                Kind = null, ApiKey = null, BaseUrl = null,
            }).Where(a => !string.IsNullOrWhiteSpace(a.Name)).ToList(),
            TopicFields = _fieldCards.Count == 0 ? null : _fieldCards.Select(c => new TopicField
            {
                Id = c.Field.Id,
                Label = c.LabelBox.Text,
                Placeholder = string.IsNullOrWhiteSpace(c.PlaceBox.Text) ? null : c.PlaceBox.Text,
                VisibleTo = c.VisibilityCombo.SelectedIndex <= 0 ? null : new List<string> { c.VisibleMemberId },
            }).ToList(),
        };
        sc.TurnPolicy = PolicyScripted.IsChecked == true ? TurnPolicy.Scripted
                      : PolicyRoundRobin.IsChecked == true ? TurnPolicy.RoundRobin
                      : TurnPolicy.Moderator;
        sc.ScriptOrder = string.IsNullOrWhiteSpace(ScriptOrderBox.Text)
            ? null : ScriptOrderBox.Text.Split(',').Select(s => s.Trim()).Where(s => s.Length > 0).ToList();
        if (PolicyModerator.IsChecked == true && ModeratorCombo.SelectedIndex >= 0)
            sc.ModeratorId = _agentCards[ModeratorCombo.SelectedIndex].Id;
        if (OpenerCombo.SelectedIndex > 0)
            sc.OpenerAgentId = _agentCards[OpenerCombo.SelectedIndex - 1].Id;
        sc.OpenerLine = string.IsNullOrWhiteSpace(OpenerLineBox.Text) ? null : OpenerLineBox.Text;
        if (DebriefToggle.IsOn && DebriefMemberCombo.SelectedIndex >= 0)
        {
            sc.Debrief = new DebriefConfig
            {
                ButtonLabel = string.IsNullOrWhiteSpace(DebriefButtonBox.Text) ? "结束" : DebriefButtonBox.Text,
                SummaryPrompt = DebriefSummaryBox.Text,
                DebriefMemberId = _agentCards[DebriefMemberCombo.SelectedIndex].Id,
            };
        }
        if (sc.Agents.Count == 0) { return; } // require ≥1 member
        _store.Upsert(sc);
        Hide();
    }
}

/// <summary>One editable agent card.</summary>
internal sealed class AgentCard : Border
{
    public string Id { get; }
    public TextBox NameBox { get; }
    public TextBox RoleBox { get; }
    public TextBox PromptBox { get; }
    public TextBox ModelBox { get; }
    public TextBox AvatarBox { get; }
    public ComboBox ColorCombo { get; }
    public event Action? Deleted;

    public AgentCard(Agent a)
    {
        Id = a.Id;
        CornerRadius = new CornerRadius(8);
        Padding = new Thickness(10);
        Background = (Brush)Application.Current.Resources["CardBackgroundFillColorDefaultBrush"];
        NameBox = new TextBox { PlaceholderText = "名字", Text = a.Name, Header = "名字" };
        RoleBox = new TextBox { PlaceholderText = "角色", Text = a.Role, Header = "角色" };
        PromptBox = new TextBox { PlaceholderText = "系统提示词", Text = a.SystemPrompt, Header = "系统提示词", AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, MinHeight = 60, MaxHeight = 120 };
        ModelBox = new TextBox { PlaceholderText = "model(空=继承)", Text = a.Model ?? "" };
        AvatarBox = new TextBox { PlaceholderText = "头像(emoji)", Text = a.Avatar };
        ColorCombo = new ComboBox { Header = "配色", ItemsSource = new List<string>(ScenarioEditor.Palette) };
        ColorCombo.SelectedIndex = Math.Max(0, Array.IndexOf(ScenarioEditor.Palette, a.Color));
        var del = new Button { Content = "🗑", Background = new SolidColorBrush(Windows.UI.Colors.Transparent), BorderThickness = new Thickness(0) };
        del.Click += (_, _) => { (Parent as Panel)?.Children.Remove(this); Deleted?.Invoke(); };
        var header = new Grid { ColumnDefinitions = { new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) }, new ColumnDefinition { Width = GridLength.Auto } } };
        Grid.SetColumn(NameBox, 0); header.Children.Add(NameBox);
        Grid.SetColumn(del, 1); header.Children.Add(del);
        var sp = new StackPanel { Spacing = 6 };
        sp.Children.Add(header);
        sp.Children.Add(RoleBox);
        sp.Children.Add(PromptBox);
        var colorRow = new Grid { ColumnDefinitions = { new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) }, new ColumnDefinition { Width = GridLength.Auto } } };
        Grid.SetColumn(ModelBox, 0); colorRow.Children.Add(ModelBox);
        Grid.SetColumn(ColorCombo, 1); colorRow.Children.Add(ColorCombo);
        sp.Children.Add(colorRow);
        sp.Children.Add(AvatarBox);
        Child = sp;
    }
}

/// <summary>One editable topic-field card.</summary>
internal sealed class FieldCard : Border
{
    public TopicField Field { get; }
    public TextBox LabelBox { get; }
    public TextBox PlaceBox { get; }
    public ComboBox VisibilityCombo { get; }
    /// <summary>Member id selected when VisibilityCombo.SelectedIndex &gt; 0.</summary>
    public string? VisibilityMemberId { get; private set; }
    public event Action? Deleted;
    private readonly ScenarioEditor _editor;
    private List<string?> _visibilityIds = new();

    public FieldCard(TopicField f, ScenarioEditor editor)
    {
        Field = f;
        _editor = editor;
        CornerRadius = new CornerRadius(6);
        Padding = new Thickness(8);
        Background = (Brush)Application.Current.Resources["LayerFillColorDefaultBrush"];
        LabelBox = new TextBox { PlaceholderText = "字段名", Text = f.Label };
        PlaceBox = new TextBox { PlaceholderText = "占位提示(可选)", Text = f.Placeholder ?? "" };
        VisibilityCombo = new ComboBox { Header = "可见性" };
        var del = new Button { Content = "🗑", Background = new SolidColorBrush(Windows.UI.Colors.Transparent), BorderThickness = new Thickness(0) };
        del.Click += (_, _) => { (Parent as Panel)?.Children.Remove(this); Deleted?.Invoke(); };
        var row = new Grid { ColumnDefinitions = { new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) }, new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) }, new ColumnDefinition { Width = GridLength.Auto } } };
        Grid.SetColumn(LabelBox, 0); row.Children.Add(LabelBox);
        Grid.SetColumn(PlaceBox, 1); row.Children.Add(PlaceBox);
        Grid.SetColumn(del, 2); row.Children.Add(del);
        var sp = new StackPanel { Spacing = 4 };
        sp.Children.Add(row);
        sp.Children.Add(VisibilityCombo);
        Child = sp;
        RefreshVisibility();
    }

    public void RefreshVisibility()
    {
        var items = new List<string> { "全员可见" };
        _visibilityIds = new List<string?> { null };
        foreach (var (id, name) in _editor.CurrentCast())
        {
            if (string.IsNullOrWhiteSpace(name)) continue;
            items.Add($"仅 {name}");
            _visibilityIds.Add(id);
        }
        int prev = VisibilityCombo.SelectedIndex;
        VisibilityCombo.ItemsSource = items;
        int sel = 0;
        if (Field.VisibleTo is { Count: 1 } vis)
        {
            int idx = _visibilityIds.IndexOf(vis[0]);
            if (idx > 0) sel = idx;
        }
        else if (prev > 0 && prev < items.Count) sel = prev; // keep user's pick across rebuilds
        VisibilityCombo.SelectedIndex = sel;
        // Ensure the SelectionChanged handler is attached once.
        if (!_wired) { _wired = true; VisibilityCombo.SelectionChanged += (_, _) =>
        {
            int i = VisibilityCombo.SelectedIndex;
            VisibilityMemberId = i > 0 && i < _visibilityIds.Count ? _visibilityIds[i] : null;
        }; }
    }
    private bool _wired;
}
