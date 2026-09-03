// ChatViewModel — port of Android/macOS VM. Drives the engine through the
// 3-symbol bus pump (Native/BusPump.cs): every action is a Directive JSON
// submitted via oneai_submit_directive; every result is an EngineYield JSON
// line drained by the pump's dedicated poll thread and routed here.
//
// The pump thread (NOT the UI thread) raises YieldReceived; the VM marshals
// render work to the UI thread via DispatcherQueue — streaming fragments go
// through StreamCoalescer (~20fps batching, the macOS beachball fix), and
// request/response ops (session create/load/delete/list) correlate their yield
// via a single-flight TaskCompletionSource slot.
//
// Multi-agent scenario support (group chat): fragment yields carry a `speaker`
// id routed to that member's bubble (active-speaker routing, topic intake,
// debrief) — mirrors the macOS ChatViewModel.

using System.Collections.ObjectModel;
using System.Linq;
using System.Text.Json;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using OneAI.Native;
using OneAI.Services;

namespace OneAI.ViewModels;

public class ChatViewModel : ObservableObject
{
    private readonly Microsoft.UI.Dispatching.DispatcherQueue _dq;
    /// <summary>The in-process engine bus driver (3-symbol pump). Null until
    /// EnsureApp builds the engine.</summary>
    private BusPump? _pump;
    private StreamCoalescer _coalescer;
    /// <summary>True once Directive::Init built the engine.</summary>
    private bool _engineReady;
    /// <summary>True while a group-chat scenario is the active conversation.</summary>
    private bool _inGroupMode;
    /// <summary>Set when start_group_chat is submitted, cleared on the first
    /// successful group yield (or reset on a group-start error yield) — lets us
    /// attribute an early `error` yield to a failed group build.</summary>
    private volatile bool _groupStarting;
    /// <summary>The AssistantItem currently accumulating events for the active
    /// speaker. Reset to null at the start of each round; the first event
    /// (or a speaker change) seeds a new item.</summary>
    private AssistantItem? _activeSpeakerItem;

    // ── Request/response correlation (single-flight per op) ─────────────
    // The bus is fire-and-observe: a submitted directive's result arrives later
    // as a yield on the pump thread. Each awaitable op parks a
    // TaskCompletionSource here; the matching yield completes it.
    private TaskCompletionSource<string>? _pendingSessionCreated;
    private TaskCompletionSource<JsonElement>? _pendingSessionLoaded;
    private TaskCompletionSource<string>? _pendingSessionDeleted;
    private TaskCompletionSource<JsonElement>? _pendingSessionList;
    private readonly object _corrLock = new();
    private const int AwaitTimeoutMs = 5000;

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
        set { SetProperty(ref _running, value); Raise(nameof(SendButtonVis)); Raise(nameof(StopButtonVis)); Raise(nameof(TurnStatusLabel)); Raise(nameof(WelcomeVis)); }
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
            Raise(nameof(HasScenarioVis)); Raise(nameof(NoScenarioVis)); Raise(nameof(DebriefButtonVis)); Raise(nameof(DebriefPhaseVis));
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
            Raise(nameof(PendingScenarioVis)); Raise(nameof(NoPendingScenarioVis)); Raise(nameof(WelcomeVis));
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
    /// <summary>Per-turn token usage (real counts from the token_usage yield,
    /// chars/4 fallback) — top-bar indicator.</summary>
    private int _lastTurnTokens;
    public int LastTurnTokens
    {
        get => _lastTurnTokens;
        set { SetProperty(ref _lastTurnTokens, value); Raise(nameof(LastTurnTokensLabel)); }
    }
    public string LastTurnTokensLabel => LastTurnTokens > 0 ? $"{LastTurnTokens} tok" : "";

