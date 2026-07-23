// ChatViewModel + models + streaming callback — port of Android's ChatViewModel.
// Events from the Rust tokio worker thread are marshalled to the main thread
// (DispatchQueue.main.async), mirroring Android's runOnUiThread.

import Foundation
import SwiftUI

// MARK: - Stream debug logger
//
// Writes timestamped lines from both the tokio worker thread and the main
// thread to ~/Library/Application Support/oneai_stream.log (truncated on each
// launch). The gap pattern between the two threads localizes a streaming
// beachball:
//   • worker keeps logging token arrivals but main "hb" gaps for seconds
//     → the main thread is BLOCKED (a sync call/lock), not merely busy.
//   • main "hb" keeps firing every 200ms but each "flush" takes >200ms
//     → main-thread CPU saturation (render cost).
//   • worker "tok" lines stop arriving → provider/tokio stalled (network).
// All writes go through a serial queue so the worker thread is never blocked
// by file I/O (it just dispatches). Disable once the freeze is localized.

private enum StreamLog {
    private static let queue = DispatchQueue(label: "ai.oneai.streamlog")
    private static var handle: FileHandle?
    private static let df: DateFormatter = {
        let f = DateFormatter(); f.dateFormat = "HH:mm:ss"; return f
    }()
    static func start() {
        queue.async {
            let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            let url = dir.appendingPathComponent("oneai_stream.log")
            FileManager.default.createFile(atPath: url.path, contents: Data())
            handle = try? FileHandle(forWritingTo: url)
            log("init", "stream log started")
        }
    }
    static func log(_ tag: String, _ msg: String) {
        queue.async {
            guard let h = handle else { return }
            let ms = Int(Date().timeIntervalSince1970 * 1000) % 1000
            let ts = "\(df.string(from: Date())).\(String(format: "%03d", ms))"
            let line = "\(ts)  [\(tag)]  \(msg)\n"
            if let data = line.data(using: .utf8) { h.write(data) }
        }
    }
}

/// Retains the main-runloop heartbeat Timer across re-renders.
private var streamHeartbeatStarted = false

extension ChatViewModel {
    fileprivate static func scheduleHeartbeat() {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) {
            StreamLog.log("main", "hb")
            scheduleHeartbeat()
        }
    }
}

// MARK: - Chat items

struct ToolStep: Identifiable {
    let id = UUID()
    let callId: String
    let name: String
    let args: String
    var result: String? = nil
    var ok: Bool? = nil
}

final class UserItem: Identifiable {
    let id = UUID()
    let text: String
    init(text: String) { self.text = text }
}

// NOTE: AssistantItem is a plain class (NOT ObservableObject/@Published).
// Per-token @Published sends on an ObservableObject re-enter Combine's
// non-reentrant publisher lock during streaming and self-deadlock the main
// thread. Instead, UI refresh is driven solely by the VM's `streamTick`
// (@Published, low-frequency) — `handle()` mutates these plain fields then
// bumps streamTick, so the row re-renders via the parent ForEach.
final class AssistantItem: Identifiable {
    let id = UUID()
    /// Which member produced this item. `nil` = single-agent (legacy).
    var speakerId: String? = nil
    var thinking = ""
    var thinkingActive = false
    var thinkingDone = false
    var thinkingExpanded = false
    var steps: [ToolStep] = []
    var text = ""
    var streaming = false
    var done = false
    var error: String? = nil
    /// Monotonic version, bumped on every mutation (in `handle()`). The macOS
    /// message list is a NON-lazy VStack (stable document height — no
    /// blank-on-send, no tiny-scrollbar, reliable stickiness geometry). That
    /// re-evaluates the ForEach every ~20 fps flush; to bound the cost,
    /// `AssistantBubble` is `.equatable()` and compares a per-render SNAPSHOT
    /// of this version. Done bubbles' version is stable → body skipped (just
    /// an Int compare); only the active streaming bubble (version bumped each
    /// token) re-renders. Without this the non-lazy list re-ran every
    /// bubble's body per flush → main-thread saturation (the streaming freeze)
    /// on long conversations.
    var version: Int = 0
}

enum ChatEntry: Identifiable {
    case user(UserItem)
    case assistant(AssistantItem)
    var id: UUID {
        switch self {
        case .user(let u): return u.id
        case .assistant(let a): return a.id
        }
    }
}

// MARK: - Streaming callback (foreign-implemented ChatEventCallback)

final class StreamCallback: ChatEventCallback, @unchecked Sendable {
    weak var vm: ChatViewModel?
    init(vm: ChatViewModel) { self.vm = vm }

