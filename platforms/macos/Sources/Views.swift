// SwiftUI views — macOS port of the Android Compose chat UI.
// NavigationSplitView (sidebar = session list) + detail (chat). Settings,
// delete-confirm, first-run hint, scroll-to-bottom, copy/share, retry all
// reproduced. Dark theme follows the system via the adaptive Theme palette.

import SwiftUI
import AppKit

// MARK: - Root screen

struct ChatScreen: View {
    @StateObject private var vm = ChatViewModel()
    @StateObject private var artifacts = ArtifactStore()
    @State private var showSettings = false
    @State private var showCommandPalette = false
    @State private var pendingDeleteId: String? = nil

    var body: some View {
        NavigationSplitView {
            Sidebar(vm: vm, onOpenSettings: { showSettings = true },
                    onDelete: { pendingDeleteId = $0 })
                .navigationSplitViewColumnWidth(min: 220, ideal: 260)
        } detail: {
            ChatDetail(vm: vm, onOpenSettings: { showSettings = true })
                .environmentObject(artifacts)
        }
        .environmentObject(artifacts)
        .background(
            // ⌘K opens the command palette.
            Button("") { showCommandPalette = true }
                .keyboardShortcut("k", modifiers: .command)
                .opacity(0)
        )
        .sheet(isPresented: $showCommandPalette) {
            CommandPalette(vm: vm, isPresented: $showCommandPalette)
        }
        .task {
            await vm.ensureApp()
            await vm.refreshSessions()
            if let mostRecent = vm.sessions.first {
                await vm.loadSession(mostRecent.id)
            } else {
                await vm.newConversation()
            }
        }
        .sheet(isPresented: $showSettings) {
            SettingsSheet(vm: vm, onClose: { showSettings = false })
        }
        .alert("删除会话", isPresented: Binding(
            get: { pendingDeleteId != nil },
            set: { if !$0 { pendingDeleteId = nil } })) {
            Button("取消", role: .cancel) { pendingDeleteId = nil }
            Button("删除", role: .destructive) {
                if let id = pendingDeleteId { Task { await vm.deleteSession(id) } }
                pendingDeleteId = nil
            }
        } message: {
            Text("确定删除这个会话?历史无法恢复。")
        }
    }
}

// MARK: - Sidebar (session drawer equivalent)

private struct Sidebar: View {
    @ObservedObject var vm: ChatViewModel
    let onOpenSettings: () -> Void
    let onDelete: (String) -> Void
    /// Single sheet source-of-truth. SwiftUI glitches when two `.sheet`
    /// modifiers attach to the same view (empty/unclosable sheet); one
    /// enum-driven sheet sidesteps that entirely.
    private enum SidebarSheet: Identifiable {
        case editScenario(Scenario)   // new or edit a scenario in the editor
        case topicFor(Scenario)      // prompt for a topic before starting
        var id: String {
            switch self {
            case .editScenario(let s): return "edit-\(s.id)"
            case .topicFor(let s):    return "topic-\(s.id)"
            }
        }
    }
    @State private var sheet: SidebarSheet? = nil

    /// Start a scenario: if it declares topic-intake fields, prompt for them
    /// first; otherwise start immediately.
    private func startScenario(_ sc: Scenario) {
        if !(sc.topicFields?.isEmpty ?? true) {
            sheet = .topicFor(sc)
        } else {
            Task { await vm.newConversation(scenario: sc) }
        }
    }

    /// New-conversation menu: single-agent chat, or start from a scenario.
    private var newConversationMenu: some View {
        Menu {
            Button("新对话(单 Agent)") { Task { await vm.newConversation() } }
            Menu("从场景开始") {
                ForEach(vm.agentStore.scenarios) { sc in
                    Button(sc.name) { startScenario(sc) }
                }
            }
        } label: {
            Label("新建", systemImage: "plus")
        }
    }

    /// The scrollable scenario + recent-session list.
    private var sidebarList: some View {
        VStack(spacing: 0) {
            scenariosSection
            sessionsSection
        }
    }

    private var scenariosSection: some View {
        SidebarSection(title: "场景", trailing: newScenarioButton) {
            ForEach(vm.agentStore.scenarios) { sc in
                ScenarioRow(scenario: sc, isCurrent: vm.currentScenario?.id == sc.id) {
                    startScenario(sc)
                }
                .contextMenu {
                    Button("编辑场景") { sheet = .editScenario(sc) }
                    Button("删除场景", role: .destructive) { vm.agentStore.delete(sc) }
                }
            }
        }
    }