    /// <summary>Starter prompts shown on the welcome screen (mirrors macOS
    /// WelcomeScreen.suggestions). The backing list is static (shared, never
    /// mutated) but exposed as an instance property so x:Bind
    /// `{x:Bind Vm.WelcomeSuggestions}` can reach it (x:Bind can't resolve a
    /// static member through an instance reference).</summary>
    private static readonly IReadOnlyList<WelcomeSuggestion> _welcomeSuggestions = new List<WelcomeSuggestion>
    {
        new() { Icon = "🔍", Text = OneAI.Services.Loc.Str("starter_summarize") },
        new() { Icon = "🔨", Text = OneAI.Services.Loc.Str("starter_rust_json") },
        new() { Icon = "🌐", Text = OneAI.Services.Loc.Str("starter_agent_rag") },
        new() { Icon = "✨", Text = OneAI.Services.Loc.Str("starter_rewrite") },
    };
    public IReadOnlyList<WelcomeSuggestion> WelcomeSuggestions => _welcomeSuggestions;

    public bool NeedsKeyConfig =>
        (Provider.Kind == "openai" || Provider.Kind == "anthropic") && string.IsNullOrEmpty(Provider.ApiKey);

    // Visibility helpers for x:Bind (raise on the underlying props' setters).
    public Visibility NeedsKeyConfigVis => NeedsKeyConfig ? Visibility.Visible : Visibility.Collapsed;

    // First-run onboarding (issue #33): the "添加一个 API Key 开始使用"
    // banner shows only once while NeedsKeyConfig. The dismissal flag is a
    // one-way persisted state — once dismissed (× or by opening settings) the
    // banner never returns. Loaded at construction; flipped by DismissOnboarding.
    public bool OnboardingDismissed { get; private set; }
    public Visibility OnboardingVis =>
        (NeedsKeyConfig && !OnboardingDismissed) ? Visibility.Visible : Visibility.Collapsed;
    public Visibility HasErrorVis => Error == null ? Visibility.Collapsed : Visibility.Visible;
    public Visibility SendButtonVis => Running ? Visibility.Collapsed : Visibility.Visible;
    public Visibility StopButtonVis => Running ? Visibility.Visible : Visibility.Collapsed;
    public bool HasInput => !string.IsNullOrWhiteSpace(Input);
    /// <summary>Group-chat turn-status bar visibility.</summary>
    public Visibility HasScenarioVis => CurrentScenario != null ? Visibility.Visible : Visibility.Collapsed;
    /// <summary>Inverse of HasScenarioVis — drives the single-agent brand logo /
    /// slogan in the top bar (shown only when no scenario is active).</summary>
    public Visibility NoScenarioVis => CurrentScenario == null ? Visibility.Visible : Visibility.Collapsed;
    /// <summary>Inline topic-intake page takes over the detail when a scenario is
    /// picked but not yet confirmed.</summary>
    public Visibility PendingScenarioVis => PendingScenario != null ? Visibility.Visible : Visibility.Collapsed;
    public Visibility NoPendingScenarioVis => PendingScenario == null ? Visibility.Visible : Visibility.Collapsed;
    /// <summary>Default chat surface (welcome screen) when a single-agent
    /// conversation has no messages yet and isn't running — mirrors macOS
    /// `WelcomeScreen`. Disappears the moment the first message lands or a turn
    /// starts. Topic-intake page takes precedence (its own visibility).</summary>
    public Visibility WelcomeVis =>
        (PendingScenario == null && Items.Count == 0 && !Running) ? Visibility.Visible : Visibility.Collapsed;
    public Visibility DebriefButtonVis => (CurrentScenario?.Debrief != null && !DebriefActive) ? Visibility.Visible : Visibility.Collapsed;
    public Visibility DebriefPhaseVis => DebriefActive ? Visibility.Visible : Visibility.Collapsed;
    // Non-nullable projections of the (nullable) scenario for safe x:Bind.
    public string CurrentScenarioIcon => CurrentScenario?.Icon ?? "";
    public string CurrentScenarioName => CurrentScenario?.Name ?? "";
    public string PendingScenarioIcon => PendingScenario?.Icon ?? "";
    public string PendingScenarioName => PendingScenario?.Name ?? "";
    public List<TopicField> PendingScenarioFields => PendingScenario?.TopicFields ?? new();
    public string TurnStatusLabel => (Running && ActiveSpeakerId != null)
        ? $"{ScenarioStore.SpeakerMeta(ActiveSpeakerId!, CurrentScenario).Name} {OneAI.Services.Loc.Str("speaking")}"
        : OneAI.Services.Loc.Str("your_turn");