    /// Buffer of coalesced hot fragments (streamChunk/thinking), drained by a
    /// single scheduled flush. Without coalescing, every token fires its own
    /// `DispatchQueue.main.async` — for a long stream the main queue backs up
    /// faster than it drains (each block runs `handle` +, on throttle-boundary,
    /// a full re-render), the main thread never catches up, and the app
    /// beachballs mid-stream. Batching bounds main-queue work to ~20 fps.
    private let lock = NSLock()
    private var pendingHot: [ChatEventView] = []
    private var flushScheduled = false
    /// ~20 fps. Renders are paced by this; if a render overruns it, flushes
    /// naturally back off to render speed (no flooding).
    private static let flushInterval: TimeInterval = 0.05

    func onEvent(event: ChatEventView) {
        // Fires on the tokio worker thread — but confirm: log whether it's
        // actually the main thread. If onEvent runs on main, the Rust future
        // is being driven on the main thread and a slow inference blocks the
        // UI → that's the beachball cause.
        if Self.isHot(event) {
            lock.lock()
            pendingHot.append(event)
            let n = pendingHot.count
            let schedule = !flushScheduled
            if schedule { flushScheduled = true }
            lock.unlock()
            StreamLog.log("worker", "hot pending=\(n) onMain=\(Thread.isMainThread)")
            if schedule {
                DispatchQueue.main.asyncAfter(deadline: .now() + Self.flushInterval) { [weak self] in
                    self?.flush()
                }
            }
        } else {
            lock.lock()
            let pending = pendingHot
            pendingHot.removeAll()
            lock.unlock()
            let kind = Self.eventKind(event)
            StreamLog.log("worker", "nonhot=\(kind) pendingAhead=\(pending.count) onMain=\(Thread.isMainThread)")
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                guard let vm = self.vm else { return }
                for e in pending { vm.handle(e) }
                vm.handle(event)
            }
        }
    }

    private func flush() {
        lock.lock()
        flushScheduled = false
        let pending = pendingHot
        pendingHot.removeAll()
        lock.unlock()
        guard let vm = vm else { return }
        if pending.isEmpty { return }
        let t0 = Date()
        StreamLog.log("main", "flush start pending=\(pending.count)")
        // handle() throttles streamTick internally — the first hot event in the
        // batch bumps (≥flushInterval since the last flush), the rest skip, so
        // the whole batch produces exactly one re-render.
        for e in pending { vm.handle(e) }
        let durMs = Int(Date().timeIntervalSince(t0) * 1000)
        StreamLog.log("main", "flush end dur_ms=\(durMs)")
    }

    private static func eventKind(_ e: ChatEventView) -> String {
        switch e {
        case .streamChunk: return "chunk"
        case .thinking: return "thinking"
        case .toolCall: return "toolCall"
        case .toolResult: return "toolResult"
        case .directAnswer: return "directAnswer"
        case .complete: return "complete"
        case .error: return "error"
        }
    }

    private static func isHot(_ event: ChatEventView) -> Bool {
        switch event {
        case .streamChunk, .thinking: return true
        default: return false
        }
    }
}

// MARK: - In-page overlay (replaces modal .sheet/.alert)

/// The single source of truth for any in-page dialog. Rendering lives in
/// `ChatScreen`'s top-level ZStack (`OverlayLayer`) — NOT in a native
/// `.sheet`/`.alert`. Native sheets rebuild their content view tree from
/// scratch on every open (and animate a modal presentation), which made the
/// heavy dialogs (settings, scenario editor) stutter; an in-page overlay
/// layer stays in the view hierarchy, so presenting it is just a state flip
/// + a cheap opacity transition. Mirrors the `pendingScenario` topic-intake
/// page pattern (a flatter flow than a modal sheet).
enum AppOverlay: Equatable {
    case settings
    case scenarioEditor(Scenario)
    case editMessage(UserItem)
    case commandPalette
    case deleteSession(String)

    /// `UserItem` is a reference type with no Equatable; compare by identity
    /// so `AppOverlay` can be Equatable for animation purposes.
    static func == (lhs: AppOverlay, rhs: AppOverlay) -> Bool {
        switch (lhs, rhs) {
        case (.settings, .settings), (.commandPalette, .commandPalette):
            return true
        case (.scenarioEditor(let a), .scenarioEditor(let b)):
            return a.id == b.id
        case (.editMessage(let a), .editMessage(let b)):
            return a === b
        case (.deleteSession(let a), .deleteSession(let b)):
            return a == b
        default:
            return false
        }
    }
}

// MARK: - View model

