// ChatViewModel + models + streaming callback — port of Android's ChatViewModel.
// Events from the Rust tokio worker thread are marshalled to the main thread
// (DispatchQueue.main.async), mirroring Android's runOnUiThread.

import Foundation
import SwiftUI

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

final class AssistantItem: ObservableObject, Identifiable {
    let id = UUID()
    @Published var thinking = ""
    @Published var thinkingActive = false
    @Published var thinkingDone = false
    @Published var thinkingExpanded = false
    @Published var steps: [ToolStep] = []
    @Published var text = ""
    @Published var streaming = false
    @Published var done = false
    @Published var error: String? = nil
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
    let turn: AssistantItem
    init(vm: ChatViewModel, turn: AssistantItem) { self.vm = vm; self.turn = turn }
    func onEvent(event: ChatEventView) {
        // Fires on the tokio worker thread → marshal to main before touching UI.
        let turn = self.turn
        let vm = self.vm
        DispatchQueue.main.async { vm?.handle(event, turn: turn) }
    }
}

// MARK: - View model

final class ChatViewModel: ObservableObject {
    private let prefs = UserDefaults(suiteName: "oneai_provider") ?? .standard

    @Published var kind: String
    @Published var model: String
    @Published var apiKey: String
    @Published var baseUrl: String

    @Published var items: [ChatEntry] = []
    @Published var sessions: [SessionInfoView] = []
    @Published var input = ""
    @Published var running = false
    @Published var error: String? = nil
    @Published var streamTick: Int64 = 0
    @Published var currentSessionId: String? = nil

    var needsKeyConfig: Bool {
        (kind == "openai" || kind == "anthropic") && apiKey.isEmpty
    }

    private var lastUserTask: String? = nil
    private var app: OneAiApp? = nil
    private var session: OneAiSession? = nil

    var dbPath: String {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("oneai.db").path
    }

    init() {
        let p = UserDefaults(suiteName: "oneai_provider") ?? .standard
        kind = p.string(forKey: "kind") ?? "openai"
        model = p.string(forKey: "model") ?? "gpt-4o-mini"
        apiKey = p.string(forKey: "apiKey") ?? ""
        baseUrl = p.string(forKey: "baseUrl") ?? ""
        prefs.register(defaults: ["kind": "openai", "model": "gpt-4o-mini"])
    }

    // MARK: Provider config

    func applyProviderPreset(_ newKind: String) {
        guard newKind != kind else { return }
        kind = newKind
        switch newKind {
        case "openai":     model = "gpt-4o-mini"; baseUrl = ""
        case "anthropic":  model = "claude-sonnet-4-6"; baseUrl = ""
        case "ollama":     model = "llama3"; baseUrl = "127.0.0.1:11434"
        default: break
        }
    }

    func saveConfig() {
        prefs.set(kind, forKey: "kind")
        prefs.set(model, forKey: "model")
        prefs.set(apiKey, forKey: "apiKey")
        prefs.set(baseUrl, forKey: "baseUrl")
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

    // MARK: App lifecycle

    func ensureApp() async {
        guard app == nil else { return }
        do {
            var builder = OneAiAppBuilder()
            builder = try builder.providerConfig(cfg: providerConfigView())
            builder = builder.defaultTools()
            builder = builder.sqlitePersistenceAt(path: dbPath)
            app = try await builder.build()
        } catch {
            self.error = "build failed: \(friendlyError(error))"
        }
    }

    func rebuildApp() async {
        app = nil
        session = nil
        currentSessionId = nil
        items.removeAll()
        error = nil
        await ensureApp()
        await refreshSessions()
        if let cur = sessions.first {
            await loadSession(cur.id)
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
        guard let a = app else { return }
        let s = a.createSession()
        session = s
        currentSessionId = s.sessionId()
        items.removeAll()
        error = nil
    }

    func loadSession(_ id: String) async {
        guard let a = app else { return }
        let s = await a.createSessionWithId(id: id)
        session = s
        currentSessionId = s.sessionId()
        items.removeAll()
        error = nil
        lastUserTask = nil
        let msgs = await s.messages()
        for m in msgs {
            switch m.role {
            case "user":
                if !m.text.isEmpty { items.append(.user(UserItem(text: m.text))); lastUserTask = m.text }
            case "assistant":
                if !m.text.isEmpty {
                    let item = AssistantItem()
                    item.text = m.text
                    item.done = true
                    items.append(.assistant(item))
                }
            default: break // system / tool — not replayed
            }
        }
        streamTick += 1
    }

    func deleteSession(_ id: String) async {
        guard let a = app else { return }
        try? await a.deleteConversation(id: id)
        await refreshSessions()
        if id == currentSessionId { await newConversation() }
    }

    // MARK: Run

    func handle(_ event: ChatEventView, turn: AssistantItem) {
        switch event {
        case .thinking(let text):
            turn.thinkingActive = true; turn.thinking += text
        case .streamChunk(let text):
            if turn.thinkingActive { turn.thinkingActive = false; turn.thinkingDone = true }
            turn.streaming = true; turn.text += text
        case .toolCall(let id, let name, let argsJson):
            turn.steps.append(ToolStep(callId: id, name: name, args: argsJson))
        case .toolResult(let callId, _, let content, let success):
            if let idx = turn.steps.firstIndex(where: { $0.callId == callId }) {
                turn.steps[idx].result = content
                turn.steps[idx].ok = success
            } else if let idx = turn.steps.lastIndex(where: { $0.result == nil }) {
                turn.steps[idx].result = content
                turn.steps[idx].ok = success
            }
        case .directAnswer(let text):
            if !text.isEmpty { turn.text = text }
            if turn.thinkingActive { turn.thinkingActive = false; turn.thinkingDone = true }
        case .complete(let finalText):
            if !finalText.isEmpty { turn.text = finalText }
            if turn.thinkingActive { turn.thinkingActive = false; turn.thinkingDone = true }
            turn.streaming = false; turn.done = true; running = false
        case .error(let message):
            turn.error = message; turn.streaming = false; turn.done = true; running = false
        }
        streamTick += 1
    }

    func runTask(_ task: String, addUserItem: Bool = true) async {
        guard let s = session else { self.error = "session not built"; return }
        if addUserItem { items.append(.user(UserItem(text: task))) }
        lastUserTask = task
        let turn = AssistantItem()
        items.append(.assistant(turn))
        running = true
        error = nil

        // Persist immediately so the new chat shows in the sidebar mid-turn.
        try? await s.save()
        await refreshSessions()

        let callback = StreamCallback(vm: self, turn: turn)
        do {
            try await s.runTask(task: task, callback: callback)
            turn.streaming = false; turn.done = true; running = false
        } catch {
            turn.error = friendlyError(error)
            turn.streaming = false; turn.done = true; running = false
        }
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

    func stop() async {
        await session?.interrupt()
    }
}