    private string? _lastUserTask;

    public ChatViewModel(Microsoft.UI.Dispatching.DispatcherQueue dq)
    {
        // The DispatcherQueue is captured upstream in App.OnLaunched (before the
        // Window's XAML loads) and passed in: WinUI 3's GetForCurrentThread()
        // returns null on the UI thread after a Window's InitializeComponent()
        // loads its XAML, even though the queue is still valid. We hold the live
        // reference; TryEnqueue still marshals to the UI thread.
        _dq = dq ?? throw new InvalidOperationException("VM must be created on UI thread");
        Provider = ProviderStore.Load();
        OnboardingDismissed = OnboardingStore.LoadDismissed();
        _coalescer = new StreamCoalescer(this, _dq);
        // Items is an ObservableCollection; add/remove fires CollectionChanged
        // (not PropertyChanged), so the welcome-screen visibility (which reads
        // Items.Count) wouldn't otherwise re-evaluate on the first/last message.
        Items.CollectionChanged += (_, _) => Raise(nameof(WelcomeVis));
    }

    /// <summary>Permanently dismiss the first-run onboarding banner (issue #33).
    /// Persists the one-way flag so the banner never returns on later launches.
    /// Called by the banner's × and "open settings" affordances.</summary>
    public void DismissOnboarding()
    {
        if (OnboardingDismissed) return;
        OnboardingDismissed = true;
        OnboardingStore.SaveDismissed(true);
        Raise(nameof(OnboardingVis));
    }

    // ── Engine lifecycle ───────────────────────────────────────────────

    /// <summary>Build the engine (Directive::Init) + start the pump. Idempotent.
    /// The Init runs off the UI thread so engine construction doesn't stall
    /// WinUI.</summary>
    public async Task EnsureApp()
    {
        if (_engineReady && _pump != null) return;
        Provider.DbPath = ProviderStore.DbPath;
        var configJson = Provider.ToJson();
        var pump = new BusPump();
        _pump = pump;                     // so OnYield (auto-approve) can reach it
        pump.YieldReceived += OnYield;
        pump.Start();
        int code = await Task.Run(() => pump.Init(configJson));
        if (code != OneAiNative.Ok)
        {
            Error = "engine init failed: " + OneAiNative.SubmitCodeMessage(code);
            pump.YieldReceived -= OnYield;
            pump.Shutdown();
            _pump = null;
            return;
        }
        _engineReady = true;
    }

    public async Task RebuildApp()
    {
        Scenario? savedScenario = CurrentScenario;
        // Tear down: stop streaming, abandon pending ops, shut the engine + pump.
        Running = false;
        FailAllPending(new InvalidOperationException("engine is rebuilding"));
        _engineReady = false;
        _inGroupMode = false;
        _groupStarting = false;
        if (_pump != null) { _pump.YieldReceived -= OnYield; _pump.Shutdown(); _pump = null; }
        CurrentSessionId = null;
        CurrentScenario = null;
        DebriefActive = false;
        _activeSpeakerItem = null;
        ActiveSpeakerId = null;
        Items.Clear();
        Error = null;
        await EnsureApp();
        await RefreshSessions();
        if (savedScenario != null) await NewConversation(savedScenario, null);
        else if (Sessions.Count > 0) await LoadSession(Sessions[0].Id);
        else await NewConversation();
    }

    // ── Sidebar (session list) ─────────────────────────────────────────

    public async Task RefreshSessions()
    {
        if (!_engineReady || _pump == null) return;
        var tcs = ResetSlot(ref _pendingSessionList);
        int code = _pump.ListSessions();
        if (code != OneAiNative.Ok) { ClearSlot(ref _pendingSessionList); return; }
        JsonElement payload;
        try { payload = await WithTimeout(tcs.Task); }
        catch (TimeoutException) { return; }   // keep the current sidebar
        catch (OperationCanceledException) { return; }   // superseded by a newer list
        catch (Exception) { return; }   // engine error failed the slot — keep sidebar
        var list = SessionInfo.ParseSessionList(payload)
            .Where(s => !s.Archived)             // archived rows fold away
            .OrderByDescending(s => s.UpdatedAtMs)
            .ToList();
        await RunOnUi(() =>
        {
            Sessions.Clear();
            foreach (var s in list) Sessions.Add(s);
        });
    }