    private var newScenarioButton: some View {
        Button {
            let sc = Scenario(id: UUID().uuidString, name: "新场景", icon: "person.2",
                              agents: [], turnPolicy: .scripted, scriptOrder: nil,
                              moderatorId: nil, openerAgentId: nil, openerLine: nil,
                              topicFields: nil, debrief: nil)
            sheet = .editScenario(sc)
        } label: { Image(systemName: "plus") }
    }

    private var sessionsSection: some View {
        SidebarSection(title: "最近会话") {
            if vm.sessions.isEmpty {
                Text("还没有会话\n发一条消息开始吧")
                    .foregroundStyle(Theme.onSurfaceVar)
                    .font(.footnote)
                    .padding(.vertical, 8)
            } else {
                ForEach(vm.sessions, id: \.id) { s in
                    SessionRow(info: s, isCurrent: s.id == vm.currentSessionId,
                               onTap: { Task { await vm.loadSession(s.id) } },
                               onDelete: { onDelete(s.id) })
                }
            }
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("会话").font(.headline)
                Spacer()
                newConversationMenu
                    .menuStyle(.borderlessButton)
                    .fixedSize()
                    .help("新建对话 / 从场景开始")
            }
            .padding(.horizontal, 12).padding(.vertical, 10)
            Divider()

            ScrollView {
                sidebarList
            }
        }
        .background(Theme.surface)
        .sheet(item: $sheet) { presented in
            switch presented {
            case .editScenario(let sc):
                ScenarioEditor(scenario: sc, store: vm.agentStore,
                               onClose: { sheet = nil })
            case .topicFor(let sc):
                ScenarioFormSheet(scenario: sc, vm: vm) { sheet = nil }
            }
        }
    }
}

private struct SidebarSection<Content: View>: View {
    let title: String
    let trailing: AnyView?
    @ViewBuilder let content: () -> Content

    init(title: String, @ViewBuilder content: @escaping () -> Content) {
        self.title = title; self.trailing = nil; self.content = content
    }
    init<T: View>(title: String, trailing: T, @ViewBuilder content: @escaping () -> Content) {
        self.title = title; self.trailing = AnyView(trailing); self.content = content
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(title).font(.caption.bold()).foregroundStyle(Theme.onSurfaceVar)
                Spacer()
                if let trailing { trailing }
            }
            .padding(.horizontal, 12).padding(.top, 8)
            content()
        }
    }
}

private struct ScenarioRow: View {
    let scenario: Scenario
    let isCurrent: Bool
    let onTap: () -> Void
    var body: some View {
        Button(action: onTap) {
            HStack(spacing: 8) {
                Image(systemName: scenario.icon)
                    .foregroundStyle(Theme.primary)
                    .frame(width: 22)
                Text(scenario.name)
                    .font(.subheadline)
                    .fontWeight(isCurrent ? .semibold : .regular)
                    .lineLimit(1)
                Spacer()
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12).padding(.vertical, 5)
            .background(isCurrent ? Theme.primaryCont.opacity(0.5) : Color.clear)
            .clipShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .padding(.horizontal, 6)
    }
}

/// Topic-intake form shown before starting a scenario that declares
/// `topicFields`. The collected values (keyed by field id) are baked into each
/// member's system prompt as background and into the session title by
/// `Scenario.specView`. Blank fields are allowed — empty values are dropped.
private struct ScenarioFormSheet: View {
    let scenario: Scenario
    @ObservedObject var vm: ChatViewModel
    let onClose: () -> Void
    /// field id → value, seeded empty.
    @State private var values: [String: String] = [:]
    @FocusState private var focusedField: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 8) {
                Image(systemName: scenario.icon)
                    .foregroundStyle(Theme.primary).font(.title3)
                Text("开始「\(scenario.name)」").font(.headline)
                Spacer()
            }
            ForEach(scenario.topicFields ?? []) { f in
                VStack(alignment: .leading, spacing: 3) {
                    Text(f.label).font(.caption).foregroundStyle(Theme.onSurfaceVar)
                    TextField(f.placeholder ?? f.label, text: Binding(
                        get: { values[f.id] ?? "" },
                        set: { values[f.id] = $0 }
                    ))
                    .textFieldStyle(.roundedBorder)
                    .focused($focusedField, equals: f.id)
                    .onSubmit { start() }
                }
            }
            Text("开场 agent 会围绕你输入的信息发言;这些值会作为各角色背景,并写入会话名。留空可直接开始。")
                .font(.caption2).foregroundStyle(Theme.onSurfaceVar)
            HStack {
                Spacer()
                Button("取消", role: .cancel, action: onClose).keyboardShortcut(.escape)
                Button("开始") { start() }.keyboardShortcut(.defaultAction)
            }
        }
        .frame(width: 440)
        .padding(16)
        .onAppear { focusedField = scenario.topicFields?.first?.id }
    }

    private func start() {
        let v = values
        onClose()
        Task { await vm.newConversation(scenario: scenario, topicValues: v) }
    }
}

