// ChatViewModel — port of Android/macOS VM. Drives the C facade via P/Invoke.
// The native streaming callback fires on a tokio worker thread; marshalled
// to the UI thread via DispatcherQueue (the WinUI counterpart of
// DispatchQueue.main / runOnUiThread).

using System;
using System.Collections.ObjectModel;
using System.Threading.Tasks;
using System.Runtime.InteropServices;
using Windows.System;
using OneAI.Native;
using OneAI.Services;

namespace OneAI.ViewModels;

public class ChatViewModel : ObservableObject
{
    private readonly DispatcherQueue _dq;
    private IntPtr _app = IntPtr.Zero;
    private IntPtr _session = IntPtr.Zero;

    public ProviderConfig Provider { get; }
    public ObservableCollection<ChatItem> Items { get; } = new();
    public ObservableCollection<SessionInfo> Sessions { get; } = new();

    private string _input = "";
    public string Input { get => _input; set { SetProperty(ref _input, value); Raise(nameof(HasInput)); } }
    private bool _running;
    public bool Running { get => _running; set { SetProperty(ref _running, value); Raise(nameof(SendButtonVis)); Raise(nameof(StopButtonVis)); } }
    private string? _error;
    public string? Error { get => _error; set { SetProperty(ref _error, value); Raise(nameof(HasErrorVis)); } }
    private long _streamTick;
    public long StreamTick { get => _streamTick; set => SetProperty(ref _streamTick, value); }
    private string? _currentSessionId;
    public string? CurrentSessionId { get => _currentSessionId; set => SetProperty(ref _currentSessionId, value); }

    public bool NeedsKeyConfig =>
        (Provider.Kind == "openai" || Provider.Kind == "anthropic") && string.IsNullOrEmpty(Provider.ApiKey);

    // Visibility helpers for x:Bind (raise on the underlying props' setters).
    public Visibility NeedsKeyConfigVis => NeedsKeyConfig ? Visibility.Visible : Visibility.Collapsed;
    public Visibility HasErrorVis => Error == null ? Visibility.Collapsed : Visibility.Visible;
    public Visibility SendButtonVis => Running ? Visibility.Collapsed : Visibility.Visible;
    public Visibility StopButtonVis => Running ? Visibility.Visible : Visibility.Collapsed;
    public bool HasInput => !string.IsNullOrWhiteSpace(Input);

    private string? _lastUserTask;

    public ChatViewModel()
    {
        _dq = DispatcherQueue.GetForCurrentThread() ?? throw new InvalidOperationException("VM must be created on UI thread");
        Provider = ProviderStore.Load();
    }

    // ── App lifecycle ────────────────────────────────────────────────
    public async Task EnsureApp()
    {
        if (_app != IntPtr.Zero) return;
        Provider.DbPath = ProviderStore.DbPath;
        var json = Provider.ToJson();
        _app = OneAiNative.CreateApp(json);
        if (_app == IntPtr.Zero)
        {
            string err = OneAiNative.PtrToUtf8(OneAiNative.LastError()) ?? "build failed";
            Error = err;
        }
    }

    public async Task RebuildApp()
    {
        if (_session != IntPtr.Zero) OneAiNative.FreeSession(_session);
        if (_app != IntPtr.Zero) OneAiNative.FreeApp(_app);
        _app = _session = IntPtr.Zero;
        CurrentSessionId = null;
        Items.Clear();
        Error = null;
        await EnsureApp();
        await RefreshSessions();
        if (Sessions.Count > 0) await LoadSession(Sessions[0].Id);
        else await NewConversation();
    }

    public async Task RefreshSessions()
    {
        if (_app == IntPtr.Zero) return;
        var json = OneAiNative.PtrToUtf8(OneAiNative.ListConversations(_app));
        var list = SessionInfo.ParseArray(json);
        list.Sort((a, b) => b.UpdatedAtMs.CompareTo(a.UpdatedAtMs));
        Sessions.Clear();
        foreach (var s in list) Sessions.Add(s);
    }

    public async Task NewConversation()
    {
        if (_app == IntPtr.Zero) return;
        _session = OneAiNative.CreateSession(_app, null);
        CurrentSessionId = OneAiNative.PtrToUtf8(OneAiNative.SessionId(_session));
        Items.Clear();
        Error = null;
    }