    // ── Conversations ──────────────────────────────────────────────────

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
        if (!_engineReady || _pump == null) return;
        CurrentScenario = scenario;
        _groupStarting = false;
        _activeSpeakerItem = null;
        ActiveSpeakerId = null;
        DebriefActive = false;

        if (scenario != null)
        {
            var spec = scenario.SpecDto(Provider.Kind, Provider.ApiKey ?? "", Provider.BaseUrl ?? "",
                                        Provider.Model, topicValues);
            int code = _pump.StartGroupChat(spec.ToJson());
            if (code != OneAiNative.Ok)
            {
                Error = OneAI.Services.Loc.Str("scenario_failed") + OneAiNative.SubmitCodeMessage(code);
                CurrentScenario = null;
                Running = false;
                return;
            }
            _inGroupMode = true;
            _groupStarting = true;   // an early error yield is a group-build failure
            Items.Clear();
            Error = null;
            CurrentSessionId = null;   // group-chat conversation id is engine-side
            if (scenario.OpenerAgentId != null)
            {
                // Opener speaks first (it knows the topic from its system prompt).
                // The round runs async; turn_complete clears Running + refreshes.
                await RunGroupStart();
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
            // Single-agent path: create a fresh session.
            _inGroupMode = false;
            var tcs = ResetSlot(ref _pendingSessionCreated);
            int code = _pump.CreateSession();
            if (code != OneAiNative.Ok) { ClearSlot(ref _pendingSessionCreated); Error = OneAiNative.SubmitCodeMessage(code); return; }
            try
            {
                var newId = await WithTimeout(tcs.Task);
                await RunOnUi(() =>
                {
                    CurrentSessionId = newId;
                    Items.Clear();
                    Error = null;
                    _activeSpeakerItem = null;
                });
            }
            catch (TimeoutException) { Error = "create session timed out"; }
            catch (OperationCanceledException) { /* superseded by a newer new-conversation */ }
            catch (Exception ex) { Error = ex.Message; }   // engine error failed the slot
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
        if (Running || !_inGroupMode || _pump == null || CurrentScenario?.Debrief is not { } debrief || DebriefActive)
            return;
        DebriefActive = true;
        _pump.GroupSetScriptedOrder(JsonSerializer.Serialize(new[] { debrief.DebriefMemberId }));
        // Send the summary prompt as a user turn; the now-singleton order routes
        // only to the debrief member. RunGroupTask handles streaming.
        await RunGroupTask(debrief.SummaryPrompt, addUserItem: true);
    }

    public async Task LoadSession(string id)
    {
        if (!_engineReady || _pump == null) return;
        _inGroupMode = false;
        _groupStarting = false;
        CurrentScenario = null;
        DebriefActive = false;
        var tcs = ResetSlot(ref _pendingSessionLoaded);
        int code = _pump.LoadSession(id);
        if (code != OneAiNative.Ok) { ClearSlot(ref _pendingSessionLoaded); Error = OneAiNative.SubmitCodeMessage(code); return; }
        JsonElement loaded;
        try { loaded = await WithTimeout(tcs.Task); }
        catch (TimeoutException) { Error = "load session timed out"; return; }
        catch (OperationCanceledException) { return; }   // superseded by a newer load
        catch (Exception ex) { Error = ex.Message; return; }   // engine error failed the slot

        var resolvedId = loaded.TryGetProperty("id", out var idEl) && idEl.ValueKind == JsonValueKind.String
            ? idEl.GetString() : id;
        var msgs = ChatMessage.ParseSessionLoaded(loaded);

        await RunOnUi(() =>
        {
            CurrentSessionId = resolvedId;
            Items.Clear();
            Error = null;
            _lastUserTask = null;
            _activeSpeakerItem = null;
            ActiveSpeakerId = null;
            // Fold consecutive same-speaker assistant messages into one bubble —
            // a single user turn persists several assistant messages (tool-call
            // preludes + final answer), but the chat view renders the whole turn
            // as ONE bubble (issue #17, mirrors macOS rebuildEntries / Android
            // loadSession). system / tool messages between assistants don't break
            // a turn; an empty-text (tool-call-only) assistant renders no bubble.
            AssistantItem? pending = null;
            foreach (var m in msgs)
            {
                if (m.Role == "user")
                {
                    if (pending != null) { Items.Add(pending); pending = null; }
                    if (!string.IsNullOrWhiteSpace(m.Text))
                    { Items.Add(new UserItem(m.Text)); _lastUserTask = m.Text; }
                }
                else if (m.Role == "assistant")
                {
                    if (string.IsNullOrWhiteSpace(m.Text)) continue; // tool-call-only, no bubble
                    if (pending != null && pending.SpeakerId == m.Speaker)
                    {
                        // Same turn, same speaker — accumulate the text.
                        if (!string.IsNullOrEmpty(pending.Text)) pending.Text += "\n";
                        pending.Text += m.Text;
                    }
                    else
                    {
                        // New turn or speaker change — flush the previous bubble.
                        if (pending != null) Items.Add(pending);
                        var (nm, col, av) = ScenarioStore.SpeakerMeta(m.Speaker ?? "", null);
                        pending = new AssistantItem
                        {
                            Text = m.Text,
                            Done = true,
                            SpeakerId = m.Speaker,
                            SpeakerName = nm,
                            SpeakerColor = col,
                            SpeakerAvatar = av,
                        };
                    }
                }
                // else: empty assistant / system / tool — don't break the turn
            }
            if (pending != null) Items.Add(pending);
            StreamTick++;
        });
    }

    public async Task DeleteSession(string id)
    {
        if (!_engineReady || _pump == null) return;
        var tcs = ResetSlot(ref _pendingSessionDeleted);
        int code = _pump.DeleteSession(id);
        if (code != OneAiNative.Ok) { ClearSlot(ref _pendingSessionDeleted); return; }
        try { await WithTimeout(tcs.Task); }
        catch (TimeoutException) { /* sidebar refresh still runs */ }
        catch (OperationCanceledException) { /* superseded; refresh below is harmless */ }
        catch (Exception) { /* engine error failed the slot; refresh below is harmless */ }
        await RefreshSessions();
        if (id == CurrentSessionId) await NewConversation();
    }

    // ── Pump yield dispatch (runs on the pump's poll thread) ──────────

    private void OnYield(BusYield y)
    {
        switch (y.Kind)
        {
            case "session_created":
                CompleteSlot(ref _pendingSessionCreated, StrProp(y.Json, "id") ?? "");
                return;
            case "session_loaded":
                CompleteSlot(ref _pendingSessionLoaded, y.Json);
                return;
            case "session_deleted":
                CompleteSlot(ref _pendingSessionDeleted, StrProp(y.Json, "id") ?? "");
                return;
            case "session_list":
                CompleteSlot(ref _pendingSessionList, y.Json);
                return;
            case "approval_request":
                AutoApprove(y.Json);
                return;
            case "token_usage":
                UpdateTokens(y.Json);
                return;
            case "speaker_turn":
                _groupStarting = false;   // group is up and speaking
                var sp = StrProp(y.Json, "speaker");
                if (sp != null) _dq.TryEnqueue(() => ActiveSpeakerId = sp);
                return;
            case "error":
                HandleErrorYield(y.Json);
                return;
        }

        // Fragment / turn yields → the render path. A fragment proves a group
        // started successfully (clears the group-build-failure window).
        if (_groupStarting && y.Kind is "stream_chunk" or "thinking" or "direct_answer" or "turn_complete")
            _groupStarting = false;
        foreach (var ev in ChatEvent.FromBusYield(y)) _coalescer.OnEvent(ev);
    }

    /// <summary>Auto-proceed a tool-approval request — mirrors the macOS
    /// sidecar's <c>respondApproval</c> fire-and-forget (the app has no approval
    /// UI yet; the iOS UIAlertController pattern is the future affordance).</summary>
    private void AutoApprove(JsonElement j)
    {
        var rid = StrProp(j, "request_id");
        if (rid != null) _pump?.RespondApproval(rid, proceed: true);
    }

    private void UpdateTokens(JsonElement j)
    {
        if (!j.TryGetProperty("usage", out var u)) return;
        uint prompt = u.TryGetProperty("prompt_tokens", out var p) && p.TryGetUInt32(out var pv) ? pv : 0;
        uint completion = u.TryGetProperty("completion_tokens", out var c) && c.TryGetUInt32(out var cv) ? cv : 0;
        int total = (int)(prompt + completion);
        _dq.TryEnqueue(() => LastTurnTokens = total);
    }

    private void HandleErrorYield(JsonElement j)
    {
        bool recoverable = j.TryGetProperty("recoverable", out var r) && r.ValueKind == JsonValueKind.True;
        string msg = StrProp(j, "message") ?? "engine error";
        if (!recoverable) FailAllPending(new InvalidOperationException(msg));
        if (_groupStarting)
        {
            // The group failed to build — reset the scenario state.
            _groupStarting = false;
            _inGroupMode = false;
            _dq.TryEnqueue(() =>
            {
                CurrentScenario = null;
                Running = false;
                Error = msg;
            });
            return;
        }
        _coalescer.OnEvent(new ChatEvent { Type = "Error", Message = msg });
    }

    // ── Correlation slot helpers (thread-safe) ────────────────────────

    private TaskCompletionSource<T> ResetSlot<T>(ref TaskCompletionSource<T>? slot)
    {
        lock (_corrLock)
        {
            slot?.TrySetCanceled();
            slot = new TaskCompletionSource<T>(TaskCreationOptions.RunContinuationsAsynchronously);
            return slot;
        }
    }
    private void CompleteSlot<T>(ref TaskCompletionSource<T>? slot, T value)
    {
        TaskCompletionSource<T>? tcs;
        lock (_corrLock) { tcs = slot; slot = null; }
        tcs?.TrySetResult(value);
    }
    private void ClearSlot<T>(ref TaskCompletionSource<T>? slot)
    {
        TaskCompletionSource<T>? tcs;
        lock (_corrLock) { tcs = slot; slot = null; }
        tcs?.TrySetCanceled();
    }
    private void FailAllPending(Exception ex)
    {
        TaskCompletionSource<string>? c; TaskCompletionSource<JsonElement>? l;
        TaskCompletionSource<string>? d; TaskCompletionSource<JsonElement>? s;
        lock (_corrLock)
        {
            c = _pendingSessionCreated; _pendingSessionCreated = null;
            l = _pendingSessionLoaded; _pendingSessionLoaded = null;
            d = _pendingSessionDeleted; _pendingSessionDeleted = null;
            s = _pendingSessionList; _pendingSessionList = null;
        }
        c?.TrySetException(ex); l?.TrySetException(ex);
        d?.TrySetException(ex); s?.TrySetException(ex);
    }
    private static async Task<T> WithTimeout<T>(Task<T> t)
    {
        var done = await Task.WhenAny(t, Task.Delay(AwaitTimeoutMs));
        return done == t ? await t : throw new TimeoutException();
    }

    /// <summary>Run an action on the UI thread and await it (WinUI 3 has no
    /// SynchronizationContext for async continuations, so an <c>await</c> off a
    /// pump/threadpool completion must marshal UI mutations explicitly).</summary>
    private Task RunOnUi(Action action)
    {
        var tcs = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        bool queued = _dq.TryEnqueue(() =>
        {
            try { action(); tcs.TrySetResult(true); }
            catch (Exception ex) { tcs.TrySetException(ex); }
        });
        if (!queued) tcs.TrySetException(new InvalidOperationException("DispatcherQueue unavailable"));
        return tcs.Task;
    }

    private static string? StrProp(JsonElement parent, string prop) =>
        parent.TryGetProperty(prop, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;

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
                // Fallback estimate only if the token_usage yield didn't land.
                if (LastTurnTokens == 0)
                    LastTurnTokens = ((ev.FinalText?.Length ?? 0) + turn.Thinking.Length) / 4;
                // turn_complete ends a single-agent turn AND a group round.
                Running = false;
                _ = RefreshSessions();   // engine auto-saved; sidebar picks it up
                break;
            case "Error":
                turn.Error = ev.Message; turn.Streaming = false; turn.Done = true; Running = false;
                break;
        }
        StreamTick++;
    }