/// High-frequency streaming tick, isolated on its OWN ObservableObject.
///
/// Why not a `@Published var streamTick` on `ChatViewModel` directly: every
/// hot-path token (~20 fps while streaming) bumps it, and `@Published` would
/// fire `ChatViewModel.objectWillChange` — which re-renders EVERY view that
/// observes the VM: the Sidebar (all session + scenario rows) and the top bar,
/// on every token. By isolating the tick, only views that explicitly observe
/// `vm.streamTick` re-render on a token; the Sidebar (which observes the VM
/// only) stays put during streaming. The streaming bubble still refreshes
/// because `ChatDetail` observes `vm.streamTick` in addition to the VM.
final class StreamTick: ObservableObject {
    @Published var value: Int64 = 0
}

final class ChatViewModel: ObservableObject {
    private let prefs = UserDefaults(suiteName: "oneai_provider") ?? .standard

    /// Protocol inferred from `baseUrl` (issue 1: the user no longer picks a
    /// `kind` in Settings — only base url / api key / model). Detection:
    ///   • url mentions `anthropic`            → "anthropic"
    ///   • url mentions `ollama` or `:11434`    → "ollama"
    ///   • otherwise (incl. blank + OpenAI-compat relays) → "openai"
    /// Blank base url + a key → openai default endpoint. A blank base url with
    /// no key still reports openai so `needsKeyConfig` can prompt.
    var kind: String { Self.inferKind(baseUrl: baseUrl) }
    @Published var model: String
    @Published var apiKey: String
    @Published var baseUrl: String

    // Embedding provider config (independent of the LLM provider key — the LLM
    // has no embed method). Default "auto": probes VOYAGE_API_KEY / OPENAI_API_KEY
    // / a local Ollama; nothing available → memory recall uses keyword matching.
    // Most users leave provider=auto and never touch these fields.
    private let embPrefs = UserDefaults(suiteName: "oneai_embedding") ?? .standard
    @Published var embProvider: String
    @Published var embModel: String
    @Published var embApiKey: String
    @Published var embBaseUrl: String

    @Published var items: [ChatEntry] = []
    @Published var sessions: [SessionInfoView] = []
    @Published var input = ""
    @Published var running = false
    @Published var error: String? = nil
    /// High-frequency streaming tick. Isolated on `StreamTick` (not a
    /// @Published field here) so token-driven bumps don't re-render the
    /// Sidebar/top bar — see `StreamTick`.
    let streamTick = StreamTick()
    @Published var currentSessionId: String? = nil
    /// Multi-agent scenario library (presets + user-edited).
    @Published var agentStore = AgentStore()
    /// Active scenario for the current conversation; `nil` = single-agent chat.
    @Published var currentScenario: Scenario? = nil
    /// A scenario the user picked but hasn't confirmed the topic for yet.
    /// When non-nil, the chat detail renders an inline topic-intake page in
    /// place of the conversation (a flatter flow than a modal sheet — the
    /// intake lives where the conversation will live). Set by tapping a
    /// scenario in the sidebar; cleared by confirm/cancel.
    @Published var pendingScenario: Scenario? = nil
    /// Speaker currently producing events (for the turn-status bar).
    @Published var activeSpeakerId: String? = nil
    /// True once the current scenario's debrief phase has been triggered (the
    /// "结束面试" button). Drives the top-bar button visibility + the phase
    /// label; reset on every new/loaded conversation.
    @Published var debriefActive: Bool = false
    /// Lightweight per-turn token estimate (chars/4) — surfaced in the top bar.
    @Published var lastTurnTokens: Int = 0
    /// Bumped whenever a session is (re)loaded so the detail view can force a
    /// scroll-to-bottom (issue 7). `onChange(of: items.count)` alone is
    /// unreliable here: a loaded session with the same message count as the
    /// previous one never fires, and `stickToBottom` may be false from a prior
    /// scroll-up — so the history landed mid-conversation instead of at the
    /// most recent message. This dedicated counter fires on every load.
    @Published var scrollRequest: Int = 0
    /// In-page dialog state (settings / scenario editor / edit-message /
    /// command palette / delete-session confirm). Non-nil → `ChatScreen`
    /// renders the overlay layer on top. See `AppOverlay`.
    @Published var overlay: AppOverlay? = nil

    var needsKeyConfig: Bool {
        (kind == "openai" || kind == "anthropic") && apiKey.isEmpty
    }

    /// Map a base url to a provider kind. See `kind` for the rules.
    static func inferKind(baseUrl: String) -> String {
        let url = baseUrl.lowercased()
        if url.contains("anthropic") { return "anthropic" }
        if url.contains("ollama") || url.contains(":11434") { return "ollama" }
        return "openai"
    }