private struct SessionRow: View {
    let info: SessionInfoView
    let isCurrent: Bool
    let onTap: () -> Void
    let onDelete: () -> Void
    var body: some View {
        Button(action: onTap) {
            HStack(alignment: .center) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(info.title?.isEmpty == false ? info.title! : "新对话")
                        .font(.subheadline)
                        .fontWeight(isCurrent ? .semibold : .regular)
                        .lineLimit(1)
                    Text("\(info.messageCount) 条 · \(relativeTime(info.updatedAtMs))")
                        .font(.caption)
                        .foregroundStyle(Theme.onSurfaceVar)
                        .lineLimit(1)
                }
                Spacer()
                Button(action: onDelete) {
                    Image(systemName: "trash")
                        .foregroundStyle(Theme.onSurfaceVar)
                }
                .buttonStyle(.plain)
                .help("删除")
            }
            .padding(.horizontal, 12).padding(.vertical, 5)
            .background(isCurrent ? Theme.primaryCont.opacity(0.5) : Color.clear)
            .clipShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .padding(.horizontal, 6)
    }
}

private func relativeTime(_ epochMs: Int64) -> String {
    let diff = Int64(Date().timeIntervalSince1970 * 1000) - epochMs
    let mins = diff / 60_000
    if mins < 1 { return "刚刚" }
    if mins < 60 { return "\(mins) 分钟前" }
    if mins < 60 * 24 { return "\(mins / 60) 小时前" }
    if mins < 60 * 24 * 7 { return "\(mins / (60 * 24)) 天前" }
    let f = DateFormatter(); f.dateFormat = "MM-dd HH:mm"
    return f.string(from: Date(timeIntervalSince1970: TimeInterval(epochMs) / 1000))
}

// MARK: - Chat detail

private struct ChatDetail: View {
    @ObservedObject var vm: ChatViewModel
    @EnvironmentObject var artifacts: ArtifactStore
    let onOpenSettings: () -> Void
    @State private var stickToBottom = true

    var body: some View {
        if artifacts.visible {
            HSplitView {
                detailContent
                ArtifactCanvas(store: artifacts)
                    .frame(minWidth: 280, idealWidth: 420)
            }
        } else {
            detailContent
        }
    }

