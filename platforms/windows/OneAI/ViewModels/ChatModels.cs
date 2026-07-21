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

/// <summary>A starter prompt on the welcome screen (mirrors macOS
/// WelcomeScreen.suggestions). Tapping it sends the text directly.</summary>
public class WelcomeSuggestion
{
    public string Icon { get; init; } = "";
    public string Text { get; init; } = "";
}

/// <summary>Hex "#RRGGBB"/"RRGGBB" → Windows.UI.Color helpers, shared by
/// ToolStep icons and the per-speaker accent colors.</summary>
public static class ColorUtil
{
    public static Windows.UI.Color FromHex(string hex)
    {
        var s = hex?.Trim() ?? "";
        if (s.StartsWith("#")) s = s[1..];
        if (s.Length < 6) s = "888888";
        byte r = Convert.ToByte(s.Substring(0, 2), 16);
        byte g = Convert.ToByte(s.Substring(2, 2), 16);
        byte b = Convert.ToByte(s.Substring(4, 2), 16);
        return Windows.UI.Color.FromArgb(255, r, g, b);
    }
    public static SolidColorBrush BrushFromHex(string hex) => new SolidColorBrush(FromHex(hex));
    /// <summary>The speaker color at ~18% alpha — for the role-pill background.</summary>
    public static Windows.UI.Color FaintFromHex(string hex)
    {
        var c = FromHex(hex);
        return Windows.UI.Color.FromArgb(46, c.R, c.G, c.B);
    }
}

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
    /// <summary>Which member produced this item. null = single-agent (legacy).</summary>
    public string? SpeakerId { get; set; }
    /// <summary>Speaker display name / color / avatar / role — populated by the VM
    /// from ScenarioStore.SpeakerMeta when the item is created, so the bubble
    /// template can bind them without reaching back into the VM.</summary>
    public string SpeakerName { get; set; } = "";
    public string SpeakerColor { get; set; } = "#8A8A8A";
    public string SpeakerAvatar { get; set; } = "";
    public string SpeakerRole { get; set; } = "";
    private string _thinking = "";
    public string Thinking { get => _thinking; set { SetProperty(ref _thinking, value); Raise(nameof(HasThinking)); } }
    private bool _thinkingActive;
    public bool ThinkingActive { get => _thinkingActive; set { SetProperty(ref _thinkingActive, value); Raise(nameof(ThinkingHeader)); } }
    private bool _thinkingDone;
    public bool ThinkingDone { get => _thinkingDone; set { SetProperty(ref _thinkingDone, value); Raise(nameof(ThinkingHeader)); } }
    private bool _thinkingExpanded;
    public bool ThinkingExpanded { get => _thinkingExpanded; set => SetProperty(ref _thinkingExpanded, value); }
    private string _text = "";
    public string Text
    {
        get => _text;
        set
        {
            SetProperty(ref _text, value);
            Raise(nameof(ShowCursor));
            Raise(nameof(ShowStreamingTextVis)); Raise(nameof(ShowMarkdownVis));
            // Computed-from-Text views must refresh with each token.
            Raise(nameof(StreamingDisplay)); Raise(nameof(StreamingWithCursor));
        }
    }
    private bool _streaming;
    public bool Streaming
    {
        get => _streaming;
        set { SetProperty(ref _streaming, value); Raise(nameof(ShowCursor)); Raise(nameof(ShowStreamingTextVis)); Raise(nameof(ShowMarkdownVis)); Raise(nameof(MarkdownText)); }
    }
    private bool _done;
    public bool Done
    {
        get => _done;
        set { SetProperty(ref _done, value); Raise(nameof(ShowStreamingTextVis)); Raise(nameof(ShowMarkdownVis)); Raise(nameof(MarkdownText)); }
    }
    /// <summary>The text to render as markdown — read-through of <see cref="Text"/>,
    /// but raised ONLY on the Streaming/Done flips (NOT on every Text/token change).
    /// `ChatView.xaml` binds `MarkdownTextBlock.Markdown` to this (not to `Text`)
    /// so the per-token Text growth during streaming doesn't push a new value into
    /// the Markdown DP → OnChanged → full Render() rebuild on every token (the
    /// streaming lag root cause — the control is Collapsed while streaming, but the
    /// DP update + Render fires anyway). The full markdown renders once, on Done.</summary>
    public string MarkdownText => Text;
    private string? _error;
    public string? Error { get => _error; set { SetProperty(ref _error, value); Raise(nameof(HasError)); Raise(nameof(ErrorWithPrefix)); } }
    public ObservableCollection<ToolStep> Steps { get; } = new();
    public AssistantItem() : base(ChatKind.Assistant) { }

    // UI helpers
    /// <summary>True when this bubble belongs to a named group-chat member (show
    /// the speaker header + left accent bar). False for single-agent turns.</summary>
    public Visibility HasSpeaker => string.IsNullOrEmpty(SpeakerId) ? Visibility.Collapsed : Visibility.Visible;
    /// <summary>Extra top gap for group-chat speaker bubbles so consecutive roles
    /// (e.g. 指导员 → 面试官) read as distinct turns rather than one block —
    /// mirrors the macOS AssistantBubble's `.padding(.top, 8)` for speaker items.</summary>
    public Thickness BubbleMargin =>
        string.IsNullOrEmpty(SpeakerId) ? new Thickness(0)
                                         : new Thickness(6, 10, 6, 2);
    public Windows.UI.Color SpeakerColorBrush => ColorUtil.FromHex(SpeakerColor);
    public SolidColorBrush SpeakerBrush => ColorUtil.BrushFromHex(SpeakerColor);
    public Windows.UI.Color SpeakerColorFaint => ColorUtil.FaintFromHex(SpeakerColor);
    public Visibility HasThinking => string.IsNullOrEmpty(Thinking) ? Visibility.Collapsed : Visibility.Visible;
    public string ThinkingHeader => ThinkingActive ? "思考中…" : "已深度思考";
    /// <summary>Streaming partial text capped to the last `cap` chars. A plain
    /// TextBlock re-lays-out its whole content every flush; capping bounds the
    /// CoreText work so a long reply doesn't saturate the UI thread mid-stream.
    /// The full markdown renders once on completion.</summary>
    public const int StreamingCap = 1800;
    public string StreamingDisplay => Text.Length <= StreamingCap ? Text : Text[^StreamingCap..];
    /// <summary>Steady caret appended inline (a separate row reads as a blank line).</summary>
    public string StreamingWithCursor => StreamingDisplay.Trim() + "▍";
    public Visibility ShowCursor => (Streaming && !string.IsNullOrEmpty(Text)) ? Visibility.Visible : Visibility.Collapsed;
    /// <summary>While streaming, render plain Text (capped) — NOT MarkdownText:
    /// re-parsing the growing markdown on every token floods the UI thread on
    /// long replies. The full markdown renders once on Done.</summary>
    public Visibility ShowStreamingTextVis => (Streaming && !Done) ? Visibility.Visible : Visibility.Collapsed;
    public Visibility ShowMarkdownVis => (Streaming && !Done) ? Visibility.Collapsed : Visibility.Visible;
    public Visibility HasError => Error == null ? Visibility.Collapsed : Visibility.Visible;
    public string ErrorWithPrefix => "✗ " + (Error ?? "");
}