    private var lastUserTask: String? = nil
    private var app: OneAiApp? = nil
    private var session: OneAiSession? = nil
    /// Group-chat session when `currentScenario != nil`.
    private var groupSession: OneAiGroupChatSession? = nil
    /// The AssistantItem currently accumulating events for the active speaker.
    private var activeSpeakerItem: AssistantItem? = nil
    /// Throttle: last time `streamTick` was bumped for a hot-path event
    /// (streamChunk/thinking). Bumping per-token re-renders the whole chat
    /// (incl. full markdown re-parse of the growing bubble) on every token —
    /// for long streams the main queue backs up faster than it drains and the
    /// app beachballs. Coalesce to ~20 fps; `.complete`/`.error` always flush.
    private var lastStreamFlush = Date.distantPast
    private static let streamFlushInterval: TimeInterval = 0.05

    var dbPath: String {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("oneai.db").path
    }

    /// Application Support dir (no trailing file) — passed to
    /// `initOneaiLog` so the Rust `tracing` subscriber writes oneai_rust.log
    /// next to oneai_stream.log / oneai.db.
    var appSupportDir: String {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.path
    }

    init() {
        let p = UserDefaults(suiteName: "oneai_provider") ?? .standard
        model = p.string(forKey: "model") ?? "gpt-4o-mini"
        apiKey = p.string(forKey: "apiKey") ?? ""
        baseUrl = p.string(forKey: "baseUrl") ?? ""
        prefs.register(defaults: ["model": "gpt-4o-mini"])
        embProvider = embPrefs.string(forKey: "provider") ?? "auto"
        embModel = embPrefs.string(forKey: "model") ?? ""
        embApiKey = embPrefs.string(forKey: "apiKey") ?? ""
        embBaseUrl = embPrefs.string(forKey: "baseUrl") ?? ""
        embPrefs.register(defaults: ["provider": "auto"])
    }

    // MARK: Provider config

    func saveConfig() {
        prefs.set(model, forKey: "model")
        prefs.set(apiKey, forKey: "apiKey")
        prefs.set(baseUrl, forKey: "baseUrl")
        embPrefs.set(embProvider, forKey: "provider")
        embPrefs.set(embModel, forKey: "model")
        embPrefs.set(embApiKey, forKey: "apiKey")
        embPrefs.set(embBaseUrl, forKey: "baseUrl")
    }

    private func providerConfigView() -> ProviderConfigView {
        ProviderConfigView(
            kind: kind.isEmpty ? "openai" : kind,
            apiKey: apiKey.isEmpty ? nil : apiKey,
            baseUrl: baseUrl.isEmpty ? nil : baseUrl,
            model: model.isEmpty ? "gpt-4o-mini" : model,
            host: nil,
            port: nil
        )
    }

    /// Build the embedding config view. Returns nil when provider=auto with no
    /// key/base, so the Rust side falls through to zero-config auto-detection.
    private func embeddingConfigView() -> EmbeddingConfigView? {
        let provider = embProvider.isEmpty ? "auto" : embProvider
        if provider == "auto" && embApiKey.isEmpty && embBaseUrl.isEmpty {
            return nil
        }
        return EmbeddingConfigView(
            provider: provider,
            model: embModel.isEmpty ? nil : embModel,
            apiKey: embApiKey.isEmpty ? nil : embApiKey,
            baseUrl: embBaseUrl.isEmpty ? nil : embBaseUrl,
            fallback: nil
        )
    }

    // MARK: App lifecycle

    func ensureApp() async {
        guard app == nil else { return }
        StreamLog.start()
        // Install the Rust-side tracing subscriber → oneai_rust.log in the
        // same dir. Pairs with StreamLog (oneai_stream.log) so create_session /
        // save / run_task on the Rust side are locatable alongside the Swift
        // sess/run events. Idempotent (OnceLock); safe across rebuildApp.
        initOneaiLog(logDir: appSupportDir)
        // Main-thread heartbeat, driven by a self-rescheduling
        // DispatchQueue.main.asyncAfter chain (NOT a Timer — that earlier
        // attempt attached to the wrong runloop because this method runs off
        // the main actor). This chain runs a block on the main queue every
        // 200ms; if the main thread blocks, the next asyncAfter can't fire →
        // a multi-second gap in "hb" lines. That gap localizes the block.
        if !streamHeartbeatStarted {
            streamHeartbeatStarted = true
            Self.scheduleHeartbeat()
        }
        do {
            var builder = OneAiAppBuilder()
            if let emb = embeddingConfigView() {
                builder = try builder.embeddingConfig(cfg: emb)
            }
            builder = try builder.providerConfig(cfg: providerConfigView())
            builder = builder.defaultTools()
            builder = builder.sqlitePersistenceAt(path: dbPath)
            app = try await builder.build()
        } catch {
            self.error = "build failed: \(friendlyError(error))"
        }
    }