    private var detailContent: some View {
        VStack(spacing: 0) {
            // Top bar
            HStack {
                if let sc = vm.currentScenario {
                    Image(systemName: sc.icon).foregroundStyle(Theme.primary)
                    Text(sc.name).font(.title3.bold()).foregroundStyle(Theme.onBg)
                    if vm.debriefActive {
                        // Debrief phase indicator.
                        Text("· 总结阶段").font(.caption).foregroundStyle(Theme.onSurfaceVar)
                    } else if let debrief = sc.debrief {
                        // "结束面试" button — switches to the debrief member only.
                        Button {
                            Task { await vm.endScenarioDebrief() }
                        } label: {
                            Label(debrief.buttonLabel, systemImage: "checkmark.circle")
                                .font(.caption)
                        }
                        .buttonStyle(.bordered)
                        .disabled(vm.running)
                        .help("结束并进入总结阶段")
                    }
                } else {
                    Text("OneAI").font(.title3.bold()).foregroundStyle(Theme.onBg)
                }
                Spacer()
                if vm.lastTurnTokens > 0 {
                    Label("\(vm.lastTurnTokens) tok", systemImage: "flame")
                        .font(.caption).foregroundStyle(Theme.onSurfaceVar)
                        .help("本轮约 token 数")
                }
                Button { onOpenSettings() } label: { Image(systemName: "gearshape") }
                    .help("Provider 设置")
            }
            .padding(.horizontal, 16).padding(.vertical, 8)
            Divider()

            // Turn-status bar (group-chat only): who's speaking / waiting.
            if vm.currentScenario != nil {
                TurnStatusBar(vm: vm)
                Divider()
            }

            if vm.needsKeyConfig {
                FirstRunHint(onOpen: onOpenSettings).padding(.horizontal, 12).padding(.vertical, 6)
            }

            // Message list
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 14) {
                        ForEach(vm.items) { entry in
                            switch entry {
                            case .user(let u): UserBubble(text: u.text, onEdit: { newText in Task { await vm.editAndResend(u, newText: newText) } })
                            case .assistant(let a): AssistantBubble(item: a, scenario: vm.currentScenario, onRetry: { Task { await vm.retryLast() } })
                            }
                        }
                        Color.clear.frame(height: 1).id("bottom")
                    }
                    .padding(12)
                }
                .onChange(of: vm.streamTick) { _ in
                    if stickToBottom { withAnimation { proxy.scrollTo("bottom", anchor: .bottom) } }
                }
                .onChange(of: vm.items.count) { _ in
                    if stickToBottom { proxy.scrollTo("bottom", anchor: .bottom) }
                }
            }

            if let msg = vm.error {
                Text("✗ \(msg)").foregroundStyle(Theme.errorC).font(.caption)
                    .padding(.horizontal, 12).padding(.vertical, 4)
            }

            InputBar(value: $vm.input, running: vm.running,
                     onChange: { vm.input = $0 },
                     onSend: {
                         let task = vm.input.trimmingCharacters(in: .whitespacesAndNewlines)
                         if !task.isEmpty && !vm.running {
                             vm.input = ""; stickToBottom = true
                             Task { await vm.runTask(task) }
                         }
                     },
                     onStop: { Task { await vm.stop() } },
                     canSend: {
                         !vm.running && !vm.input.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                     })
        }
        .background(Theme.background)
    }
}

// MARK: - Bubbles

private struct UserBubble: View {
    let text: String
    let onEdit: (String) -> Void
    @State private var editing = false
    @State private var draft = ""
    var body: some View {
        HStack { Spacer(minLength: 60)
            Text(text).foregroundStyle(Theme.onBg)
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(Theme.primaryCont)
                .clipShape(RoundedRectangle(cornerRadius: 14))
                .frame(maxWidth: 360, alignment: .trailing)
                .contextMenu {
                    Button("编辑并重发") { draft = text; editing = true }
                    Button("复制") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(text, forType: .string)
                    }
                }
        }
        .sheet(isPresented: $editing) {
            VStack(spacing: 12) {
                Text("编辑消息").font(.headline)
                TextEditor(text: $draft)
                    .font(.body).scrollContentBackground(.hidden)
                    .background(Theme.surfaceVar).clipShape(RoundedRectangle(cornerRadius: 8))
                    .frame(minHeight: 100, maxHeight: 240)
                HStack {
                    Spacer()
                    Button("取消", role: .cancel) { editing = false }.keyboardShortcut(.escape)
                    Button("重发") { let s = draft; editing = false; onEdit(s) }.keyboardShortcut(.defaultAction)
                }
            }.frame(width: 420).padding(16)
        }
    }
}

private struct SpeakerHeader: View {
    let speakerId: String?
    let scenario: Scenario?
    var body: some View {
        let meta = AgentStore.speakerMeta(for: speakerId ?? "", in: scenario)
        HStack(spacing: 6) {
            Image(systemName: meta.2)
                .foregroundStyle(Color(hex: meta.1))
            Text(meta.0)
                .font(.subheadline.bold())
                .foregroundStyle(Color(hex: meta.1))
            if let a = scenario?.agent(speakerId ?? "") {
                Text(a.role)
                    .font(.caption2)
                    .padding(.horizontal, 6).padding(.vertical, 1)
                    .background(Color(hex: a.color).opacity(0.18))
                    .foregroundStyle(Color(hex: a.color))
                    .clipShape(Capsule())
            }
        }
    }
}