    // ── Turn submission ───────────────────────────────────────────────

    public async Task RunTask(string task, bool addUserItem = true)
    {
        _lastUserTask = task;
        if (_inGroupMode)
        {
            await RunGroupTask(task, addUserItem);
            return;
        }
        if (!_engineReady || _pump == null) { Error = "engine not ready"; return; }
        if (addUserItem) Items.Add(new UserItem(task));
        var turn = new AssistantItem();
        _activeSpeakerItem = turn;
        Items.Add(turn);
        Running = true;
        Error = null;
        LastTurnTokens = 0;

        int code = _pump.SendUserMessage(task);
        if (code != OneAiNative.Ok)
        {
            turn.Error = OneAiNative.SubmitCodeMessage(code);
            turn.Streaming = false; turn.Done = true; Running = false;
        }
        await Task.CompletedTask;
    }

    /// <summary>Run the scenario's opener turn (no user message). The opener
    /// knows the topic from its system prompt; its events route via Handle. The
    /// round runs async — turn_complete clears Running.</summary>
    private async Task RunGroupStart()
    {
        if (_pump == null) return;
        Running = true;
        Error = null;
        LastTurnTokens = 0;
        _activeSpeakerItem = null;
        ActiveSpeakerId = null;
        int code = _pump.GroupStart();
        if (code != OneAiNative.Ok)
        {
            _groupStarting = false; _inGroupMode = false;
            Running = false;
            Error = OneAiNative.SubmitCodeMessage(code);
        }
        await Task.CompletedTask;
    }