    func rebuildApp() async {
        let savedScenario = currentScenario
        // The user may be sitting on the scenario topic-intake page
        // (`pendingScenario` set, `currentScenario` not yet — it's only
        // assigned once the intake is confirmed). Saving settings from EITHER
        // surface must return there, not jump to `sessions.first`. Capture
        // both; restore the intake page in place when it was open.
        let savedPending = pendingScenario
        // Was the user on a real conversation (history loaded / messages
        // exchanged), or on the empty welcome screen? The cold-start `.task`
        // deliberately opens a fresh single-agent chat — NOT the most recent
        // history — so the welcome screen shows. rebuildApp must preserve that:
        // saving settings from the welcome screen must NOT yank the user to
        // `sessions.first` (the last history). We reload the SAME session only
        // when a real conversation was open; otherwise we start fresh again.
        let savedSessionId = currentSessionId
        let hadConversation = !items.isEmpty
        // Tear down ONLY the engine refs (app/session/groupSession) — these
        // are not displayed, so nilling them is invisible. Visible state
        // (items / currentScenario / currentSessionId / debriefActive / error)
        // is left intact through the async rebuild so the screen does NOT flash
        // to the welcome page mid-rebuild (macOS sheets dim-but-show the
        // underlying content, so an `items.removeAll()` here was visible as a
        // welcome-page flash before loadSession repopulated). The chosen route
        // below replaces the visible state atomically once the new app is ready.
        // Routes also clear pendingScenario themselves, so it's intentionally
        // NOT cleared here — that's what lets the topic-intake page survive.
        app = nil
        session = nil
        groupSession = nil
        await ensureApp()
        await refreshSessions()
        if let saved = savedScenario {
            await newConversation(scenario: saved)
        } else if savedPending != nil {
            // The topic-intake page was open. pendingScenario was NOT cleared
            // above, so `detailContent` keeps rendering TopicIntakeView across
            // the rebuild — its half-filled @State survives because the view
            // never left the hierarchy. Nothing to do here except NOT fall
            // through to newConversation/loadSession (which would jump the
            // user off the intake page).
        } else if hadConversation, let id = savedSessionId,
                  sessions.contains(where: { $0.id == id }) {
            await loadSession(id)
        } else {
            await newConversation()
        }
    }

    func refreshSessions() async {
        guard let a = app else { return }
        let list = await a.listConversations()
        sessions = list.sorted { $0.updatedAtMs > $1.updatedAtMs }
    }

    func newConversation() async {
        await newConversation(scenario: nil, topicValues: nil)
    }

    /// Convenience for starting without collected topic values (a scenario
    /// with no `topicFields`, or programmatic single-agent restart).
    func newConversation(scenario: Scenario?) async {
        await newConversation(scenario: scenario, topicValues: nil)
    }

    /// Confirm the inline topic-intake page: bake the collected values into the
    /// scenario and start the conversation.
    func confirmStartScenario(topicValues: [String: String]) async {
        let sc = pendingScenario
        pendingScenario = nil
        guard let sc else { return }
        await newConversation(scenario: sc, topicValues: topicValues)
    }

    /// Abort the inline topic-intake page; returns to whatever was current.
    func cancelPendingScenario() {
        pendingScenario = nil
    }

    /// Interrupt any in-flight stream on the CURRENT session/group-session and
    /// reset the running flags. Called when leaving a conversation (starting a
    /// new one or loading history) so a still-streaming previous turn doesn't
    /// keep bumping `streamTick` — which would otherwise auto-scroll/yank the
    /// newly shown conversation to its bottom on every flush (issue 4: switch
    /// to a history while another conversation is streaming → can't scroll it).
    private func interruptInFlight() async {
        await session?.interrupt()
        await groupSession?.interrupt()
        running = false
        activeSpeakerItem = nil
        activeSpeakerId = nil
    }