/// Compact turn-status bar: shows the active speaker / who's waiting.
private struct TurnStatusBar: View {
    @ObservedObject var vm: ChatViewModel
    var body: some View {
        let policyLabel = vm.currentScenario?.turnPolicy.label ?? ""
        HStack(spacing: 6) {
            if vm.running, let sid = vm.activeSpeakerId {
                let meta = AgentStore.speakerMeta(for: sid, in: vm.currentScenario)
                Image(systemName: meta.2).foregroundStyle(Color(hex: meta.1))
                Text("\(meta.0) 正在发言").font(.caption).foregroundStyle(Theme.onSurfaceVar)
                ThreeDots()
            } else {
                Image(systemName: "hand.raised").foregroundStyle(Theme.onSurfaceVar)
                Text("轮到你 — 发送你的回答").font(.caption).foregroundStyle(Theme.onSurfaceVar)
            }
            Spacer()
            Text("策略: \(policyLabel)").font(.caption2).foregroundStyle(Theme.onSurfaceVar)
        }
        .padding(.horizontal, 16).padding(.vertical, 5)
        .background(Theme.secondaryCont)
    }
}

private struct AssistantBubble: View {
    let item: AssistantItem
    let scenario: Scenario?
    let onRetry: () -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            // Speaker header (group-chat only; single-agent shows nothing).
            if item.speakerId != nil {
                SpeakerHeader(speakerId: item.speakerId, scenario: scenario)
            }
            ThinkingCard(item: item)
            if !item.steps.isEmpty {
                ToolStepsCard(steps: item.steps)
            }
            if !item.text.isEmpty {
                if item.streaming && !item.done {
                    // During streaming, render the partial text as plain Text —
                    // NOT MarkdownText. Re-parsing the growing markdown on every
                    // token (splitMarkdown + buildInline, O(n²) over the stream)
                    // floods the main thread and beachballs the app on long
                    // replies. The full markdown render lands once on `.done`.
                    Text(item.text)
                        .foregroundStyle(Theme.onBg)
                        .font(.body)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    MarkdownText(text: item.text)
                        .equatable()
                        .contextMenu {
                            Button("重新生成") { onRetry() }
                            Button("复制") { copyText(item.text) }
                            Button("分享") { shareText(item.text) }
                        }
                }
            }
            if item.streaming && !item.text.isEmpty {
                BlinkingCursor()
            }
            if let msg = item.error {
                HStack {
                    Text("✗ \(msg)").foregroundStyle(Theme.errorC).font(.caption)
                    Spacer()
                    Button("重试", action: onRetry).buttonStyle(.borderless)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        // Left accent bar in the speaker's color (group-chat only).
        .overlay(alignment: .leading) {
            if let sid = item.speakerId {
                let meta = AgentStore.speakerMeta(for: sid, in: scenario)
                Color(hex: meta.1).frame(width: 3).clipShape(RoundedRectangle(cornerRadius: 1.5))
            }
        }
    }
    private func copyText(_ s: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(s, forType: .string)
    }
    private func shareText(_ s: String) {
        guard let view = NSApp.keyWindow?.contentView else { return }
        let picker = NSSharingServicePicker(items: [s])
        picker.show(relativeTo: .zero, of: view, preferredEdge: .minY)
    }
}

private struct ThinkingCard: View {
    let item: AssistantItem
    var body: some View {
        if item.thinking.isEmpty { EmptyView() }
        else {
            // Collapsed by default — don't stream the raw reasoning text into
            // the bubble (it's the model's internal chain-of-thought, often
            // starting "The user…"; showing it expanded looked like a glitch).
            // Show "思考中…" + dots while active, "已深度思考" + chevron after,
            // expand on click.
            let expanded = item.thinkingExpanded
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Image(systemName: "brain.head.profile").foregroundStyle(Theme.primary)
                    Text(item.thinkingActive ? "思考中…" : "已深度思考")
                        .foregroundStyle(Theme.onSurfaceVar).font(.caption)
                    if item.thinkingActive {
                        ThreeDots()
                    } else {
                        Spacer()
                        Button {
                            item.thinkingExpanded.toggle()
                        } label: {
                            Image(systemName: item.thinkingExpanded ? "chevron.down" : "chevron.right")
                                .foregroundStyle(Theme.onSurfaceVar)
                        }
                        .buttonStyle(.plain)
                    }
                }
                if expanded {
                    ScrollView { Text(item.thinking)
                            .foregroundStyle(Theme.onSurfaceVar).font(.caption)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .textSelection(.enabled)
                    }
                    .frame(maxHeight: 260)
                }
            }
            .padding(10)
            .background(Theme.secondaryCont)
            .clipShape(RoundedRectangle(cornerRadius: 10))
        }
    }
}