    public async Task LoadSession(string id)
    {
        if (_app == IntPtr.Zero) return;
        if (_session != IntPtr.Zero) OneAiNative.FreeSession(_session);
        _session = OneAiNative.CreateSession(_app, id);
        CurrentSessionId = OneAiNative.PtrToUtf8(OneAiNative.SessionId(_session));
        Items.Clear();
        Error = null;
        _lastUserTask = null;
        var json = OneAiNative.PtrToUtf8(OneAiNative.SessionMessages(_session));
        foreach (var m in ChatMessage.ParseArray(json))
        {
            if (m.Role == "user" && !string.IsNullOrWhiteSpace(m.Text))
            { Items.Add(new UserItem(m.Text)); _lastUserTask = m.Text; }
            else if (m.Role == "assistant" && !string.IsNullOrWhiteSpace(m.Text))
            { var a = new AssistantItem(); a.Text = m.Text; a.Done = true; Items.Add(a); }
        }
        StreamTick++;
    }

    public async Task DeleteSession(string id)
    {
        if (_app == IntPtr.Zero) return;
        OneAiNative.DeleteConversation(_app, id);
        await RefreshSessions();
        if (id == CurrentSessionId) await NewConversation();
    }

    // ── Run ──────────────────────────────────────────────────────────
    private void Handle(ChatEvent ev, AssistantItem turn)
    {
        switch (ev.Type)
        {
            case "Thinking":
                turn.ThinkingActive = true; turn.Thinking += ev.Text ?? "";
                break;
            case "StreamChunk":
                if (turn.ThinkingActive) { turn.ThinkingActive = false; turn.ThinkingDone = true; }
                turn.Streaming = true; turn.Text += ev.Text ?? "";
                break;
            case "ToolCall":
                turn.Steps.Add(new ToolStep(ev.Id ?? "", ev.Name ?? "", ev.ArgsJson ?? ""));
                break;
            case "ToolResult":
                {
                    ToolStep? step = null;
                    foreach (var s in turn.Steps) if (s.CallId == ev.CallId) { step = s; break; }
                    if (step == null)
                        for (int i = turn.Steps.Count - 1; i >= 0; i--)
                            if (turn.Steps[i].Result == null) { step = turn.Steps[i]; break; }
                    if (step != null) { step.Result = ev.Content; step.Ok = ev.Success; }
                }
                break;
            case "DirectAnswer":
                if (!string.IsNullOrEmpty(ev.Text)) turn.Text = ev.Text;
                if (turn.ThinkingActive) { turn.ThinkingActive = false; turn.ThinkingDone = true; }
                break;
            case "Complete":
                if (!string.IsNullOrEmpty(ev.FinalText)) turn.Text = ev.FinalText;
                if (turn.ThinkingActive) { turn.ThinkingActive = false; turn.ThinkingDone = true; }
                turn.Streaming = false; turn.Done = true; Running = false;
                break;
            case "Error":
                turn.Error = ev.Message; turn.Streaming = false; turn.Done = true; Running = false;
                break;
        }
        StreamTick++;
    }

    public async Task RunTask(string task, bool addUserItem = true)
    {
        if (_session == IntPtr.Zero) { Error = "session not built"; return; }
        if (addUserItem) Items.Add(new UserItem(task));
        _lastUserTask = task;
        var turn = new AssistantItem();
        Items.Add(turn);
        Running = true;
        Error = null;

        // Mid-turn save so the new chat shows in the sidebar instantly.
        OneAiNative.SessionSave(_session);
        _ = RefreshSessions();

        await Task.Run(() =>
        {
            // The delegate captures `turn`; rooted by this frame for the call.
            OneAiEventCb cb = (ctx, jsonPtr) =>
            {
                string? json = jsonPtr == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(jsonPtr);
                ChatEvent? ev = json is null ? null : ChatEvent.Parse(json);
                if (ev != null) _dq.TryEnqueue(() => Handle(ev!, turn));
            };
            IntPtr err = OneAiNative.SessionRunTask(_session, task, cb, IntPtr.Zero);
            string? errMsg = OneAiNative.PtrToUtf8(err);
            _dq.TryEnqueue(() =>
            {
                turn.Streaming = false; turn.Done = true; Running = false;
                if (errMsg != null) turn.Error = errMsg;
            });
        });
        await RefreshSessions();
    }

    public async Task RetryLast()
    {
        if (_lastUserTask == null || Running) return;
        if (Items[^1] is AssistantItem last && last.Error != null)
        {
            Items.RemoveAt(Items.Count - 1);
            await RunTask(_lastUserTask, addUserItem: false);
        }
        else
        {
            await RunTask(_lastUserTask, addUserItem: true);
        }
    }

    public async Task Stop()
    {
        if (_session != IntPtr.Zero) OneAiNative.SessionInterrupt(_session);
    }

    public void SaveConfig() => ProviderStore.Save(Provider);
}