    /// Start a fresh conversation. When `scenario` is non-nil, a multi-agent
    /// group-chat session is created. The collected `topicValues` (keyed by
    /// field id) are folded into each member's system prompt as background
    /// and into the session title by `specView`. For scenarios with no opener,
    /// the values are sent as the first user message to kick off the first
    /// round (e.g. writing workshop → writer drafts).
    func newConversation(scenario: Scenario?, topicValues: [String: String]?) async {
        guard let a = app else { return }
        StreamLog.log("sess", "newConversation entry scenario=\(scenario?.id ?? "nil") running=\(running) curId=\(currentSessionId ?? "nil") items=\(items.count)")
        // Stop a still-streaming previous turn before swapping sessions — see
        // `interruptInFlight` (issue 4).
        await interruptInFlight()
        // Clear any pending scenario-intake page so navigating to a new chat
        // (or loading history) doesn't leave the detail stuck on the topic
        // form — `detailContent` renders the intake whenever this is non-nil.
        pendingScenario = nil
        currentScenario = scenario
        groupSession = nil
        activeSpeakerItem = nil
        activeSpeakerId = nil
        debriefActive = false

        if let scenario = scenario {
            let spec = scenario.specView(defaultKind: kind,
                                         defaultApiKey: apiKey,
                                         defaultBaseUrl: baseUrl,
                                         defaultModel: model,
                                         topicValues: topicValues)
            do {
                let gs = try a.createGroupSession(scenario: spec)
                groupSession = gs
                session = nil
                items.removeAll()
                error = nil
                currentSessionId = nil   // group-chat conversation id is engine-side
                running = true
                if scenario.openerAgentId != nil {
                    // Opener speaks first (it knows the topic from its system prompt).
                    let cb = StreamCallback(vm: self)
                    try await gs.start(callback: cb)
                    running = false
                    await refreshSessions()   // scenario session shows up, titled, immediately
                } else {
                    // No opener — kick off the first round with a user message
                    // built from the collected topic values (writing workshop).
                    let firstMsg = Self.firstUserMessage(for: scenario, topicValues: topicValues)
                    if !firstMsg.isEmpty {
                        await runGroupTask(firstMsg, addUserItem: true)
                    } else {
                        running = false
                    }
                }
            } catch {
                self.error = "场景启动失败: \(friendlyError(error))"
                currentScenario = nil
                groupSession = nil
                debriefActive = false
                running = false
            }
        } else {
            // Single-agent path.
            let s = a.createSession()
            StreamLog.log("sess", "createSession (single) id=\(s.sessionId()) running=\(running) prevId=\(currentSessionId ?? "nil")")
            session = s
            currentSessionId = s.sessionId()
            items.removeAll()
            error = nil
        }
    }