private struct ToolStepsCard: View {
    let steps: [ToolStep]
    @State private var expanded: Bool = true
    var body: some View {
        let ok = steps.filter { $0.ok == true }.count
        let fail = steps.filter { $0.ok == false }.count
        let pending = steps.filter { $0.ok == nil }.count
        VStack(alignment: .leading, spacing: 3) {
            Button { withAnimation { expanded.toggle() } } label: {
                HStack(spacing: 4) {
                    Image(systemName: expanded ? "chevron.down" : "chevron.right")
                        .font(.caption2).foregroundStyle(Theme.onSurfaceVar)
                    Image(systemName: "wrench.and.screwdriver")
                        .font(.caption2).foregroundStyle(Theme.primary)
                    Text("调用了 \(steps.count) 个工具")
                        .font(.caption).foregroundStyle(Theme.onSurfaceVar)
                    if ok > 0 { Text("✓\(ok)").font(.caption2).foregroundStyle(Theme.tertiary) }
                    if fail > 0 { Text("✗\(fail)").font(.caption2).foregroundStyle(Theme.errorC) }
                    if pending > 0 { ThreeDots() }
                }
            }.buttonStyle(.plain)
            if expanded {
                VStack(alignment: .leading, spacing: 2) {
                    ForEach(steps) { StepLine(step: $0) }
                }
                .padding(.leading, 12)
            }
        }
        .padding(8)
        .background(Theme.secondaryCont)
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }
}

private struct StepLine: View {
    let step: ToolStep
    @State private var expanded: Bool = false
    var body: some View {
        let (icon, color) = step.ok == true ? ("checkmark", Theme.tertiary)
                          : step.ok == false ? ("xmark", Theme.errorC)
                          : ("gearshape", Theme.onSurfaceVar)
        VStack(alignment: .leading, spacing: 1) {
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                Image(systemName: icon).foregroundStyle(color).font(.caption2)
                Text("\(step.name)(\(step.args))")
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(color)
                    .lineLimit(2)
            }
            if let r = step.result {
                Text("└ \(r)")
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(Theme.onSurfaceVar)
                    .lineLimit(expanded ? nil : 3)
                    .padding(.leading, 12)
            }
        }
        .contentShape(Rectangle())
        .onTapGesture { withAnimation { expanded.toggle() } }
    }
}

// MARK: - Markdown

private struct MarkdownText: View, Equatable {
    let text: String
    @State private var copied: Bool = false
    // Equatable so `.equatable()` skips re-parsing the markdown of unchanged
    // bubbles. Without this, every streamTick flush re-evaluates EVERY visible
    // AssistantBubble (AssistantItem is a plain class SwiftUI can't short-circuit)
    // → every MarkdownText re-runs splitMarkdown/buildInline → O(N×parse) per
    // flush → main thread drowns → beachball on long conversations. With this,
    // only the bubble whose `text` actually changed re-parses.
    static func == (lhs: MarkdownText, rhs: MarkdownText) -> Bool { lhs.text == rhs.text }
    var body: some View {
        let blocks = splitMarkdown(text)
        return VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                switch block {
                case .heading(let level, let body):
                    Text(buildInline(body, codeBg: Theme.surfaceVar))
                        .font(headingFont(level))
                        .foregroundStyle(Theme.onBg)
                        .textSelection(.enabled)
                case .paragraph(let body):
                    Text(buildInline(body, codeBg: Theme.surfaceVar))
                        .foregroundStyle(Theme.onBg)
                        .font(.body)
                        .textSelection(.enabled)
                case .blockquote(let body):
                    HStack(alignment: .top, spacing: 8) {
                        Rectangle().fill(Theme.primary.opacity(0.5)).frame(width: 3)
                        Text(buildInline(body, codeBg: Theme.surfaceVar))
                            .font(.body.italic())
                            .foregroundStyle(Theme.onSurfaceVar)
                            .textSelection(.enabled)
                    }
                case .bulletList(let items):
                    VStack(alignment: .leading, spacing: 3) {
                        ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                            HStack(alignment: .firstTextBaseline, spacing: 6) {
                                Text("•")
                                Text(buildInline(item, codeBg: Theme.surfaceVar))
                                    .foregroundStyle(Theme.onBg).font(.body).textSelection(.enabled)
                            }
                        }
                    }
                case .orderedList(let items):
                    VStack(alignment: .leading, spacing: 3) {
                        ForEach(Array(items.enumerated()), id: \.offset) { idx, item in
                            HStack(alignment: .firstTextBaseline, spacing: 6) {
                                Text("\(idx + 1).")
                                Text(buildInline(item, codeBg: Theme.surfaceVar))
                                    .foregroundStyle(Theme.onBg).font(.body).textSelection(.enabled)
                            }
                        }
                    }
                case .table(let header, let rows):
                    MarkdownTable(header: header, rows: rows)
                case .code(let lang, let code):
                    CodeCard(lang: lang, code: code)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func headingFont(_ level: Int) -> Font {
        switch level {
        case 1: return .title2.bold()
        case 2: return .title3.bold()
        case 3: return .headline
        default: return .subheadline.bold()
        }
    }
}

private struct MarkdownTable: View {
    let header: [String]
    let rows: [[String]]
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top, spacing: 0) {
                ForEach(Array(header.enumerated()), id: \.offset) { _, cell in
                    Text(buildInline(cell, codeBg: Theme.surfaceVar))
                        .font(.subheadline.bold()).foregroundStyle(Theme.onBg)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(6)
                }
            }
            .background(Theme.surfaceVar)
            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                HStack(alignment: .top, spacing: 0) {
                    ForEach(Array(row.enumerated()), id: \.offset) { _, cell in
                        Text(buildInline(cell, codeBg: Theme.surfaceVar))
                            .font(.subheadline).foregroundStyle(Theme.onBg)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(6)
                    }
                }
                Divider()
            }
        }
        .overlay(RoundedRectangle(cornerRadius: 6).stroke(Theme.surfaceVar, lineWidth: 1))
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }
}

