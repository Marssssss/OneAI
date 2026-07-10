// MVVM primitives + chat models. Self-contained INotifyPropertyChanged
// (no external NuGet) so the project builds with just WinUI + System.Text.Json.

using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Runtime.CompilerServices;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;

namespace OneAI.ViewModels;

public abstract class ObservableObject : INotifyPropertyChanged
{
    public event PropertyChangedEventHandler? PropertyChanged;
    protected bool SetProperty<T>(ref T field, T value, [CallerMemberName] string? name = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value)) return false;
        field = value;
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
        return true;
    }
    protected void Raise([CallerMemberName] string? name = null) =>
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

public enum ChatKind { User, Assistant }

public abstract class ChatItem : ObservableObject
{
    public ChatKind Kind { get; }
    protected ChatItem(ChatKind kind) { Kind = kind; }
}

public class UserItem : ChatItem
{
    public string Text { get; }
    public UserItem(string text) : base(ChatKind.User) { Text = text; }
}

public class ToolStep : ObservableObject
{
    public string CallId { get; }
    public string Name { get; }
    public string Args { get; }
    private string? _result;
    public string? Result { get => _result; set { SetProperty(ref _result, value); Raise(nameof(HasResult)); Raise(nameof(ResultPreview)); Raise(nameof(StepIcon)); Raise(nameof(StepIconColor)); } }
    private bool? _ok;
    public bool? Ok { get => _ok; set { SetProperty(ref _ok, value); Raise(nameof(StepIcon)); Raise(nameof(StepIconColor)); } }
    public ToolStep(string callId, string name, string args) { CallId = callId; Name = name; Args = args; }

    // UI helpers
    public string StepIcon => Ok == true ? "✓" : Ok == false ? "✗" : "⚙";
    public Brush StepIconColor => Ok == true ? Green : Ok == false ? Red : Grey;
    public string StepSummary => $"{Name}({Args})";
    public Visibility HasResult => Result == null ? Visibility.Collapsed : Visibility.Visible;
    public string ResultPreview => "    └ " + Trunc(Result ?? "", 200);

    private static string Trunc(string s, int n) => s.Length <= n ? s : s.Substring(0, n);
    private static readonly SolidColorBrush Green = Hex("#3B8C5A");
    private static readonly SolidColorBrush Red = Hex("#E5484D");
    private static readonly SolidColorBrush Grey = Hex("#8A8A8A");
    private static SolidColorBrush Hex(string h)
    {
        byte r = Convert.ToByte(h.Substring(1, 2), 16);
        byte g = Convert.ToByte(h.Substring(3, 2), 16);
        byte b = Convert.ToByte(h.Substring(5, 2), 16);
        return new SolidColorBrush(Windows.UI.Color.FromArgb(255, r, g, b));
    }
}

public class AssistantItem : ChatItem
{
    private string _thinking = "";
    public string Thinking { get => _thinking; set { SetProperty(ref _thinking, value); Raise(nameof(HasThinking)); } }
    private bool _thinkingActive;
    public bool ThinkingActive { get => _thinkingActive; set { SetProperty(ref _thinkingActive, value); Raise(nameof(ThinkingHeader)); } }
    private bool _thinkingDone;
    public bool ThinkingDone { get => _thinkingDone; set { SetProperty(ref _thinkingDone, value); Raise(nameof(ThinkingHeader)); } }
    private bool _thinkingExpanded;
    public bool ThinkingExpanded { get => _thinkingExpanded; set => SetProperty(ref _thinkingExpanded, value); }
    private string _text = "";
    public string Text { get => _text; set { SetProperty(ref _text, value); Raise(nameof(ShowCursor)); } }
    private bool _streaming;
    public bool Streaming { get => _streaming; set { SetProperty(ref _streaming, value); Raise(nameof(ShowCursor)); } }
    private bool _done;
    public bool Done { get => _done; set => SetProperty(ref _done, value); }
    private string? _error;
    public string? Error { get => _error; set { SetProperty(ref _error, value); Raise(nameof(HasError)); Raise(nameof(ErrorWithPrefix)); } }
    public ObservableCollection<ToolStep> Steps { get; } = new();
    public AssistantItem() : base(ChatKind.Assistant) { }

    // UI helpers
    public Visibility HasThinking => string.IsNullOrEmpty(Thinking) ? Visibility.Collapsed : Visibility.Visible;
    public string ThinkingHeader => ThinkingActive ? "思考中…" : "已深度思考";
    public Visibility ShowCursor => (Streaming && !string.IsNullOrEmpty(Text)) ? Visibility.Visible : Visibility.Collapsed;
    public Visibility HasError => Error == null ? Visibility.Collapsed : Visibility.Visible;
    public string ErrorWithPrefix => "✗ " + (Error ?? "");
}