    /// Compose the first user message for a no-opener scenario from its topic
    /// fields + collected values (e.g. writing workshop → "秋天散文"). Empty
    /// when the user supplied nothing.
    private static func firstUserMessage(for scenario: Scenario, topicValues: [String: String]?) -> String {
        guard let fields = scenario.topicFields else { return "" }
        let vals = fields.compactMap { f -> String? in
            let v = (topicValues?[f.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            return v.isEmpty ? nil : v
        }
        return vals.joined(separator: " · ")
    }

    /// Trigger the scenario's debrief phase (e.g. "结束面试"): switch the turn
    /// policy to a scripted order containing only the debrief member, then send
    /// the summary prompt so that member produces a full-session summary.
    /// Subsequent user messages route only to the debrief member — the other
    /// members (e.g. the interviewer) no longer participate.
    func endScenarioDebrief() async {
        guard !running, let gs = groupSession, let debrief = currentScenario?.debrief,
              !debriefActive else { return }
        debriefActive = true
        await gs.setScriptedOrder(order: [debrief.debriefMemberId])
        // Send the summary prompt as a user turn; with the now-singleton order
        // only the debrief member responds. runGroupTask handles streaming/save.
        await runGroupTask(debrief.summaryPrompt, addUserItem: true)
    }

    /// Resume a saved single-agent session (group-chat resume not wired yet —
    /// group chats are created fresh per conversation in v1).
    func loadSession(_ id: String) async {
        guard let a = app else { return }
        StreamLog.log("sess", "loadSession id=\(id) running=\(running) curId=\(currentSessionId ?? "nil")")
        // Stop a still-streaming previous turn first (issue 4): otherwise its
        // `streamTick` bumps keep firing while the new history is on screen,
        // yanking the scroll to the bottom every flush.
        await interruptInFlight()
        // Same guard as newConversation: drop a pending scenario-intake page so
        // the loaded history actually shows instead of the topic form.
        pendingScenario = nil
        currentScenario = nil
        groupSession = nil
        debriefActive = false
        let s = await a.createSessionWithId(id: id)
        StreamLog.log("sess", "createSessionWithId (resume) id=\(id) resolvedId=\(s.sessionId())")
        let msgs = await s.messages()
        // Build the entry list off the main thread, then publish ONCE. Mutating
        // `items` per message (N @Published sends + N ForEach diff passes) is
        // what made switching to a long conversation stutter; a single
        // assignment coalesces to one objectWillChange + one render.
        var rebuilt: [ChatEntry] = []
        var lastTask: String? = nil
        rebuilt.reserveCapacity(msgs.count)
        for m in msgs {
            switch m.role {
            case "user":
                if !m.text.isEmpty {
                    rebuilt.append(.user(UserItem(text: m.text)))
                    lastTask = m.text
                }
            case "assistant":
                if !m.text.isEmpty {
                    let item = AssistantItem()
                    item.speakerId = m.speaker   // nil for single-agent
                    item.text = m.text
                    item.done = true
                    rebuilt.append(.assistant(item))
                }
            default: break // system / tool — not replayed
            }
        }
        // Publishing @Published state must land on the main thread; an async
        // non-isolated method resumes on a generic executor after the awaits
        // above, so hop back before touching UI state.
        await MainActor.run {
            session = s
            currentSessionId = s.sessionId()
            items = rebuilt
            lastUserTask = lastTask
            error = nil
            streamTick.value += 1
            // Force the detail to scroll to the most recent message (issue 7):
            // a freshly loaded history must show the bottom, not wherever the
            // previous session's scroll offset left the viewport.
            scrollRequest += 1
        }
    }

    func deleteSession(_ id: String) async {
        guard let a = app else { return }
        try? await a.deleteConversation(id: id)
        await refreshSessions()
        if id == currentSessionId { await newConversation() }
    }

    // MARK: Run

    /// Route an event to the active speaker's AssistantItem. When the speaker
    /// changes (a new member's turn), a fresh AssistantItem is created. For
    /// single-agent events (speaker nil), each runTask call's first event
    /// seeds the item.
    func handle(_ event: ChatEventView) {
        let speakerId = speaker(of: event)
        // New speaker → flush the previous item and start a new one.
        if let sid = speakerId, activeSpeakerItem?.speakerId != sid {
            let item = AssistantItem()
            item.speakerId = sid
            activeSpeakerItem = item
            items.append(.assistant(item))
            activeSpeakerId = sid
        } else if activeSpeakerItem == nil {
            // Single-agent (speaker nil) — create the turn's item on first event.
            let item = AssistantItem()
            activeSpeakerItem = item
            items.append(.assistant(item))
        }
        guard let turn = activeSpeakerItem else { return }

        switch event {
        case .thinking(let text, _):
            turn.thinkingActive = true; turn.thinking += text
        case .streamChunk(let text, _):
            // When the first text chunk arrives, thinking just ended. Force an
            // immediate (non-throttled) flush so the ThinkingCard switches from
            // "思考中…" to "已深度思考" right away — without this, the hot
            // throttle drops this tick's streamTick bump and the card stays on
            // "思考中…" (its plain-class field already flipped, but the row
            // wasn't re-rendered) until the next flush window.
            let flipped = turn.thinkingActive
            if flipped { turn.thinkingActive = false; turn.thinkingDone = true }
            turn.streaming = true; turn.text += text
            if flipped { lastStreamFlush = Date.distantPast }
        case .toolCall(let id, let name, let argsJson, _):
            // Dedup by callId: the engine emits on_tool_calls both mid-stream
            // (incremental ToolCallComplete) AND after the iteration completes
            // (AgentDecision::ToolCalls). Without dedup each call shows two rows.
            if turn.steps.contains(where: { $0.callId == id }) {
                break   // already shown — skip the duplicate emit
            }
            turn.steps.append(ToolStep(callId: id, name: name, args: argsJson))
        case .toolResult(let callId, _, let content, let success, _):
            if let idx = turn.steps.firstIndex(where: { $0.callId == callId }) {
                turn.steps[idx].result = content
                turn.steps[idx].ok = success
            } else if let idx = turn.steps.lastIndex(where: { $0.result == nil }) {
                turn.steps[idx].result = content
                turn.steps[idx].ok = success
            }
        case .directAnswer(let text, _):
            if !text.isEmpty { turn.text = text }
            if turn.thinkingActive { turn.thinkingActive = false; turn.thinkingDone = true }
        case .complete(let finalText, _):
            if !finalText.isEmpty { turn.text = finalText }
            if turn.thinkingActive { turn.thinkingActive = false; turn.thinkingDone = true }
            turn.streaming = false; turn.done = true
            // Lightweight token estimate for the top-bar usage indicator.
            lastTurnTokens = (finalText.count + turn.thinking.count) / 4
            if currentScenario == nil { running = false }
        case .error(let message, _):
            turn.error = message; turn.streaming = false; turn.done = true; running = false
        }
        // Bump the per-item version so `.equatable()` on `AssistantBubble`
        // re-renders THIS bubble's body on the next flush. Done (idle) bubbles
        // are never mutated, so their version stays put → their body is skipped
        // (just an Int compare) → the non-lazy list's per-flush cost is bounded
        // to the active streaming bubble instead of O(all bubbles).
        turn.version += 1
        bumpStreamTick(for: event)
    }

    /// Bump `streamTick` to trigger a UI refresh. Hot-path events
    /// (streamChunk/thinking) are coalesced to ~20 fps so a long stream does
    /// not flood the main queue with full-view re-renders; everything else
    /// (tool calls, direct answer, complete, error) flushes immediately, and
    /// `.complete`/`.error` reset the throttle window. The item's plain fields
    /// (text/thinking/steps) are already mutated by the caller, so a deferred
    /// flush still shows the latest content.
    private func bumpStreamTick(for event: ChatEventView) {
        let hot: Bool
        switch event {
        case .streamChunk, .thinking: hot = true
        default: hot = false
        }
        if hot {
            let now = Date()
            if now.timeIntervalSince(lastStreamFlush) < Self.streamFlushInterval {
                return   // within the throttle window — skip this refresh
            }
            lastStreamFlush = now
        } else {
            lastStreamFlush = Date.distantPast   // reset window; next hot event flushes
        }
        streamTick.value += 1
    }

    /// Extract the speaker id from any event variant (nil = single-agent).
    private func speaker(of event: ChatEventView) -> String? {
        switch event {
        case .streamChunk(_, let s), .thinking(_, let s),
             .toolCall(_, _, _, let s), .toolResult(_, _, _, _, let s),
             .directAnswer(_, let s), .complete(_, let s), .error(_, let s):
            return s
        }
    }

    func runTask(_ task: String, addUserItem: Bool = true) async {
        lastUserTask = task
        if groupSession != nil {
            await runGroupTask(task, addUserItem: addUserItem)
            return
        }
        guard let s = session else { self.error = "session not built"; return }
        StreamLog.log("sess", "runTask entry id=\(s.sessionId()) running=\(running) items=\(items.count) len=\(task.count)")
        if addUserItem { items.append(.user(UserItem(text: task))) }
        let turn = AssistantItem()
        activeSpeakerItem = turn
        items.append(.assistant(turn))
        running = true
        error = nil

        // Persist immediately so the new chat shows in the sidebar mid-turn.
        StreamLog.log("sess", "save pre-run id=\(s.sessionId())")
        try? await s.save()
        await refreshSessions()

        let callback = StreamCallback(vm: self)
        StreamLog.log("run", "single-agent runTask start len=\(task.count)")
        do {
            try await s.runTask(task: task, callback: callback)
            turn.streaming = false; turn.done = true; running = false
            StreamLog.log("run", "single-agent runTask end ok")
        } catch {
            turn.error = friendlyError(error)
            turn.streaming = false; turn.done = true; running = false
            StreamLog.log("run", "single-agent runTask err=\(friendlyError(error))")
        }
        await refreshSessions()
    }

    /// Multi-agent run: appends the user item, runs the round (each member's
    /// events route to its own item via `handle`), stops at the user's turn.
    private func runGroupTask(_ task: String, addUserItem: Bool) async {
        guard let gs = groupSession else { return }
        StreamLog.log("sess", "runGroupTask entry running=\(running) items=\(items.count) len=\(task.count)")
        if addUserItem { items.append(.user(UserItem(text: task))) }
        activeSpeakerItem = nil     // a new round starts; first event seeds item
        activeSpeakerId = nil
        running = true
        error = nil
        let callback = StreamCallback(vm: self)
        StreamLog.log("run", "group runTask start len=\(task.count)")
        do {
            try await gs.runTask(userInput: task, callback: callback)
            running = false
            StreamLog.log("run", "group runTask end ok")
        } catch {
            // Attach the error to the active speaker's item (or a fresh one).
            if activeSpeakerItem == nil {
                let item = AssistantItem()
                activeSpeakerItem = item
                items.append(.assistant(item))
            }
            activeSpeakerItem?.error = friendlyError(error)
            activeSpeakerItem?.streaming = false
            activeSpeakerItem?.done = true
            running = false
        }
        try? await gs.save()
        await refreshSessions()
    }

    func retryLast() async {
        guard let task = lastUserTask, !running else { return }
        if case .assistant(let last) = items.last, last.error != nil {
            items.removeLast()
            await runTask(task, addUserItem: false)
        } else {
            await runTask(task, addUserItem: true)
        }
    }

    /// Edit a user message in-place: replace its text, drop everything after
    /// it, and re-run from that point (a pragmatic edit-and-branch — true
    /// checkpoint branching lands with the persistence layer's help later).
    func editAndResend(_ item: UserItem, newText: String) async {
        guard !running else { return }
        let trimmed = newText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        // Find the item, truncate everything after it (incl. its old reply).
        if let idx = items.firstIndex(where: {
            if case .user(let u) = $0 { return u.id == item.id }
            return false
        }) {
            items[idx] = .user(UserItem(text: trimmed))
            // Keep items up to and including the edited user message.
            let kept = Array(items.prefix(idx + 1))
            items = kept
            lastUserTask = trimmed
            // Re-run without re-adding the user item (already there).
            await runTask(trimmed, addUserItem: false)
        }
    }

    func stop() async {
        if let gs = groupSession { await gs.interrupt() }
        await session?.interrupt()
    }
}
