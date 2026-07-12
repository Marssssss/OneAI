// ChatViewModel — port of Android/macOS VM. Drives the C facade via P/Invoke.
// The native streaming callback fires on a tokio worker thread; marshalled
// to the UI thread via DispatcherQueue (the WinUI counterpart of
// DispatchQueue.main / runOnUiThread).
//
// Multi-agent scenario support (group chat): when a Scenario is active, events
// carry a `speaker` id and are routed to that member's bubble; the VM mirrors
// the macOS ChatViewModel (active-speaker routing, topic intake, debrief,
// streaming coalescing to ~20fps so per-token TryEnqueue doesn't flood the UI
// thread — the macOS beachball root cause).

using System.Collections.ObjectModel;
using System.Text.Json;
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
    /// <summary>Group-chat session when CurrentScenario != null.</summary>
    private IntPtr _groupSession = IntPtr.Zero;
    /// <summary>The AssistantItem currently accumulating events for the active
    /// speaker. Reset to null at the start of each round; the first event
    /// (or a speaker change) seeds a new item.</summary>
    private AssistantItem? _activeSpeakerItem;

    public ProviderConfig Provider { get; }
    public ObservableCollection<ChatItem> Items { get; } = new();
    public ObservableCollection<SessionInfo> Sessions { get; } = new();
    /// <summary>Multi-agent scenario library (presets + user-edited).</summary>
    public ScenarioStore AgentStore { get; } = new();

    private string _input = "";
    public string Input { get => _input; set { SetProperty(ref _input, value); Raise(nameof(HasInput)); } }
    private bool _running;
    public bool Running
    {
        get => _running;
        set { SetProperty(ref _running, value); Raise(nameof(SendButtonVis)); Raise(nameof(StopButtonVis)); Raise(nameof(TurnStatusLabel)); }
    }
    private string? _error;
    public string? Error { get => _error; set { SetProperty(ref _error, value); Raise(nameof(HasErrorVis)); } }
    private long _streamTick;
    public long StreamTick { get => _streamTick; set => SetProperty(ref _streamTick, value); }
    private string? _currentSessionId;
    public string? CurrentSessionId { get => _currentSessionId; set => SetProperty(ref _currentSessionId, value); }
    /// <summary>Active scenario for the current conversation; null = single-agent chat.</summary>
    private Scenario? _currentScenario;
    public Scenario? CurrentScenario
    {
        get => _currentScenario;
        set
        {
            SetProperty(ref _currentScenario, value);
            Raise(nameof(HasScenarioVis)); Raise(nameof(DebriefButtonVis)); Raise(nameof(DebriefPhaseVis));
            Raise(nameof(CurrentScenarioIcon)); Raise(nameof(CurrentScenarioName)); Raise(nameof(TurnStatusLabel));
        }
    }
    /// <summary>A scenario the user picked but hasn't confirmed the topic for
    /// yet. When non-null, the chat detail renders an inline topic-intake page
    /// in place of the conversation. Set by tapping a scenario in the sidebar;
    /// cleared by confirm/cancel.</summary>
    private Scenario? _pendingScenario;
    public Scenario? PendingScenario
    {
        get => _pendingScenario;
        set
        {
            SetProperty(ref _pendingScenario, value);
            Raise(nameof(PendingScenarioVis)); Raise(nameof(NoPendingScenarioVis));
            Raise(nameof(PendingScenarioIcon)); Raise(nameof(PendingScenarioName)); Raise(nameof(PendingScenarioFields));
        }
    }
    /// <summary>Speaker currently producing events (turn-status bar).</summary>
    private string? _activeSpeakerId;
    public string? ActiveSpeakerId
    {
        get => _activeSpeakerId;
        set { SetProperty(ref _activeSpeakerId, value); Raise(nameof(TurnStatusLabel)); }
    }
    /// <summary>True once the current scenario's debrief phase has been
    /// triggered. Drives the top-bar button visibility + phase label; reset on
    /// every new/loaded conversation.</summary>
    private bool _debriefActive;
    public bool DebriefActive
    {
        get => _debriefActive;
        set { SetProperty(ref _debriefActive, value); Raise(nameof(DebriefButtonVis)); Raise(nameof(DebriefPhaseVis)); Raise(nameof(TurnStatusLabel)); }
    }
    /// <summary>Lightweight per-turn token estimate (chars/4) — top-bar indicator.</summary>
    private int _lastTurnTokens;
    public int LastTurnTokens
    {
        get => _lastTurnTokens;
        set { SetProperty(ref _lastTurnTokens, value); Raise(nameof(LastTurnTokensLabel)); }
    }
    public string LastTurnTokensLabel => LastTurnTokens > 0 ? $"{LastTurnTokens} tok" : "";

    public bool NeedsKeyConfig =>
        (Provider.Kind == "openai" || Provider.Kind == "anthropic") && string.IsNullOrEmpty(Provider.ApiKey);

    // Visibility helpers for x:Bind (raise on the underlying props' setters).
    public Visibility NeedsKeyConfigVis => NeedsKeyConfig ? Visibility.Visible : Visibility.Collapsed;
    public Visibility HasErrorVis => Error == null ? Visibility.Collapsed : Visibility.Visible;
    public Visibility SendButtonVis => Running ? Visibility.Collapsed : Visibility.Visible;
    public Visibility StopButtonVis => Running ? Visibility.Visible : Visibility.Collapsed;
    public bool HasInput => !string.IsNullOrWhiteSpace(Input);
    /// <summary>Group-chat turn-status bar visibility.</summary>
    public Visibility HasScenarioVis => CurrentScenario != null ? Visibility.Visible : Visibility.Collapsed;
    /// <summary>Inline topic-intake page takes over the detail when a scenario is
    /// picked but not yet confirmed.</summary>
    public Visibility PendingScenarioVis => PendingScenario != null ? Visibility.Visible : Visibility.Collapsed;
    public Visibility NoPendingScenarioVis => PendingScenario == null ? Visibility.Visible : Visibility.Collapsed;
    public Visibility DebriefButtonVis => (CurrentScenario?.Debrief != null && !DebriefActive) ? Visibility.Visible : Visibility.Collapsed;
    public Visibility DebriefPhaseVis => DebriefActive ? Visibility.Visible : Visibility.Collapsed;
    // Non-nullable projections of the (nullable) scenario for safe x:Bind.
    public string CurrentScenarioIcon => CurrentScenario?.Icon ?? "";
    public string CurrentScenarioName => CurrentScenario?.Name ?? "";
    public string PendingScenarioIcon => PendingScenario?.Icon ?? "";
    public string PendingScenarioName => PendingScenario?.Name ?? "";
    public List<TopicField> PendingScenarioFields => PendingScenario?.TopicFields ?? new();
    public string TurnStatusLabel => (Running && ActiveSpeakerId != null)
        ? $"{ScenarioStore.SpeakerMeta(ActiveSpeakerId!, CurrentScenario).Name} 正在发言…"
        : "轮到你 — 发送你的回答";

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
        Scenario? savedScenario = CurrentScenario;
        if (_groupSession != IntPtr.Zero) OneAiNative.FreeGroupSession(_groupSession);
        if (_session != IntPtr.Zero) OneAiNative.FreeSession(_session);
        if (_app != IntPtr.Zero) OneAiNative.FreeApp(_app);
        _app = _session = _groupSession = IntPtr.Zero;
        CurrentSessionId = null;
        CurrentScenario = null;
        DebriefActive = false;
        _activeSpeakerItem = null;
        Items.Clear();
        Error = null;
        await EnsureApp();
        await RefreshSessions();
        if (savedScenario != null) await NewConversation(savedScenario, null);
        else if (Sessions.Count > 0) await LoadSession(Sessions[0].Id);
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

    public Task NewConversation() => NewConversation(null, null);

    /// <summary>Convenience for starting without collected topic values.</summary>
    public Task NewConversation(Scenario? scenario) => NewConversation(scenario, null);

    /// <summary>Confirm the inline topic-intake page: bake the collected values
    /// into the scenario and start the conversation.</summary>
    public async Task ConfirmStartScenario(Dictionary<string, string> topicValues)
    {
        var sc = PendingScenario;
        PendingScenario = null;
        if (sc != null) await NewConversation(sc, topicValues);
    }

    /// <summary>Abort the inline topic-intake page.</summary>
    public void CancelPendingScenario() => PendingScenario = null;

    /// <summary>Start a fresh conversation. When scenario is non-null, a
    /// multi-agent group-chat session is created. The collected topicValues
    /// (keyed by field id) are folded into each member's system prompt as
    /// background and into the session title. For scenarios with no opener,
    /// the values are sent as the first user message to kick off the first
    /// round (e.g. writing workshop → writer drafts).</summary>
    public async Task NewConversation(Scenario? scenario, Dictionary<string, string>? topicValues)
    {
        if (_app == IntPtr.Zero) return;
        CurrentScenario = scenario;
        if (_groupSession != IntPtr.Zero) { OneAiNative.FreeGroupSession(_groupSession); _groupSession = IntPtr.Zero; }
        _activeSpeakerItem = null;
        ActiveSpeakerId = null;
        DebriefActive = false;

        if (scenario != null)
        {
            var spec = scenario.SpecDto(Provider.Kind, Provider.ApiKey ?? "", Provider.BaseUrl ?? "",
                                        Provider.Model, topicValues);
            _groupSession = OneAiNative.CreateGroupSession(_app, spec.ToJson());
            if (_groupSession == IntPtr.Zero)
            {
                Error = "场景启动失败: " + (OneAiNative.PtrToUtf8(OneAiNative.LastError()) ?? "unknown");
                CurrentScenario = null;
                _groupSession = IntPtr.Zero;
                Running = false;
                return;
            }
            _session = IntPtr.Zero;
            Items.Clear();
            Error = null;
            CurrentSessionId = null;   // group-chat conversation id is engine-side
            if (scenario.OpenerAgentId != null)
            {
                // Opener speaks first (it knows the topic from its system prompt).
                Running = true;
                await RunGroupStart();
                await RefreshSessions();   // scenario session shows up, titled, immediately
            }
            else
            {
                // No opener — kick off the first round with a user message built
                // from the collected topic values (writing workshop).
                var firstMsg = FirstUserMessage(scenario, topicValues);
                if (!string.IsNullOrEmpty(firstMsg)) await RunGroupTask(firstMsg, addUserItem: true);
                else Running = false;
            }
        }
        else
        {
            // Single-agent path.
            _session = OneAiNative.CreateSession(_app, null);
            CurrentSessionId = OneAiNative.PtrToUtf8(OneAiNative.SessionId(_session));
            Items.Clear();
            Error = null;
        }
    }

    /// <summary>Compose the first user message for a no-opener scenario from its
    /// topic fields + collected values (e.g. writing workshop → "秋天散文").</summary>
    private static string FirstUserMessage(Scenario scenario, Dictionary<string, string>? topicValues)
    {
        if (scenario.TopicFields is null) return "";
        var vals = new List<string>();
        foreach (var f in scenario.TopicFields)
        {
            var v = (topicValues?.GetValueOrDefault(f.Id) ?? "").Trim();
            if (!string.IsNullOrEmpty(v)) vals.Add(v);
        }
        return string.Join(" · ", vals);
    }

    /// <summary>Trigger the scenario's debrief phase (e.g. "结束面试"): switch
    /// the turn policy to a scripted order containing only the debrief member,
    /// then send the summary prompt so that member produces a full-session
    /// summary. Subsequent user messages route only to the debrief member.</summary>
    public async Task EndScenarioDebrief()
    {
        if (Running || _groupSession == IntPtr.Zero || CurrentScenario?.Debrief is not { } debrief || DebriefActive)
            return;
        DebriefActive = true;
        OneAiNative.PtrToUtf8(OneAiNative.GroupSetScriptedOrder(_groupSession,
            JsonSerializer.Serialize(new[] { debrief.DebriefMemberId })));
        // Send the summary prompt as a user turn; the now-singleton order routes
        // only to the debrief member. RunGroupTask handles streaming/save.
        await RunGroupTask(debrief.SummaryPrompt, addUserItem: true);
    }

    public async Task LoadSession(string id)
    {
        if (_app == IntPtr.Zero) return;
        if (_groupSession != IntPtr.Zero) { OneAiNative.FreeGroupSession(_groupSession); _groupSession = IntPtr.Zero; }
        CurrentScenario = null;
        DebriefActive = false;
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
            {
                // Group-chat resume isn't fully wired in v1 (matches macOS); for
                // replayed assistant messages, surface the speaker id as the name.
                var (nm, col, av) = ScenarioStore.SpeakerMeta(m.Speaker ?? "", null);
                var a = new AssistantItem { Text = m.Text, Done = true, SpeakerId = m.Speaker, SpeakerName = nm, SpeakerColor = col, SpeakerAvatar = av };
                Items.Add(a);
            }
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

    // ── Event handling (speaker routing) ──────────────────────────────
    // Route an event to the active speaker's AssistantItem. When the speaker
    // changes (a new member's turn), a fresh AssistantItem is created. For
    // single-agent events (speaker null), each runTask call's first event
    // seeds the item. Mirrors macOS `handle(_:)`.
    public void Handle(ChatEvent ev)
    {
        string? speaker = ev.Speaker;
        // New speaker → start a new item.
        if (speaker != null && (_activeSpeakerItem?.SpeakerId != speaker))
        {
            var item = NewSpeakerItem(speaker);
            _activeSpeakerItem = item;
            Items.Add(item);
            ActiveSpeakerId = speaker;
        }
        else if (_activeSpeakerItem == null)
        {
            // Single-agent (speaker null) — create the turn's item on first event.
            var item = new AssistantItem();
            _activeSpeakerItem = item;
            Items.Add(item);
        }
        var turn = _activeSpeakerItem!;
        HandleEvent(ev, turn);
    }

    /// <summary>Build an AssistantItem for a group-chat speaker, pre-filling its
    /// display name/color/avatar/role from the scenario so the bubble template
    /// can bind them without reaching into the VM.</summary>
    private AssistantItem NewSpeakerItem(string speaker)
    {
        var (name, color, avatar) = ScenarioStore.SpeakerMeta(speaker, CurrentScenario);
        var role = CurrentScenario?.AgentById(speaker)?.Role ?? "";
        return new AssistantItem { SpeakerId = speaker, SpeakerName = name, SpeakerColor = color, SpeakerAvatar = avatar, SpeakerRole = role };
    }

    private void HandleEvent(ChatEvent ev, AssistantItem turn)
    {
        switch (ev.Type)
        {
            case "Thinking":
                turn.ThinkingActive = true; turn.Thinking += ev.Text ?? "";
                break;
            case "StreamChunk":
                // When the first text chunk arrives, thinking just ended. Force a
                // flush so the thinking card switches from "思考中…" to "已深度思考".
                if (turn.ThinkingActive) { turn.ThinkingActive = false; turn.ThinkingDone = true; }
                turn.Streaming = true; turn.Text += ev.Text ?? "";
                break;
            case "ToolCall":
                // Dedup by callId: the engine emits on_tool_calls both mid-stream
                // AND after the iteration completes. Without dedup each call shows
                // two rows.
                if (!turn.Steps.Any(s => s.CallId == (ev.Id ?? "")))
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
                turn.Streaming = false; turn.Done = true;
                LastTurnTokens = ((ev.FinalText?.Length ?? 0) + turn.Thinking.Length) / 4;
                if (CurrentScenario == null) Running = false;
                break;
            case "Error":
                turn.Error = ev.Message; turn.Streaming = false; turn.Done = true; Running = false;
                break;
        }
        StreamTick++;
    }

    public async Task RunTask(string task, bool addUserItem = true)
    {
        _lastUserTask = task;
        if (_groupSession != IntPtr.Zero)
        {
            await RunGroupTask(task, addUserItem);
            return;
        }
        if (_session == IntPtr.Zero) { Error = "session not built"; return; }
        if (addUserItem) Items.Add(new UserItem(task));
        var turn = new AssistantItem();
        _activeSpeakerItem = turn;
        Items.Add(turn);
        Running = true;
        Error = null;

        // Mid-turn save so the new chat shows in the sidebar instantly.
        OneAiNative.SessionSave(_session);
        _ = RefreshSessions();

        var coalescer = new StreamCoalescer(this, _dq);
        await Task.Run(() =>
        {
            OneAiEventCb cb = (ctx, jsonPtr) =>
            {
                string? json = jsonPtr == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(jsonPtr);
                ChatEvent? ev = json is null ? null : ChatEvent.Parse(json);
                if (ev != null) coalescer.OnEvent(ev!);
            };
            IntPtr err = OneAiNative.SessionRunTask(_session, task, cb, IntPtr.Zero);
            string? errMsg = OneAiNative.PtrToUtf8(err);
            coalescer.FlushNow();
            _dq.TryEnqueue(() =>
            {
                turn.Streaming = false; turn.Done = true; Running = false;
                if (errMsg != null) turn.Error = errMsg;
            });
        });
        await RefreshSessions();
    }

    /// <summary>Run the scenario's opener turn (no user message). The opener
    /// knows the topic from its system prompt; its events route via Handle.</summary>
    private async Task RunGroupStart()
    {
        Running = true;
        Error = null;
        _activeSpeakerItem = null;
        ActiveSpeakerId = null;
        var coalescer = new StreamCoalescer(this, _dq);
        await Task.Run(() =>
        {
            OneAiEventCb cb = (ctx, jsonPtr) =>
            {
                string? json = jsonPtr == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(jsonPtr);
                ChatEvent? ev = json is null ? null : ChatEvent.Parse(json);
                if (ev != null) coalescer.OnEvent(ev!);
            };
            IntPtr err = OneAiNative.GroupStart(_groupSession, cb, IntPtr.Zero);
            string? errMsg = OneAiNative.PtrToUtf8(err);
            coalescer.FlushNow();
            _dq.TryEnqueue(() =>
            {
                Running = false;
                if (errMsg != null) Error = errMsg;
            });
        });
    }

    /// <summary>Multi-agent run: append the user item, run the round (each
    /// member's events route to its own item via Handle), stop at the user's
    /// turn.</summary>
    private async Task RunGroupTask(string task, bool addUserItem)
    {
        if (_groupSession == IntPtr.Zero) return;
        if (addUserItem) Items.Add(new UserItem(task));
        _activeSpeakerItem = null;   // a new round starts; first event seeds item
        ActiveSpeakerId = null;
        Running = true;
        Error = null;
        var coalescer = new StreamCoalescer(this, _dq);
        await Task.Run(() =>
        {
            OneAiEventCb cb = (ctx, jsonPtr) =>
            {
                string? json = jsonPtr == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(jsonPtr);
                ChatEvent? ev = json is null ? null : ChatEvent.Parse(json);
                if (ev != null) coalescer.OnEvent(ev!);
            };
            IntPtr err = OneAiNative.GroupRunTask(_groupSession, task, cb, IntPtr.Zero);
            string? errMsg = OneAiNative.PtrToUtf8(err);
            coalescer.FlushNow();
            _dq.TryEnqueue(() =>
            {
                // Attach the error to the active speaker's item (or a fresh one).
                if (_activeSpeakerItem == null)
                { var item = new AssistantItem(); _activeSpeakerItem = item; Items.Add(item); }
                if (errMsg != null) _activeSpeakerItem!.Error = errMsg;
                _activeSpeakerItem!.Streaming = false; _activeSpeakerItem!.Done = true;
                Running = false;
            });
        });
        if (_groupSession != IntPtr.Zero) OneAiNative.GroupSave(_groupSession);
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

    public void Stop()
    {
        if (_groupSession != IntPtr.Zero) OneAiNative.GroupInterrupt(_groupSession);
        if (_session != IntPtr.Zero) OneAiNative.SessionInterrupt(_session);
    }

    public void SaveConfig() => ProviderStore.Save(Provider);
}

/// <summary>Coalesces hot streaming events (StreamChunk/Thinking) into ~20fps
/// batches so per-token <c>DispatcherQueue.TryEnqueue</c> doesn't flood the UI
/// thread — the macOS streaming beachball root cause (the main queue backed up
/// faster than it drained). Non-hot events (tool calls, direct answer,
/// complete, error) flush immediately. <c>FlushNow</c> drains everything before
/// the run returns so the final state is always rendered.</summary>
internal sealed class StreamCoalescer
{
    private readonly ChatViewModel _vm;
    private readonly DispatcherQueue _dq;
    private readonly object _lock = new();
    private readonly Queue<ChatEvent> _pendingHot = new();
    private bool _flushScheduled;
    private static readonly TimeSpan FlushInterval = TimeSpan.FromMilliseconds(50);

    public StreamCoalescer(ChatViewModel vm, DispatcherQueue dq) { _vm = vm; _dq = dq; }

    public void OnEvent(ChatEvent ev)
    {
        if (IsHot(ev))
        {
            bool schedule;
            lock (_lock)
            {
                _pendingHot.Enqueue(ev);
                schedule = !_flushScheduled;
                if (schedule) _flushScheduled = true;
            }
            if (schedule) _ = Task.Delay(FlushInterval).ContinueWith(_ => Flush());
        }
        else
        {
            // Drain hot buffer first (in order), then this event — immediately.
            List<ChatEvent> pending;
            lock (_lock) { pending = new List<ChatEvent>(_pendingHot); _pendingHot.Clear(); }
            _dq.TryEnqueue(() =>
            {
                foreach (var e in pending) _vm.Handle(e);
                _vm.Handle(ev);
            });
        }
    }

    /// <summary>Drain any buffered hot events right now (used when the run is
    /// about to return so the final tokens are rendered).</summary>
    public void FlushNow()
    {
        List<ChatEvent> pending;
        lock (_lock) { _flushScheduled = false; pending = new List<ChatEvent>(_pendingHot); _pendingHot.Clear(); }
        if (pending.Count == 0) return;
        _dq.TryEnqueue(() => { foreach (var e in pending) _vm.Handle(e); });
    }

    private void Flush()
    {
        List<ChatEvent> pending;
        lock (_lock) { _flushScheduled = false; pending = new List<ChatEvent>(_pendingHot); _pendingHot.Clear(); }
        if (pending.Count == 0) return;
        _dq.TryEnqueue(() => { foreach (var e in pending) _vm.Handle(e); });
    }

    private static bool IsHot(ChatEvent ev) => ev.Type is "StreamChunk" or "Thinking";
}