    /// <summary>Multi-agent run: append the user item, submit the round (each
    /// member's events route to its own item via Handle). The round runs async —
    /// turn_complete clears Running + refreshes the sidebar.</summary>
    private async Task RunGroupTask(string task, bool addUserItem)
    {
        if (!_inGroupMode || _pump == null) return;
        if (addUserItem) Items.Add(new UserItem(task));
        _activeSpeakerItem = null;   // a new round starts; first event seeds item
        ActiveSpeakerId = null;
        Running = true;
        Error = null;
        LastTurnTokens = 0;
        int code = _pump.GroupUserMessage(task);
        if (code != OneAiNative.Ok)
        {
            Running = false;
            Error = OneAiNative.SubmitCodeMessage(code);
        }
        await Task.CompletedTask;
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
        // Cooperative interrupt — the bus fires the cancel token for a
        // single-agent turn; the facade intercepts it for an active group round.
        _pump?.SendInterrupt("user stopped");
    }

    public void SaveConfig()
    {
        ProviderStore.Save(Provider);
        // The Provider's ApiKey/Kind are mutated directly in SettingsDialog before
        // this is called; NeedsKeyConfigVis is a computed read of them, so without
        // a raise the x:Bind won't re-evaluate and the "未配置 API Key" hint stays
        // visible after a successful save. Re-announce it now that the key is set.
        // OnboardingVis reads NeedsKeyConfig too — raise it alongside (the banner
        // vanishes once a key is configured, regardless of the dismissal flag).
        Raise(nameof(NeedsKeyConfigVis));
        Raise(nameof(OnboardingVis));
    }
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
    private readonly Microsoft.UI.Dispatching.DispatcherQueue _dq;
    private readonly object _lock = new();
    private readonly Queue<ChatEvent> _pendingHot = new();
    private bool _flushScheduled;
    private static readonly TimeSpan FlushInterval = TimeSpan.FromMilliseconds(50);

    public StreamCoalescer(ChatViewModel vm, Microsoft.UI.Dispatching.DispatcherQueue dq) { _vm = vm; _dq = dq; }

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
