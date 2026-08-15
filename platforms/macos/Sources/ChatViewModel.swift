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

// MARK: - Sidecar transport (Phase D)

/// Which engine backend the app talks to. `.ffi` (default) = the in-process
/// c_facade staticlib (best UX — no process/socket). `.sidecar` = spawn
/// `oneai app-server` and drive it over JSON-RPC (Codex model: the frontend
/// that can spawn owns the spawn). Selected by UserDefaults
/// `oneai_engine_transport`. See Phase D in the plan — Slice 1 wires the
/// single-agent sidecar path; group/scenario via sidecar is Slice 2.
enum ChatTransport: String { case ffi, sidecar }

/// A pending tool-approval request the sidecar engine surfaced (it uses
/// `BusInteractionGate`, which emits `EngineYield::ApprovalRequest`; the FFI
/// path uses `NoopInteractionGate` and never prompts). The UI renders an
/// Allow/Deny overlay; `respondApproval` sends `approval/respond`.
struct PendingApproval: Identifiable {
    let id = UUID()
    let requestId: String
    let toolName: String
    let justification: String
}

// MARK: - Streaming callback (foreign-implemented ChatEventCallback)

final class StreamCallback: ChatEventCallback, @unchecked Sendable {
    weak var vm: ChatViewModel?
    /// Key of the session this callback's run belongs to (issue #13). Compared
    /// against `vm.currentStreamKey()` (the visible session's key) on every
    /// event to route it: visible → render into `items` + bump `streamTick`;
    /// single-agent background → accumulate into `backgroundTurns[key]` (no
    /// re-render, no freeze) so switching back shows the full streamed output;
    /// group background → drop (group rounds are short, group resume isn't
    /// wired). Group keys carry a `"group:"` prefix; single-agent keys are bare
    /// session ids.
    private let sessionKey: String
    init(vm: ChatViewModel, sessionKey: String) {
        self.vm = vm
        self.sessionKey = sessionKey
    }

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
        guard let vm = vm else { return }
        let visible = (vm.currentStreamKey() == sessionKey)
        // Visible session: full hot/non-hot flush machinery → `handle` (renders
        // into `items` + bumps `streamTick`). Background single-agent: cheap
        // per-event dispatch into the buffered turn (no `streamTick`, no
        // re-render) so the run keeps accumulating while the user is elsewhere
        // and switch-back shows the full output (issue #13). Background group:
        // drop (group is interrupted on switch-away; rounds are short).
        if visible {
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
                let key = sessionKey
                DispatchQueue.main.async { [weak self] in
                    guard let self, let vm = self.vm else { return }
                    for e in pending { vm.handle(e, key: key) }
                    vm.handle(event, key: key)
                }
            }
        } else if !sessionKey.hasPrefix("group:") {
            // Background single-agent: route to the buffered turn (no streamTick).
            let key = sessionKey
            DispatchQueue.main.async { [weak self] in
                guard let vm = self?.vm else { return }
                vm.handle(event, key: key)
            }
        }
        // else group background → drop.
    }

    private func flush() {
        lock.lock()
        flushScheduled = false
        let pending = pendingHot
        pendingHot.removeAll()
        lock.unlock()
        guard let vm = vm, !pending.isEmpty else { return }
        let key = sessionKey
        let t0 = Date()
        StreamLog.log("main", "flush start pending=\(pending.count)")
        // handle() throttles streamTick internally — the first hot event in the
        // batch bumps (≥flushInterval since the last flush), the rest skip, so
        // the whole batch produces exactly one re-render. It re-checks
        // visibility, so a switch that landed mid-flush routes the leftover
        // events to the buffered turn instead of contaminating the new view.
        for e in pending { vm.handle(e, key: key) }
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
        case .tokenUsage: return "tokenUsage"
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
    /// Per-turn token count for the top bar. Accumulated from real API usage
    /// (`prompt + completion` across the turn's inferences via `tokenUsage`);
    /// falls back to a char-based estimate (`finalText`/4) only when the
    /// provider reports no usage at all. Reset to 0 at the start of each turn.
    @Published var lastTurnTokens: Int = 0
    /// Prompt-cache hit ratio for the most recent inference (0–100), reported
    /// by the agent loop via the `tokenUsage` event. Set whenever the provider
    /// reports real usage: the ratio for caching providers (Anthropic), or `0`
    /// for providers that report usage but no prompt caching (OpenAI) so the
    /// badge reads "cache 0%" (caching visibly off) rather than vanishing
    /// (which reads as broken). Stays `nil` only when the provider reports no
    /// usage at all (e.g. GLM streaming) — no data, no badge. Reset per turn.
    @Published var lastCacheHitPct: Double? = nil
    /// Bumped whenever a session is (re)loaded so the detail view can force a
    /// scroll-to-bottom (issue 7). `onChange(of: items.count)` alone is
    /// unreliable here: a loaded session with the same message count as the
    /// previous one never fires, and `stickToBottom` may be false from a prior
    /// scroll-up — so the history landed mid-conversation instead of at the
    /// most recent message. This dedicated counter fires on every load.
    @Published var scrollRequest: Int = 0

    // ── Paginated history (lazy "load earlier messages") ───────────────
    // `messages()` returns the FULL merged transcript; for long sessions that
    // deserializes every archived snapshot up front (memory + latency). Instead
    // load only the recent page on session open (`transcriptRecent`), then
    // prepend older pages on demand via `loadOlder()` (`transcriptOlder`).
    // `items` holds only the loaded window — bounded memory regardless of total
    // transcript length.
    /// True when older messages exist below the currently-loaded window. Drives
    /// the "加载更早消息 ↑" button at the top of the chat.
    @Published var hasOlder: Bool = false
    /// True while an older page is being fetched (button → spinner).
    @Published var olderLoading: Bool = false
    /// Opaque cursor for the next-older page (a rank string from Rust). `nil`
    /// when at top or no session loaded.
    private var olderCursor: String? = nil
    /// Page size for transcript fetches.
    //
    // Capped at 12 (was 50) for the macOS chat list: the SwiftUI non-lazy
    // `VStack` in a `ScrollView` realizes + measures every bubble eagerly to
    // size the document → O(n²) AppKit layout, so a 50-message page froze the
    // session switch for several seconds (issue #11). 12 keeps the layout
    // cost well below the freeze threshold while still showing a usable
    // recent window; older messages load on demand via `loadOlder()` (same
    // page size). Safe minimal patch — touches only this number, no
    // scroll/streaming machinery.
    private let transcriptPageSize: UInt32 = 12
    /// Set by `loadOlder` to the first (oldest) id of the just-prepended page;
    /// `Views` observes it to scroll that id to the viewport top so the user
    /// sees the newly loaded older messages.
    @Published var olderJumpId: UUID? = nil

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

    // ── Sidecar transport state (Phase D) ─────────────────────────────────
    // Built only when `transport == .sidecar`. The `OneAiRpcClient` is the
    // single channel for turn/run, approval/respond, and the `event`
    // notification stream (stream_chunk / thinking / tool_calls /
    // turn_complete / approval_request / …). `engineMgr` owns the spawned
    // `oneai app-server` child (locate binary → spawn → wait for socket →
    // hand client here → restart on crash). FFI path leaves all three nil.
    private var engineMgr: EngineProcessManager? = nil
    private var rpcClient: OneAiRpcClient? = nil
    /// turn_id of the in-flight sidecar turn (from `turn/run`'s result, sent
    /// at TurnStart). Used by `stop()` to cancel.
    private var sidecarTurnId: String? = nil
    /// Per-conversation stream key for the sidecar single-agent path —
    /// mirrors the FFI path's `setActiveStreamKey(sessionId())` so the
    /// existing visibility machinery (background buffering in particular)
    /// can route sidecar events too, even though Slice 1 is always-visible.
    private var sidecarStreamKey: String = ""
    /// Resumes `ensureApp`'s `withCheckedThrowingContinuation` once the
    /// EngineProcessManager delegate reports the client is connected (or
    /// failed to start). Set in `ensureApp`, resumed in the delegate.
    private var sidecarStartCont: CheckedContinuation<Void, Error>?
    /// A pending tool-approval request from the sidecar engine (the FFI
    /// path's NoopInteractionGate never produces these). Non-nil → ChatScreen
    /// renders an Allow/Deny overlay; `respondApproval` resolves it.
    @Published var pendingApproval: PendingApproval? = nil

    /// Which engine backend. Read from UserDefaults `oneai_engine_transport`
    /// (default `.ffi`).
    var transport: ChatTransport {
        let v = prefs.string(forKey: "oneai_engine_transport") ?? "ffi"
        return ChatTransport(rawValue: v) ?? .ffi
    }
    /// The AssistantItem currently accumulating events for the active speaker.
    private var activeSpeakerItem: AssistantItem? = nil
    /// Throttle: last time `streamTick` was bumped for a hot-path event
    /// (streamChunk/thinking). Bumping per-token re-renders the whole chat
    /// (incl. full markdown re-parse of the growing bubble) on every token —
    /// for long streams the main queue backs up faster than it drains and the
    /// app beachballs. Coalesce to ~20 fps; `.complete`/`.error` always flush.
    private var lastStreamFlush = Date.distantPast
    private static let streamFlushInterval: TimeInterval = 0.05

    // ── Stream routing for session-switch-during-streaming (issue #13) ──
    // When the user switches away from a still-streaming single-agent
    // conversation, the in-flight run is NOT interrupted — it continues in the
    // background and its events route to `backgroundTurns[key]` (a per-session
    // `AssistantItem`) instead of the visible `items`. No `streamTick` bump →
    // no main-queue re-render flood → no freeze. On switch-back, the buffered
    // turn is restored into `items` (the live item, with everything streamed
    // while away); the live session object is reused so future sends see the
    // full conversation. Group rounds are short and group resume isn't wired,
    // so group streams are dropped on switch-away (not buffered).
    //
    // `activeStreamKey` (NSLock-guarded: written from `interruptInFlight`/
    // `loadSession`/`newConversation` on the cooperative pool, read from the
    // tokio worker thread in `StreamCallback.onEvent`) is the key of the
    // session whose events should render visibly. A `StreamCallback` captures
    // its `sessionKey` at creation; `onEvent` routes by comparing the two.
    private let streamKeyLock = NSLock()
    private var activeStreamKey: String = ""
    func currentStreamKey() -> String {
        streamKeyLock.lock(); defer { streamKeyLock.unlock() }
        return activeStreamKey
    }
    func setActiveStreamKey(_ key: String) {
        streamKeyLock.lock(); defer { streamKeyLock.unlock() }
        activeStreamKey = key
    }
    /// Buffered in-flight turn for a single-agent session the user switched
    /// away from, keyed by session id. Drained on switch-back (`loadSession`).
    private var backgroundTurns: [String: AssistantItem] = [:]
    /// The live `OneAiSession` object for a backgrounded single-agent run,
    /// reused on switch-back so future sends carry the full conversation
    /// (a fresh `createSessionWithId` would read stale SQLite mid-run).
    private var backgroundSessions: [String: OneAiSession] = [:]
    /// Key for the active group-chat stream (group sessions have no
    /// `sessionId()`; a fresh UUID per group conversation disambiguates).
    private var groupStreamKey: String = ""

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

    /// Map the user's UserDefaults provider config to the env vars the
    /// spawned `oneai app-server` child reads (`ONEAI_API_KEY` /
    /// `ONEAI_BASE_URL` / `ONEAI_MODEL` — see `examples/cli/src/config.rs`).
    /// The provider *kind* is inferred from the base url on the Rust side
    /// (ProviderFactory), same rule as `Self.inferKind`, so no kind env var.
    private func providerEnvMap() -> [String: String] {
        var env: [String: String] = [:]
        if !apiKey.isEmpty { env["ONEAI_API_KEY"] = apiKey }
        if !baseUrl.isEmpty { env["ONEAI_BASE_URL"] = baseUrl }
        if !model.isEmpty { env["ONEAI_MODEL"] = model }
        return env
    }

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
    private func embeddingConfigView() -> EmbeddingConfigView? {        let provider = embProvider.isEmpty ? "auto" : embProvider
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
        // Sidecar transport: don't build the in-process FFI app — spawn the
        // `oneai app-server` sidecar and connect a JSON-RPC client instead.
        // Slice 1 wires single-agent turns; group/scenario stay FFI (Slice 2).
        if transport == .sidecar {
            guard rpcClient == nil, engineMgr == nil else { return }
            StreamLog.start()
            initOneaiLog(logDir: appSupportDir)
            if !streamHeartbeatStarted {
                streamHeartbeatStarted = true
                Self.scheduleHeartbeat()
            }
            let mgr = EngineProcessManager()
            mgr.extraEnv = providerEnvMap()
            // Share the SAME SQLite DB as the in-process FFI engine
            // (~/Library/Application Support/oneai.db — `dbPath`). The
            // sidecar reads ONEAI_DB_PATH and calls sqlite_persistence_at,
            // so a session saved under FFI appears in the sidecar's sidebar
            // and vice versa — switching transports never loses history.
            // SQLite WAL + busy_timeout make the cross-process sharing safe;
            // only one engine is ever active (the FFI app isn't built in
            // sidecar mode).
            mgr.extraEnv["ONEAI_DB_PATH"] = dbPath
            // Surface the engine's tracing spans in the sidecar's stderr log
            // (~/.oneai/app-server-sidecar.log) — the app-server inits a
            // stderr subscriber that reads RUST_LOG (default info). Without
            // this a stuck turn is invisible (no engine logs).
            mgr.extraEnv["RUST_LOG"] = "info"
            mgr.delegate = self
            engineMgr = mgr
            do {
                try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
                    self.sidecarStartCont = cont
                    mgr.start()
                }
            } catch {
                self.error = "引擎 sidecar 启动失败: \(friendlyError(error))"
            }
            return
        }
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
        // Sidecar: tear down the child + client so ensureApp re-spawns with
        // the (possibly changed) provider env. Settings save → rebuildApp →
        // fresh sidecar carrying the new ONEAI_API_KEY / base url / model.
        if transport == .sidecar {
            engineMgr?.stop()
            engineMgr = nil
            rpcClient = nil
            sidecarTurnId = nil
            pendingApproval = nil
        }
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
        // Sidecar: list over JSON-RPC `session/list` (synchronous CRUD — the
        // app-server returns the same epoch-millis shape the FFI
        // `listConversations` yields, so the sidebar renders one list
        // regardless of transport). The rpcClient is ready only after
        // `didStart`; before that, bail (the cold-start `.task` calls this
        // after ensureApp, and rebuildApp calls it after re-spawn).
        if transport == .sidecar {
            guard let rpc = rpcClient else { return }
            do {
                let res = try await rpc.call("session/list", params: [String: String]())
                let arr = (res["sessions"] as? [[String: Any]]) ?? []
                let sorted = arr.compactMap { Self.sessionInfoView(from: $0) }
                    .sorted { $0.updatedAtMs > $1.updatedAtMs }
                // refreshSessions may be called from a non-main context
                // (the turn_complete handler spawns a Task), so marshal the
                // @Published write to the main actor.
                await MainActor.run { self.sessions = sorted }
            } catch {
                StreamLog.log("sess", "sidecar session/list err=\(friendlyError(error))")
            }
            return
        }
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

    /// Tear down the visible conversation before swapping to another. For a
    /// still-streaming single-agent turn, the run is NOT interrupted — it keeps
    /// going in the background, its events routing to `backgroundTurns[sid]`
    /// (no `streamTick` → no re-render flood → no freeze), and switching back
    /// restores the full streamed output (issue #13). Group rounds are short
    /// and group resume isn't wired, so group is interrupted + dropped.
    private func interruptInFlight() async {
        // Sidecar: cancel the in-flight turn/round (if any) via turn/cancel so
        // its events don't contaminate the next conversation — the sidecar is
        // always-visible (no background buffering). Group rounds don't register
        // a bus cancel token (the round runs to completion), but turn/cancel is
        // harmless when nothing's registered; for single-agent it fires the
        // token registered at TurnStart. Send whenever `running` (the ack nature
        // of group/run + turn/run means turn_id isn't a reliable proxy).
        if transport == .sidecar {
            if running, let rpc = rpcClient {
                _ = try? await rpc.call("turn/cancel", params: [String: String]())
            }
            if let t = activeSpeakerItem { t.streaming = false; t.done = true }
            sidecarTurnId = nil
            setActiveStreamKey("")
            running = false
            activeSpeakerItem = nil
            activeSpeakerId = nil
            return
        }
        if currentScenario == nil, let sid = currentSessionId,
           running, let turn = activeSpeakerItem, let s = session {
            // Preserve the live turn + session object FIRST (while the visible
            // key still points here) so any event firing in the switch window
            // finds the buffered turn immediately instead of creating a thrown-
            // away one. The run (still going on `s`) keeps firing events into
            // `backgroundTurns[sid]` once the visible key moves away.
            backgroundTurns[sid] = turn
            backgroundSessions[sid] = s
        } else {
            // Group (or non-streaming): stop the group round if one is in flight.
            await groupSession?.interrupt()
        }
        // Visible key no longer matches any in-flight run → single-agent runs
        // route to their buffered turn, group runs drop.
        setActiveStreamKey("")
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
        // Sidecar transport: turns go over JSON-RPC. Single-agent = turn/run;
        // group chat = group/start + group/open|group/run. (Phase D Slice 2.)
        if transport == .sidecar {
            await interruptInFlight()
            pendingScenario = nil
            currentScenario = scenario
            groupSession = nil
            activeSpeakerItem = nil
            activeSpeakerId = nil
            hasOlder = false
            olderCursor = nil
            olderJumpId = nil
            debriefActive = false
            guard let scenario = scenario else {
                // Single-agent: create a fresh engine session so the upcoming
                // turn/run saves under a NEW conversation row (the sidecar's
                // CreateSession directive swaps the pump's runtime session;
                // turn/run then runs on it + run_agent auto-saves it). The
                // returned id binds currentSessionId so the sidebar can mark
                // this conversation current and a later turn_complete refresh
                // surfaces it. No scenario ⇒ engine assigns the id.
                items.removeAll()
                error = nil
                currentSessionId = nil
                running = false
                if let rpc = rpcClient,
                   let res = try? await rpc.call("session/create", params: [String: String]()),
                   let id = res["id"] as? String {
                    currentSessionId = id
                    StreamLog.log("sess", "sidecar session/create id=\(id)")
                }
                return
            }
            // Group chat via sidecar: group/start builds the engine-side
            // GroupChatSession; group/open (opener) or group/run (first user
            // msg) drives the first round. Streaming + the round-end
            // turn_complete arrive as `event` notifications.
            guard let rpc = rpcClient else { self.error = "sidecar 引擎未就绪"; return }
            items.removeAll()
            error = nil
            currentSessionId = nil   // group-chat conversation id is engine-side
            running = true
            groupStreamKey = "group:" + UUID().uuidString
            sidecarStreamKey = groupStreamKey
            setActiveStreamKey(groupStreamKey)
            sidecarTurnId = nil
            let spec = buildGroupScenarioJSON(scenario: scenario, topicValues: topicValues)
            do {
                _ = try await rpc.call("group/start", params: ["scenario": spec])
                StreamLog.log("sess", "sidecar group/start scenario=\(scenario.id)")
                if scenario.openerAgentId != nil {
                    _ = try await rpc.call("group/open", params: [String: String]())
                    StreamLog.log("run", "sidecar group/open (opener)")
                } else {
                    // No opener — kick off the first round with a user message
                    // built from the collected topic values (writing workshop).
                    let firstMsg = Self.firstUserMessage(for: scenario, topicValues: topicValues)
                    if !firstMsg.isEmpty {
                        await runGroupTaskSidecar(firstMsg, addUserItem: true)
                    } else {
                        running = false
                    }
                }
            } catch {
                self.error = String(format: NSLocalizedString("场景启动失败: %@", comment: ""), friendlyError(error))
                currentScenario = nil
                running = false
            }
            return
        }
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
        // A brand-new chat has no older pages — drop any cursor carried over
        // from the previously-loaded session so the "load earlier" button
        // doesn't dangle.
        hasOlder = false
        olderCursor = nil
        olderJumpId = nil
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
                // Fresh per-conversation key for this group stream (group
                // sessions have no `sessionId()`); visible key now matches the
                // callback created below so its events render (issue #13).
                groupStreamKey = "group:" + UUID().uuidString
                setActiveStreamKey(groupStreamKey)
                if scenario.openerAgentId != nil {
                    // Opener speaks first (it knows the topic from its system prompt).
                    let cb = StreamCallback(vm: self, sessionKey: groupStreamKey)
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
                self.error = String(format: NSLocalizedString("场景启动失败: %@", comment: ""), friendlyError(error))
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
            // Visible key matches this session so its (forthcoming) stream
            // renders; a still-streaming previous session keeps running in the
            // background, routed to its buffered turn (issue #13).
            setActiveStreamKey(s.sessionId())
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

    /// Build the JSON-RPC `BusGroupScenario` payload for `group/start` from a
    /// `Scenario` + collected topic values. Mirrors `Scenario.specView` /
    /// `Agent.specView` (the FFI path's `ScenarioSpecView`): bakes the visible
    /// topic fields into each member's `system_prompt` as `【场景背景】…`, folds
    /// every non-blank value into the title suffix, and inherits the app's
    /// provider config where an agent leaves kind/apiKey/baseUrl/model nil.
    ///
    /// Emits snake_case keys matching `crates/oneai-bus::BusGroupScenario`
    /// (the `app-server` adapter parses `group/start`'s `scenario` param as
    /// that DTO) — NOT the camelCase `Scenario` Codable (which mirrors the rich
    /// `BusScenario` editor unit the macOS sidebar reads from disk directly).
    private func buildGroupScenarioJSON(scenario: Scenario, topicValues: [String: String]?) -> [String: Any] {
        let fields = scenario.topicFields ?? []
        let pairs: [(field: TopicField, value: String)] = fields.compactMap { f in
            let v = (topicValues?[f.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            return v.isEmpty ? nil : (f, v)
        }
        let titleParts = pairs.map { $0.value }
        let title = titleParts.isEmpty ? scenario.name : "\(scenario.name)·" + titleParts.joined(separator: "·")

        let members = scenario.agents.map { agent -> [String: Any] in
            // Background for THIS member: only fields it's allowed to see.
            let visible = pairs.filter { p in
                guard let allowed = p.field.visibleTo else { return true }
                return allowed.contains(agent.id)
            }
            let lines = visible.map { "\($0.field.label): \($0.value)" }
            let background = lines.isEmpty ? "" : "【场景背景】\n" + lines.joined(separator: "\n")
            let prompt = background.isEmpty ? agent.systemPrompt : "\(agent.systemPrompt)\n\n\(background)"
            var m: [String: Any] = [
                "id": agent.id,
                "name": agent.name,
                "system_prompt": prompt,
                "kind": agent.kind ?? kind,
                "model": agent.model ?? model,
                "color": agent.color,
                "avatar": agent.avatar,
            ]
            // nil ⇒ inherit the app's provider; only forward an explicit override
            // or the app setting when the agent leaves it nil.
            let key = agent.apiKey ?? (apiKey.isEmpty ? nil : apiKey)
            let url = agent.baseUrl ?? (baseUrl.isEmpty ? nil : baseUrl)
            if let key = key { m["api_key"] = key }
            if let url = url { m["base_url"] = url }
            return m
        }

        var spec: [String: Any] = [
            "members": members,
            "turn_policy": scenario.turnPolicy.specValue,
            "title": title,
            "locale": AppLocale.current.rawValue,
        ]
        if let order = scenario.scriptOrder { spec["script_order"] = order }
        if let mod = scenario.moderatorId { spec["moderator_id"] = mod }
        if let op = scenario.openerAgentId { spec["opener_agent_id"] = op }
        if let line = scenario.openerLine { spec["opener_line"] = line }
        if let rl = scenario.reviewLoop {
            spec["review_loop"] = [
                "reviewer_id": rl.reviewerId,
                "approve_marker": rl.approveMarker,
                "max_rounds": rl.maxRounds,
            ] as [String: Any]
        }
        return spec
    }

    /// Trigger the scenario's debrief phase (e.g. "结束面试"): switch the turn
    /// policy to a scripted order containing only the debrief member, then send
    /// the summary prompt so that member produces a full-session summary.
    /// Subsequent user messages route only to the debrief member — the other
    /// members (e.g. the interviewer) no longer participate.
    func endScenarioDebrief() async {
        guard !running, let debrief = currentScenario?.debrief, !debriefActive else { return }
        debriefActive = true
        if transport == .sidecar {
            // group/set_order narrows the turn policy to the debrief member;
            // the subsequent group/run sends the summary prompt, and only that
            // member responds. runGroupTaskSidecar handles streaming.
            if let rpc = rpcClient {
                _ = try? await rpc.call("group/set_order", params: ["order": [debrief.debriefMemberId]])
            }
            await runGroupTaskSidecar(debrief.summaryPrompt, addUserItem: true)
            return
        }
        guard let gs = groupSession else { return }
        await gs.setScriptedOrder(order: [debrief.debriefMemberId])
        // Send the summary prompt as a user turn; with the now-singleton order
        // only the debrief member responds. runGroupTask handles streaming/save.
        await runGroupTask(debrief.summaryPrompt, addUserItem: true)
    }

    /// Resume a saved single-agent session (group-chat resume not wired yet —
    /// group chats are created fresh per conversation in v1).
    func loadSession(_ id: String) async {
        // Sidecar: load over JSON-RPC `session/load` (bus-driven — the pump
        // swaps the runtime session to the loaded conversation and emits
        // SessionLoaded{ id, messages }). Decode the raw Message JSON
        // (role / content[].text flatten / metadata["speaker"]) into the
        // flat MessageView shape `rebuildEntries` consumes, so a reloaded
        // sidecar history shows the same bubble count + fold the user saw
        // live. Full message list (no pagination) — the simple-path closed
        // loop; long conversations are windowed client-side on render.
        if transport == .sidecar {
            guard let rpc = rpcClient else { return }
            StreamLog.log("sess", "sidecar loadSession id=\(id)")
            await interruptInFlight()
            pendingScenario = nil
            currentScenario = nil
            groupSession = nil
            debriefActive = false
            do {
                let res = try await rpc.call("session/load", params: ["id": id])
                let resolvedId = (res["id"] as? String) ?? id
                let msgs = Self.messageViews(fromSidecar: res["messages"])
                var lastTask: String? = nil
                let rebuilt = Self.rebuildEntries(from: msgs, lastTask: &lastTask)
                await MainActor.run {
                    self.currentSessionId = resolvedId
                    self.items = rebuilt
                    self.lastUserTask = lastTask
                    self.error = nil
                    self.hasOlder = false
                    self.olderCursor = nil
                    self.olderJumpId = nil
                    self.activeSpeakerItem = nil
                    self.running = false
                    self.streamTick.value += 1
                    self.scrollRequest += 1
                }
            } catch {
                self.error = "加载会话失败: \(friendlyError(error))"
            }
            return
        }
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

        // Restore a background (still-streaming) turn if this session was
        // switched away from mid-stream (issue #13): its events kept
        // accumulating into `backgroundTurns[id]` on the live object. Reuse the
        // live `OneAiSession` so future sends carry the full conversation; the
        // in-flight turn isn't in SQLite yet (auto-save is post-run), so the DB
        // page alone would miss it. If the turn already completed (`done`),
        // `run_agent` auto-saved the full conversation → trust the DB page and
        // discard the buffer.
        let bgTurn = backgroundTurns.removeValue(forKey: id)
        let bgSession = backgroundSessions.removeValue(forKey: id)
        let resumeBg = (bgTurn != nil && !(bgTurn?.done ?? true))

        let s: OneAiSession
        let page: TranscriptPage
        if resumeBg, let live = bgSession {
            // Reuse the live object (still running; its `inner` is locked for
            // the run, so read older history from a throwaway DB-backed object).
            s = live
            let tmp = await a.createSessionWithId(id: id)
            page = await tmp.transcriptRecent(limit: transcriptPageSize)
        } else {
            s = await a.createSessionWithId(id: id)
            StreamLog.log("sess", "createSessionWithId (resume) id=\(id) resolvedId=\(s.sessionId())")
            // Paginated load: only the most recent page is fetched + rebuilt into
            // `items` (full `messages()` would deserialize every archived snapshot
            // up front). Older pages are prepended on demand via `loadOlder`.
            page = await s.transcriptRecent(limit: transcriptPageSize)
        }
        var lastTask: String? = nil
        var rebuilt = Self.rebuildEntries(from: page.messages, lastTask: &lastTask)
        if resumeBg, let turn = bgTurn {
            // The DB page predates the in-flight turn (only the user message
            // was pre-saved); append the live turn so switch-back shows the
            // streamed-so-far output, then keep routing subsequent events to it.
            rebuilt.append(.assistant(turn))
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
            hasOlder = (page.olderCursor != nil)
            olderCursor = page.olderCursor
            olderJumpId = nil
            // Visible key now matches the resumed run's callback → its ongoing
            // events render into the restored turn (issue #13).
            setActiveStreamKey(s.sessionId())
            if resumeBg, let turn = bgTurn {
                activeSpeakerItem = turn
                running = true     // the background run is still in flight
            } else {
                activeSpeakerItem = nil
                running = false
            }
            streamTick.value += 1
            // Force the detail to scroll to the most recent message (issue 7):
            // a freshly loaded history must show the bottom, not wherever the
            // previous session's scroll offset left the viewport.
            scrollRequest += 1
        }
    }

    /// Build a `[ChatEntry]` from a page of `MessageView`s, mirroring the
    /// live-streaming fold: a single user turn often persists several
    /// assistant messages (tool-call preludes + final answer), but live
    /// `handle(_:)` folds the whole run into ONE `AssistantItem` (one turn
    /// accumulates every `streamChunk` / `toolCall` / `toolResult` /
    /// `directAnswer`). Replay the same fold here so a reloaded session shows
    /// the same bubble count the user saw live — and the same number the
    /// sidebar reports (issue #17: 一轮中的多次输出不应单独成泡). Consecutive
    /// assistant messages with the same speaker merge into one bubble (text
    /// concatenated with a newline); a speaker change or a user message
    /// starts a new bubble. `tool` / `system` messages between assistants
    /// don't break a turn. `lastTask` is bumped to the last user message.
    private static func rebuildEntries(
        from msgs: [MessageView],
        lastTask: inout String?
    ) -> [ChatEntry] {
        var rebuilt: [ChatEntry] = []
        rebuilt.reserveCapacity(msgs.count)
        var pending: AssistantItem? = nil
        // Flush the in-progress assistant bubble (if any) before a user
        // message, a speaker change, or the end of the list.
        func flushPending() {
            if let item = pending {
                rebuilt.append(.assistant(item))
                pending = nil
            }
        }
        for m in msgs {
            switch m.role {
            case "user":
                flushPending()
                if !m.text.isEmpty {
                    rebuilt.append(.user(UserItem(text: m.text)))
                    lastTask = m.text
                }
            case "assistant":
                if m.text.isEmpty {
                    // Tool-call-only assistant (no prelude) — part of the
                    // current turn, renders no bubble of its own; don't break it.
                    break
                }
                if let item = pending, item.speakerId == m.speaker {
                    // Same turn, same speaker — accumulate the text.
                    if !item.text.isEmpty { item.text += "\n" }
                    item.text += m.text
                } else {
                    // New turn or speaker change — flush the previous bubble
                    // and start a fresh one.
                    flushPending()
                    let item = AssistantItem()
                    item.speakerId = m.speaker   // nil for single-agent
                    item.text = m.text
                    item.done = true
                    pending = item
                }
            default: break // system / tool — not replayed, don't break the turn
            }
        }
        flushPending()
        return rebuilt
    }

    /// Decode one `session/list` entry (the epoch-millis shape the app-server
    /// emits — identical to the FFI `SessionInfoView`) into the FFI struct so
    /// `SessionRow` renders sidecar and FFI rows through the same path. nil
    /// when the row is missing the required `id` (a malformed entry is
    /// dropped rather than crashing the whole list).
    private static func sessionInfoView(from dict: [String: Any]) -> SessionInfoView? {
        guard let id = dict["id"] as? String else { return nil }
        let created = (dict["created_at_ms"] as? Int64) ?? 0
        let updated = (dict["updated_at_ms"] as? Int64) ?? 0
        let count = (dict["message_count"] as? UInt64) ?? 0
        let title = dict["title"] as? String
        return SessionInfoView(
            id: id,
            createdAtMs: created,
            updatedAtMs: updated,
            messageCount: count,
            title: title
        )
    }

    /// Decode the raw `Message` JSON `session/load` returns into the flat
    /// `MessageView` shape `rebuildEntries` consumes. Each message serializes
    /// as `{ role, content: [{ type: "text", text }], metadata: {...} }`
    /// (oneai_core's wire shape — Role is `#[serde(rename_all="lowercase")]`,
    /// matching the FFI MessageView role strings). Flatten the text blocks,
    /// read `metadata["speaker"]` for group-chat attribution (nil for single-
    /// agent). Returns `[]` on a missing/malformed array.
    private static func messageViews(fromSidecar raw: Any?) -> [MessageView] {
        guard let arr = raw as? [[String: Any]] else { return [] }
        return arr.compactMap { m -> MessageView? in
            guard let role = m["role"] as? String else { return nil }
            // Flatten text content blocks into one string (mirrors the FFI
            // MessageView.text = concat of text blocks).
            var text = ""
            if let blocks = m["content"] as? [[String: Any]] {
                for b in blocks where (b["type"] as? String) == "text" {
                    if let t = b["text"] as? String { text += t }
                }
            }
            let speaker = (m["metadata"] as? [String: Any])?["speaker"] as? String
            return MessageView(role: role, text: text, speaker: speaker)
        }
    }

    /// Prepend one older page of transcript to `items` (lazy "load earlier
    /// messages"). Driven by the top-of-chat button in `Views`. After
    // prepending, `olderJumpId` is set to the page's first (oldest) id so the
    /// ScrollViewReader scrolls it into view — no NSScrollView offset-compensation
    /// timing race (the user is at the top, atBottom already false, no latch flip).
    func loadOlder() async {
        guard hasOlder, !olderLoading, let cursor = olderCursor, let s = session else { return }
        olderLoading = true
        StreamLog.log("sess", "loadOlder cursor=\(cursor) items=\(items.count)")
        let page = await s.transcriptOlder(cursor: cursor, limit: transcriptPageSize)
        var lastTask: String? = nil
        let prepend = Self.rebuildEntries(from: page.messages, lastTask: &lastTask)
        await MainActor.run {
            // Prepend older entries; keep lastUserTask the most-recent user msg.
            items = prepend + items
            hasOlder = (page.olderCursor != nil)
            olderCursor = page.olderCursor
            olderJumpId = prepend.first?.id
            olderLoading = false
            streamTick.value += 1
        }
        StreamLog.log("sess", "loadOlder done prepended=\(prepend.count) items=\(items.count) hasOlder=\(hasOlder)")
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
    ///
    /// `key` identifies the session the event belongs to (issue #13). When it
    /// matches the visible session, the event renders into `items` and bumps
    /// `streamTick`; otherwise it accumulates into `backgroundTurns[key]`
    /// (single-agent) with no re-render — preserving output streamed while the
    /// user was on another session for switch-back.
    func handle(_ event: ChatEventView, key: String) {
        let visible = (currentStreamKey() == key)
        let speakerId = speaker(of: event)
        // Resolve the turn this event mutates.
        let turn: AssistantItem
        if visible {
            if let sid = speakerId, activeSpeakerItem?.speakerId != sid {
                // New speaker → flush the previous item and start a new one.
                let item = AssistantItem()
                item.speakerId = sid
                activeSpeakerItem = item
                items.append(.assistant(item))
                activeSpeakerId = sid
                turn = item
            } else if activeSpeakerItem == nil {
                // Single-agent (speaker nil) — create the turn's item on first event.
                let item = AssistantItem()
                activeSpeakerItem = item
                items.append(.assistant(item))
                turn = item
            } else {
                turn = activeSpeakerItem!
            }
        } else if !key.hasPrefix("group:") {
            // Background single-agent: one buffered turn per session key
            // (speaker is always nil for single-agent). No `items` append —
            // the turn stays hidden until switch-back restores it.
            if let existing = backgroundTurns[key] {
                turn = existing
            } else {
                let item = AssistantItem()
                backgroundTurns[key] = item
                turn = item
            }
        } else {
            // Group background (only reachable via the visible→background race
            // window: a switch lands between scheduling and `handle`). Group
            // streams aren't buffered (rounds are short, resume isn't wired) —
            // drop the leftover event.
            return
        }

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
            if flipped && visible { lastStreamFlush = Date.distantPast }
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
        case .tokenUsage(let prompt, let completion, let cacheRead, let cacheCreation, _):
            // Real per-inference token usage from the provider. This drives the
            // top-bar token count + cache-hit badge from actual API numbers
            // (not the char-estimate fallback in `.complete`). Visible session
            // only — a background run must not clobber the visible bar.
            //
            // `prompt + completion == 0` means the provider reported no usage
            // (e.g. GLM streaming sends none in message_delta) → leave the
            // fields untouched so `.complete`'s char-estimate still surfaces a
            // number and the cache badge stays hidden (no data, not "0%").
            if visible && (prompt + completion) > 0 {
                lastTurnTokens += Int(prompt) + Int(completion)
                // Cache-hit ratio = cache_read / prompt_tokens, where
                // `prompt_tokens` is the total input footprint (OpenAI reports
                // the total directly; Anthropic normalizes input+cache_read+
                // creation in the provider). Matches the OpenAI dashboard's
                // cached_tokens / prompt_tokens. cacheCreation is NOT in the
                // denominator — prompt already covers the total input.
                let denom = max(Double(prompt), 1)
                lastCacheHitPct = Double(cacheRead) / denom * 100
            }
        case .complete(let finalText, _):
            if !finalText.isEmpty { turn.text = finalText }
            if turn.thinkingActive { turn.thinkingActive = false; turn.thinkingDone = true }
            turn.streaming = false; turn.done = true
            // Token count: prefer the real API total accumulated from
            // `.tokenUsage` events. Only fall back to a char-based estimate
            // when the provider reported no usage at all (lastTurnTokens still
            // 0 this turn) so the indicator isn't blank for such providers.
            if visible && lastTurnTokens == 0 {
                lastTurnTokens = (finalText.count + turn.thinking.count) / 4
            }
            if visible && currentScenario == nil { running = false }
        case .error(let message, _):
            turn.error = message; turn.streaming = false; turn.done = true
            if visible { running = false }
        }
        guard visible else { return }   // background: no version bump, no streamTick
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
        bumpStreamTick(isHot: hot)
    }

    /// Transport-agnostic throttle core: hot events (streamChunk/thinking +
    /// the sidecar's stream_chunk/thinking) coalesce to ~20 fps; everything
    /// else flushes immediately and resets the window. Shared by the FFI
    /// `handle(_ event: ChatEventView)` and the sidecar `handleSidecarEvent`.
    private func bumpStreamTick(isHot: Bool) {
        if isHot {
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
             .directAnswer(_, let s), .complete(_, let s), .error(_, let s),
             .tokenUsage(_, _, _, _, let s):
            return s
        }
    }

    func runTask(_ task: String, addUserItem: Bool = true) async {
        lastUserTask = task
        if transport == .sidecar {
            // Group chat over sidecar has no FFI `groupSession`; route on
            // `currentScenario != nil` (a scenario is active). Single-agent
            // falls through to runTaskSidecar.
            if currentScenario != nil {
                await runGroupTaskSidecar(task, addUserItem: addUserItem)
            } else {
                await runTaskSidecar(task, addUserItem: addUserItem)
            }
            return
        }
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
        // Reset the per-turn top-bar indicators: token count accumulates from
        // `.tokenUsage` events during the turn, cache-hit ratio reflects the
        // latest inference. Without this they'd carry over from the prior turn.
        lastTurnTokens = 0
        lastCacheHitPct = nil

        // Persist immediately so the new chat shows in the sidebar mid-turn.
        StreamLog.log("sess", "save pre-run id=\(s.sessionId())")
        try? await s.save()
        await refreshSessions()

        // Visible key matches this run so its events render (a background run
        // from a previous session keeps routing to its buffered turn).
        setActiveStreamKey(s.sessionId())
        let callback = StreamCallback(vm: self, sessionKey: s.sessionId())
        StreamLog.log("run", "single-agent runTask start len=\(task.count)")
        do {
            try await s.runTask(task: task, callback: callback)
            turn.streaming = false; turn.done = true
            StreamLog.log("run", "single-agent runTask end ok")
        } catch {
            turn.error = friendlyError(error)
            turn.streaming = false; turn.done = true
            StreamLog.log("run", "single-agent runTask err=\(friendlyError(error))")
        }
        // Only clear `running` if this run is still the visible one — a
        // background run completing while the user is on another (running)
        // session must not clobber that session's flag (issue #13).
        if currentStreamKey() == s.sessionId() {
            running = false
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
        // Reset per-turn top-bar indicators (see runTask).
        lastTurnTokens = 0
        lastCacheHitPct = nil
        // Visible key matches this group's stream so its events render.
        setActiveStreamKey(groupStreamKey)
        let callback = StreamCallback(vm: self, sessionKey: groupStreamKey)
        StreamLog.log("run", "group runTask start len=\(task.count)")
        do {
            try await gs.runTask(userInput: task, callback: callback)
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
        }
        // Only clear `running` if this group is still the visible session — a
        // group switched away from is interrupted, so its run ends, but it
        // must not clobber another session's flag (issue #13).
        if currentStreamKey() == groupStreamKey {
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
        if transport == .sidecar {
            // Cancel the in-flight sidecar turn (if any) via turn/cancel.
            // The adapter defaults a missing reason to a generic client
            // cancel, so an empty params object is fine.
            if let rpc = rpcClient, sidecarTurnId != nil {
                _ = try? await rpc.call("turn/cancel", params: [String: String]())
            }
            if let t = activeSpeakerItem { t.streaming = false; t.done = true }
            sidecarTurnId = nil
            running = false
            streamTick.value += 1
            return
        }
        if let gs = groupSession { await gs.interrupt() }
        await session?.interrupt()
    }

    // MARK: Sidecar run (Phase D, Slice 1)

    /// Single-agent turn over the sidecar JSON-RPC client. Mirrors the FFI
    /// `runTask`'s visible-path: append a UserItem + a streaming
    /// AssistantItem, set `running`, reset the per-turn indicators, then
    /// `turn/run`. The `turn/run` result resolves at TurnStart (carries
    /// `turn_id`); the actual stream + `turn_complete` arrive as `event`
    /// notifications handled by `handleSidecarEvent`, so this method does
    /// NOT mark the turn done — that happens on `turn_complete`.
    private func runTaskSidecar(_ task: String, addUserItem: Bool) async {
        guard let rpc = rpcClient else { self.error = "sidecar 引擎未就绪"; return }
        StreamLog.log("run", "sidecar runTask start len=\(task.count)")
        if addUserItem { items.append(.user(UserItem(text: task))) }
        let turn = AssistantItem()
        activeSpeakerItem = turn
        items.append(.assistant(turn))
        running = true
        error = nil
        lastTurnTokens = 0
        lastCacheHitPct = nil
        sidecarTurnId = nil
        // Fresh per-conversation stream key; visible key matches so events
        // render (no backgrounding in Slice 1, but the routing machinery is
        // shared with the FFI path and expects a non-empty visible key).
        sidecarStreamKey = "sidecar:" + UUID().uuidString
        setActiveStreamKey(sidecarStreamKey)
        // ContentBlock is `#[serde(tag="type")]`; Text → {"type":"text","text":…}.
        let params: [String: Any] = ["content": [["type": "text", "text": task]]]
        do {
            let res = try await rpc.call("turn/run", params: params)
            if let tid = res["turn_id"] as? String {
                sidecarTurnId = tid
                StreamLog.log("run", "sidecar turn/run resolved turn_id=\(tid)")
            } else {
                StreamLog.log("run", "sidecar turn/run result no turn_id: \(res)")
            }
        } catch {
            turn.error = friendlyError(error)
            turn.streaming = false; turn.done = true
            if currentStreamKey() == sidecarStreamKey { running = false }
            StreamLog.log("run", "sidecar runTask err=\(friendlyError(error))")
        }
    }

    /// Multi-agent turn over the sidecar JSON-RPC client. Mirrors the FFI
    /// `runGroupTask`'s visible-path: append the UserItem, reset the active
    /// speaker item (a new round starts; the first fragment seeds it), set
    /// `running`, then `group/run` (ack). The round's speaker-tagged fragments
    /// arrive as `event` notifications routed by `sidecarItemFor(speaker:)`;
    /// the round-end `turn_complete` (emitted by the sidecar runtime on round
    /// success) finalizes the last member's item + clears `running`.
    private func runGroupTaskSidecar(_ task: String, addUserItem: Bool) async {
        guard let rpc = rpcClient else { self.error = "sidecar 引擎未就绪"; return }
        StreamLog.log("run", "sidecar group runTask start len=\(task.count)")
        if addUserItem { items.append(.user(UserItem(text: task))) }
        activeSpeakerItem = nil     // a new round starts; first event seeds item
        activeSpeakerId = nil
        running = true
        error = nil
        lastTurnTokens = 0
        lastCacheHitPct = nil
        sidecarTurnId = nil
        // The group's stream key (set at group start); visible key matches so
        // events render, and `handleSidecarEvent`'s turn_complete guard
        // (currentStreamKey == sidecarStreamKey) clears `running`.
        sidecarStreamKey = groupStreamKey
        setActiveStreamKey(groupStreamKey)
        do {
            _ = try await rpc.call("group/run", params: ["user_input": task])
            StreamLog.log("run", "sidecar group/run sent len=\(task.count)")
        } catch {
            if activeSpeakerItem == nil {
                let item = AssistantItem()
                activeSpeakerItem = item
                items.append(.assistant(item))
            }
            activeSpeakerItem?.error = friendlyError(error)
            activeSpeakerItem?.streaming = false
            activeSpeakerItem?.done = true
            if currentStreamKey() == sidecarStreamKey { running = false }
            StreamLog.log("run", "sidecar group/run err=\(friendlyError(error))")
        }
    }

    /// Resolve the AssistantItem a sidecar fragment event mutates, with
    /// speaker routing for group chat. Mirrors the FFI `handle`'s speaker
    /// switch (1257-1263): a fragment carrying a non-nil `speaker` that
    /// differs from the active item's `speakerId` finalizes the previous item
    /// and starts a new one. `speaker == nil` (single-agent) reuses/creates
    /// one item. Always-visible (the sidecar has no background buffering).
    private func sidecarItemFor(speaker: String?) -> AssistantItem? {
        if let sid = speaker, activeSpeakerItem?.speakerId != sid {
            // New speaker → finalize the previous member's item, start fresh.
            if let prev = activeSpeakerItem { prev.streaming = false; prev.done = true }
            let item = AssistantItem()
            item.speakerId = sid
            activeSpeakerItem = item
            items.append(.assistant(item))
            activeSpeakerId = sid
            return item
        }
        if activeSpeakerItem == nil {
            let item = AssistantItem()
            if let sid = speaker { item.speakerId = sid; activeSpeakerId = sid }
            activeSpeakerItem = item
            items.append(.assistant(item))
        }
        return activeSpeakerItem
    }

    /// Route a sidecar `event` notification into the active AssistantItem.
    /// Independent of the FFI `handle(_ event: ChatEventView)` — the FFI
    /// path's `ChatEventView` enum and the sidecar's `[String: Any]` EngineYield
    /// are different shapes; this is the sidecar's own event→items fold. Reuses
    /// the same `AssistantItem` fields + `bumpStreamTick(isHot:)` throttle so
    /// the rendering + coalescing machinery is identical to the FFI path.
    func handleSidecarEvent(_ event: OneAiEvent) {
        let p = event.params
        // Log every event kind so a stuck turn is diagnosable from
        // oneai_stream.log — shows whether stream_chunk/turn_complete arrive
        // (engine completed but render broke) vs. only tool/approval events
        // (engine stuck in a tool loop).
        StreamLog.log("sidecar", "event kind=\(event.kind)")
        switch event.kind {
        case "speaker_turn":
            // Engine brackets a member's turn with this before their fragments.
            // The first fragment carries the same `speaker` id and seeds the
            // item via `sidecarItemFor`, so this is informational only (log);
            // pre-creating an empty bubble here would flash before any content.
            let sp = (p["speaker"] as? String) ?? ""
            StreamLog.log("sidecar", "speaker_turn=\(sp)")
        case "stream_chunk":
            let text = (p["text"] as? String) ?? ""
            guard let t = sidecarItemFor(speaker: p["speaker"] as? String) else { break }
            let flipped = t.thinkingActive
            if flipped { t.thinkingActive = false; t.thinkingDone = true }
            t.streaming = true; t.text += text
            if flipped { lastStreamFlush = Date.distantPast }
            t.version += 1
            bumpStreamTick(isHot: true)
        case "thinking":
            let text = (p["text"] as? String) ?? ""
            guard let t = sidecarItemFor(speaker: p["speaker"] as? String) else { break }
            t.thinkingActive = true
            t.thinking += text
            t.version += 1
            bumpStreamTick(isHot: true)
        case "direct_answer":
            let text = (p["text"] as? String) ?? ""
            guard let t = sidecarItemFor(speaker: p["speaker"] as? String) else { break }
            if !text.isEmpty { t.text = text }
            if t.thinkingActive { t.thinkingActive = false; t.thinkingDone = true }
            t.version += 1
            bumpStreamTick(isHot: false)
        case "tool_calls":
            // calls: [{id, name, args}]
            guard let t = sidecarItemFor(speaker: p["speaker"] as? String),
                  let calls = p["calls"] as? [[String: Any]] else { break }
            for c in calls {
                let id = (c["id"] as? String) ?? UUID().uuidString
                if t.steps.contains(where: { $0.callId == id }) { continue }
                let name = (c["name"] as? String) ?? "?"
                let args = c["args"].map { String(describing: $0) } ?? ""
                t.steps.append(ToolStep(callId: id, name: name, args: args))
            }
            t.version += 1
            bumpStreamTick(isHot: false)
        case "tool_result":
            let callId = (p["call_id"] as? String) ?? ""
            let output = p["output"] as? [String: Any]
            let content = (output?["content"] as? String) ?? ""
            let success = (output?["success"] as? Bool) ?? true
            // tool_result carries the speaker too — route to that member's item.
            guard let t = sidecarItemFor(speaker: p["speaker"] as? String) else { break }
            if let idx = t.steps.firstIndex(where: { $0.callId == callId }) {
                t.steps[idx].result = content
                t.steps[idx].ok = success
            } else if let idx = t.steps.lastIndex(where: { $0.result == nil }) {
                t.steps[idx].result = content
                t.steps[idx].ok = success
            }
            t.version += 1
            bumpStreamTick(isHot: false)
        case "token_usage":
            // Real per-inference usage from the provider (mirrors the FFI
            // `.tokenUsage` arm). Drives the top-bar token + cache badge.
            let usage = p["usage"] as? [String: Any]
            let prompt = (usage?["prompt_tokens"] as? Int) ?? 0
            let completion = (usage?["completion_tokens"] as? Int) ?? 0
            let cacheRead = (usage?["cache_read_tokens"] as? Int) ?? 0
            if (prompt + completion) > 0 {
                lastTurnTokens += prompt + completion
                let denom = max(Double(prompt), 1)
                lastCacheHitPct = Double(cacheRead) / denom * 100
                bumpStreamTick(isHot: false)
            }
        case "turn_complete":
            let summary = p["summary"] as? [String: Any]
            let final = (summary?["final_answer"] as? String) ?? ""
            if let t = activeSpeakerItem {
                if !final.isEmpty { t.text = final }
                if t.thinkingActive { t.thinkingActive = false; t.thinkingDone = true }
                t.streaming = false; t.done = true
                if lastTurnTokens == 0 {
                    lastTurnTokens = (final.count + t.thinking.count) / 4
                }
                t.version += 1
            }
            if currentStreamKey() == sidecarStreamKey { running = false }
            sidecarTurnId = nil
            bumpStreamTick(isHot: false)
            StreamLog.log("run", "sidecar turn_complete")
            // The turn auto-saved the conversation (run_agent persists on
            // completion); refresh the sidebar so a brand-new chat appears
            // and an updated session bubbles up in updatedAt order.
            Task { [weak self] in
                await self?.refreshSessions()
            }
        case "error":
            let msg = (p["message"] as? String) ?? "engine error"
            if let t = activeSpeakerItem {
                t.error = msg; t.streaming = false; t.done = true
                t.version += 1
            } else {
                self.error = msg
            }
            if currentStreamKey() == sidecarStreamKey { running = false }
            bumpStreamTick(isHot: false)
        case "approval_request":
            // Sidecar uses BusInteractionGate → it surfaces approvals the FFI
            // NoopInteractionGate auto-grants. For Slice-1 parity with the FFI
            // transport (which NEVER prompts — best UX, no friction), auto-
            // approve every tool call (Proceed) and log the tool so the
            // engine never blocks on a prompt. The `ApprovalOverlay` UI stays
            // available for a later slice that surfaces real approval gating;
            // it's just not set here. (Same trust model as the FFI app, which
            // auto-runs shell/file tools via Noop.)
            let reqId = (p["request_id"] as? String) ?? ""
            // InteractionRequest is externally-tagged: {"ToolApproval":{"approval":{…}}}.
            var toolName = "engine"
            if let request = p["request"] as? [String: Any],
               let approval = request["ToolApproval"] as? [String: Any],
               let a = approval["approval"] as? [String: Any] {
                if let n = a["tool_name"] as? String { toolName = n }
            }
            StreamLog.log("sidecar", "approval_request auto-proceed tool=\(toolName) id=\(reqId)")
            // Fire-and-forget: respond so the engine unblocks. handleSidecarEvent
            // is sync (runs on the main callback queue), so dispatch the async
            // respond onto a Task — it must not block the event loop.
            let rpc = rpcClient
            Task { [weak self] in
                guard let rpc = rpc else { return }
                let params: [String: Any] = ["request_id": reqId, "response": "Proceed"]
                do {
                    _ = try await rpc.call("approval/respond", params: params)
                } catch {
                    self?.error = "auto-approval respond 失败: \(friendlyError(error))"
                }
            }
        default:
            // Unknown `kind` — the bus is `#[non_exhaustive]`; new yield
            // variants arrive as unknown kinds a frontend ignores. No-op.
            break
        }
    }

    /// Resolve the pending sidecar approval: `approval/respond` with
    /// `InteractionResponse::Proceed` (allow) or `Abort` (deny). The engine
    /// unblocks and the turn continues / aborts.
    func respondApproval(_ allow: Bool) async {
        guard let rpc = rpcClient, let pa = pendingApproval else { return }
        pendingApproval = nil
        // InteractionResponse is externally-tagged: `Proceed` (unit) → bare
        // string "Proceed"; `Abort { reason }` → {"Abort":{"reason":…}}.
        let response: Any = allow
            ? "Proceed"
            : ["Abort": ["reason": NSLocalizedString("用户拒绝", comment: "")]]
        let params: [String: Any] = ["request_id": pa.requestId, "response": response]
        do {
            _ = try await rpc.call("approval/respond", params: params)
        } catch {
            self.error = "审批回复失败: \(friendlyError(error))"
        }
    }
}

// MARK: - Sidecar transport delegates (Phase D, Slice 1)

extension ChatViewModel: OneAiRpcClientDelegate {
    func rpcClient(_ client: OneAiRpcClient, didReceive event: OneAiEvent) {
        // callbackQueue is set to .main in `engineProcessManager(_:didStart:)`,
        // so this already lands on the main actor — `@Published` mutations +
        // `activeSpeakerItem` field writes are safe.
        handleSidecarEvent(event)
    }

    func rpcClient(_ client: OneAiRpcClient, didCloseWithError error: Error?) {
        // The EngineProcessManager restarts the child; surface a transport
        // error only if a turn is mid-flight so the user sees why it stalled.
        if running, let t = activeSpeakerItem {
            t.error = NSLocalizedString("引擎连接断开,正在重连…", comment: "")
            t.streaming = false
            t.version += 1
            running = false
            bumpStreamTick(isHot: false)
        }
        rpcClient = nil
    }
}

extension ChatViewModel: EngineProcessManagerDelegate {
    func engineProcessManager(_ mgr: EngineProcessManager, didStart client: OneAiRpcClient) {
        // didStart fires from a background queue (connectClient's Task); hop
        // to main before touching VM state (incl. resuming the ensureApp
        // continuation).
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.rpcClient = client
            client.delegate = self
            // Deliver events on main so `@Published` mutations + the
            // plain-field writes in `handleSidecarEvent` are main-actor
            // (mirrors the FFI callback's main-thread marshalling).
            client.callbackQueue = .main
            // Phase G3: the scenario library is server-authoritative over
            // `scenario/*`. Hand the store the client + pull the shared
            // library (merging local presets + server customs, migrating
            // local customs on first connect).
            self.agentStore.rpcClient = client
            Task { await self.agentStore.refresh() }
            let cont = self.sidecarStartCont
            self.sidecarStartCont = nil
            cont?.resume()
        }
    }

    func engineProcessManager(_ mgr: EngineProcessManager, didFailWith error: Error) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            let cont = self.sidecarStartCont
            self.sidecarStartCont = nil
            // Only resume once — a late didFail after a successful start is
            // surfaced as a transport error, not an ensureApp throw.
            if let cont = cont {
                cont.resume(throwing: error)
            } else if self.rpcClient == nil {
                self.error = "引擎 sidecar 失败: \(friendlyError(error))"
            }
        }
    }
}