private struct CodeCard: View {
    let lang: String
    let code: String
    @EnvironmentObject var artifacts: ArtifactStore
    @State private var copied: Bool = false
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                if !lang.isEmpty {
                    Text(lang).font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(Theme.onSurfaceVar)
                }
                Spacer()
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(code, forType: .string)
                    copied = true
                    DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { copied = false }
                } label: {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .font(.caption2).foregroundStyle(Theme.onSurfaceVar)
                }.buttonStyle(.plain).help("复制代码")
                if code.count > 80 {
                    Button {
                        artifacts.open(Artifact(title: lang.isEmpty ? "代码" : lang,
                                                lang: lang, content: code))
                    } label: {
                        Image(systemName: "rectangle.split.3x1")
                            .font(.caption2).foregroundStyle(Theme.onSurfaceVar)
                    }
                    .buttonStyle(.plain)
                    .help("在画布打开")
                }
            }
            .padding(.horizontal, 10).padding(.vertical, 6)
            .background(Theme.surfaceVar)
            ScrollView(.horizontal) {
                Text(code).font(.system(size: 13, design: .monospaced))
                    .foregroundStyle(Theme.onBg)
                    .textSelection(.enabled)
                    .padding(.horizontal, 10).padding(.bottom, 10)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.surfaceVar.opacity(0.5))
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(Theme.surfaceVar, lineWidth: 1))
    }
}

// MARK: - Streaming cursors

private struct BlinkingCursor: View {
    @State private var on = true
    var body: some View {
        Text("▍").foregroundStyle(Theme.primary.opacity(on ? 1 : 0.2))
            .onAppear {
                withAnimation(.easeInOut(duration: 0.5).repeatForever(autoreverses: true)) { on = false }
            }
    }
}

private struct ThreeDots: View {
    @State private var phase = 0.0
    var body: some View {
        HStack(spacing: 2) {
            ForEach(0..<3) { i in
                Text("·").foregroundStyle(Theme.onSurfaceVar.opacity(alpha(for: i)))
            }
        }
        .onAppear {
            withAnimation(.linear(duration: 1.2).repeatForever(autoreverses: false)) { phase = 3 }
        }
    }
    private func alpha(for i: Int) -> Double {
        let p = (phase + Double(i)) .truncatingRemainder(dividingBy: 3) / 3
        return 0.3 + 0.7 * (1 - abs(2 * p - 1))
    }
}

// MARK: - First-run hint

private struct FirstRunHint: View {
    let onOpen: () -> Void
    var body: some View {
        Button(action: onOpen) {
            Text("未配置 API Key,点击设置 → 填入 kind / model / key 后保存")
                .foregroundStyle(Theme.onBg).font(.caption)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(Theme.primaryCont)
                .clipShape(RoundedRectangle(cornerRadius: 10))
        }
        .buttonStyle(.plain)
    }
}

// MARK: - Input bar

private struct InputBar: View {
    @Binding var value: String
    let running: Bool
    let onChange: (String) -> Void
    let onSend: () -> Void
    let onStop: () -> Void
    /// Live "can the input be sent right now?" (non-empty + not running). A
    /// closure (not a Bool) so the Return-key monitor reads current state.
    let canSend: () -> Bool
    @FocusState private var focused: Bool

    /// Reference holder so the NSEvent monitor (installed once) reads live
    /// focus state. Stored state lives across re-renders via @StateObject.
    @StateObject private var focus = InputFocusHolder()

    var body: some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextEditor(text: $value)
                .font(.body)
                .scrollContentBackground(.hidden)
                .background(Theme.surfaceVar)
                .padding(.horizontal, 8).padding(.vertical, 4)
                .clipShape(RoundedRectangle(cornerRadius: 18))
                .frame(minHeight: 40, maxHeight: 100)
                .focused($focused)
                .onChange(of: value) { onChange($0) }
            if running {
                Button(action: onStop) {
                    Image(systemName: "stop.fill").font(.title3)
                        .foregroundStyle(.white)
                        .frame(width: 36, height: 36)
                        .background(Theme.errorC)
                        .clipShape(Circle())
                }
                .buttonStyle(.plain)
            } else {
                Button(action: onSend) {
                    Image(systemName: "paperplane.fill").font(.title3)
                        .foregroundStyle(.white)
                        .frame(width: 36, height: 36)
                        .background(value.isEmpty ? Theme.surfaceVar : Theme.primary)
                        .clipShape(Circle())
                }
                .buttonStyle(.plain)
                .keyboardShortcut(.return, modifiers: .command)
                .disabled(value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(8)
        .background(Theme.surface)
        .onChange(of: focused) { focus.focused = $0 }
        // ⏎ Return sends; ⇧⏎ inserts a newline. (macOS 13 TextEditor doesn't
        // fire onSubmit on plain Return, and a Button `.return` shortcut is
        // pre-empted by the field editor — so we intercept at the key-event
        // level, scoped to when the input is focused.)
        .onAppear {
            focus.canSend = canSend
            focus.send = onSend
            if focus.monitor == nil {
                focus.monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [focus] event in
                    // 36 = Return, 76 = keypad Enter.
                    let isReturn = event.keyCode == 36 || event.keyCode == 76
                    let mods = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
                    let bare = mods.isEmpty || mods == .capsLock
                    guard isReturn, bare, focus.focused, focus.canSend() else { return event }
                    DispatchQueue.main.async { focus.send() }
                    return nil   // swallow so no newline is inserted
                }
            }
        }
        .onDisappear {
            if let m = focus.monitor { NSEvent.removeMonitor(m); focus.monitor = nil }
        }
    }
}

/// Holds live focus state + the install-once Return-key monitor, so the
/// monitor (created in `onAppear`) reads current values instead of stale
/// captures.
private final class InputFocusHolder: ObservableObject {
    var focused: Bool = false
    var canSend: () -> Bool = { false }
    var send: () -> Void = {}
    var monitor: Any?
}

// MARK: - Settings sheet

private struct SettingsSheet: View {
    @ObservedObject var vm: ChatViewModel
    let onClose: () -> Void
    private let kinds = ["openai", "anthropic", "ollama"]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Provider 设置").font(.headline)
            Picker("kind", selection: Binding(
                get: { vm.kind },
                set: { vm.applyProviderPreset($0) })) {
                ForEach(kinds, id: \.self) { Text($0).tag($0) }
            }
            .pickerStyle(.menu)
            TextField("model", text: $vm.model).textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            SecureField("api key (openai / anthropic)", text: $vm.apiKey).textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            TextField("base url override (blank = 默认; ollama → host:port)", text: $vm.baseUrl)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            Text("ollama 示例:kind=ollama, model=llama3, base url=127.0.0.1:11434。保存后重建 App(历史保留)。")
                .font(.caption2).foregroundStyle(Theme.onSurfaceVar)
            HStack {
                Spacer()
                Button("保存") {
                    Task {
                        vm.saveConfig()
                        await vm.rebuildApp()
                        onClose()
                    }
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .frame(width: 460)
        .padding(16)
    }
}
