// SwiftUI views — macOS port of the Android Compose chat UI.
// NavigationSplitView (sidebar = session list) + detail (chat). Settings,
// delete-confirm, first-run hint, scroll-to-bottom, copy/share, retry all
// reproduced. Dark theme follows the system via the adaptive Theme palette.

import SwiftUI
import AppKit

// MARK: - 3D pixel-block brand logo

/// The OneAI wordmark as extruded 3D pixel tiles — the macOS counterpart of the
/// TUI's colorful block-art brand (examples/cli/src/tui/render/brand.rs). The
/// TUI paints flat background-colored cells; here each filled cell is a raised
/// tile (gradient face + offset depth side + shadow) so the logo reads as
/// dimensional blocks rather than flat letters. Same 5×7 per-character bitmap
/// and the same per-character gradient hues (`Brand.charColors`) so the brand
/// stays consistent across surfaces.
struct BrandLogo: View {
    /// Edge length of one pixel tile.
    var cell: CGFloat = 5
    /// Gap between tiles (and between rows).
    var gap: CGFloat = 1
    /// Extrusion depth — how far the side face drops below the front face.
    var depth: CGFloat = 1

    /// 5 chars × 5 rows × 7 cols. Each char's leading column is empty, giving
    /// natural intra-word spacing (mirrors the TUI pattern verbatim).
    private static let patterns: [[[Bool]]] = [
        // O
        [[false,true, true, true, true, true, true ],
         [false,true, true, false,false,true, true ],
         [false,true, true, false,false,true, true ],
         [false,true, true, false,false,true, true ],
         [false,true, true, true, true, true, true ]],
        // n
        [[false,true, true, true, true, true, false],
         [false,true, true, false,true, true, false],
         [false,true, true, false,true, true, false],
         [false,true, true, false,true, true, false],
         [false,true, true, false,true, true, false]],
        // e
        [[false,true, true, true, true, true, true ],
         [false,true, true, false,false,false,false],
         [false,true, true, true, true, false,false],
         [false,true, true, false,false,false,false],
         [false,true, true, true, true, true, true ]],
        // A
        [[false,false,false,true, true, false,false],
         [false,false,true, true, true, true, false],
         [false,true, true, true, true, true, true ],
         [false,true, true, false,false,true, true ],
         [false,true, true, false,false,true, true ]],
        // I
        [[false,true, true, true, true, true, true ],
         [false,false,false,true, true, false,false],
         [false,false,false,true, true, false,false],
         [false,false,false,true, true, false,false],
         [false,true, true, true, true, true, true ]],
    ]

    var body: some View {
        let r = cell * 0.22
        VStack(spacing: gap) {
            ForEach(0..<5, id: \.self) { row in
                HStack(spacing: 0) {
                    ForEach(0..<5, id: \.self) { ch in
                        HStack(spacing: gap) {
                            ForEach(0..<7, id: \.self) { col in
                                if Self.patterns[ch][row][col] {
                                    tile(charIdx: ch, radius: r)
                                } else {
                                    Color.clear.frame(width: cell, height: cell)
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// One raised pixel: a darker "side" face offset below the front gradient
    /// face, plus a thin top highlight and a soft shadow. The two-layer ZStack
    /// is what turns a flat square into an extruded 3D block.
    private func tile(charIdx: Int, radius: CGFloat) -> some View {
        let base = Brand.color(charIdx)
        let darker = base.opacity(0.55)
        let lighter = Color.white.opacity(0.35)
        return ZStack(alignment: .top) {
            // extrusion side — sits behind/below the front face
            RoundedRectangle(cornerRadius: radius)
                .fill(darker)
                .frame(width: cell, height: cell)
                .offset(y: depth)
            // front face with a top-lit gradient
            RoundedRectangle(cornerRadius: radius)
                .fill(LinearGradient(colors: [base.mixedLight(), base, darker],
                                     startPoint: .top, endPoint: .bottom))
                .frame(width: cell, height: cell)
                .overlay(alignment: .top) {
                    RoundedRectangle(cornerRadius: radius)
                        .fill(lighter)
                        .frame(width: cell, height: cell * 0.45)
                        .opacity(0.5)
                }
        }
        .frame(width: cell, height: cell + depth)
        .shadow(color: base.opacity(0.35), radius: cell * 0.25, x: 0, y: depth)
    }
}

/// Mix a color toward white (used for the tile's top-lit gradient stop). SwiftUI
/// has no public `mix(with:)`; this is a tiny manual blend.
private extension Color {
    func mixedLight(_ amount: CGFloat = 0.35) -> Color {
        let ns = NSColor(self).usingColorSpace(.sRGB) ?? NSColor(self)
        let r = ns.redComponent + (1 - ns.redComponent) * amount
        let g = ns.greenComponent + (1 - ns.greenComponent) * amount
        let b = ns.blueComponent + (1 - ns.blueComponent) * amount
        return Color(NSColor(srgbRed: r, green: g, blue: b, alpha: 1))
    }
}

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
        // Extend into the (now hidden) title-bar region so the chat + sidebar
        // headers sit at the very top instead of below a large reserved gap.
        .ignoresSafeArea(.container, edges: .top)
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
            // Cold start → a fresh single-agent conversation (not the most
            // recent history). The empty chat renders the welcome screen; the
            // user's past sessions remain reachable from the sidebar.
            await vm.newConversation()
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
        var id: String {
            switch self {
            case .editScenario(let s): return "edit-\(s.id)"
            }
        }
    }
    @State private var sheet: SidebarSheet? = nil

    /// Start a scenario: scenarios with topic-intake fields route through the
    /// inline `pendingScenario` page (rendered in the chat detail) instead of a
    /// modal sheet; scenarios without fields start immediately.
    private func startScenario(_ sc: Scenario) {
        if !(sc.topicFields?.isEmpty ?? true) {
            vm.pendingScenario = sc
        } else {
            Task { await vm.newConversation(scenario: sc) }
        }
    }

    /// The scrollable scenario + recent-session list, headed by the
    /// full-width "新对话" action.
    private var sidebarList: some View {
        VStack(spacing: 0) {
            // Primary action at the top of the list — a fresh single-agent chat.
            // (Multi-agent chats start by tapping a scenario below.) Mirrors the
            // affordance Doubao/Kimi put at the top of their sidebar.
            Button {
                Task { await vm.newConversation() }
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: "plus.circle.fill")
                        .font(.oBody)
                    Text("新对话").font(.oBody.weight(.semibold))
                    Spacer()
                }
                .foregroundStyle(Theme.onBg)
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(Theme.primaryCont, in: RoundedRectangle(cornerRadius: 10))
            }
            .buttonStyle(.plain)
            .pointerCursor()
            .padding(.horizontal, 10).padding(.top, 8).padding(.bottom, 4)

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
                              topicFields: nil, debrief: nil, reviewLoop: nil)
            sheet = .editScenario(sc)
        } label: { Image(systemName: "plus") }
    }

    private var sessionsSection: some View {
        SidebarSection(title: "最近会话") {
            if vm.sessions.isEmpty {
                Text("还没有会话\n点上方「新对话」开始吧")
                    .foregroundStyle(Theme.onSurfaceVar)
                    .font(.oFootnote)
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
            // Top: just traffic-light clearance — the "会话" title moved out;
            // the chat detail's top bar now carries the OneAI brand instead.
            HStack { Spacer() }
                .frame(height: 36)
                .padding(.leading, 76).padding(.trailing, 12)

            ScrollView {
                sidebarList
            }

            // Settings live at the sidebar bottom now (it used to sit at the
            // chat top bar's right edge — issue 3 moved it here so the top bar
            // is brand-only and the gear is always reachable from the drawer).
            Divider()
            HStack {
                Image(systemName: "gearshape")
                    .foregroundStyle(Theme.onSurfaceVar)
                Text("设置").font(.oCaption).foregroundStyle(Theme.onSurfaceVar)
                Spacer()
            }
            .padding(.horizontal, 16).padding(.vertical, 10)
            .background(Theme.surface)
            .contentShape(Rectangle())
            .onTapGesture { onOpenSettings() }
            .pointerCursor()
        }
        .background(Theme.surface)
        .sheet(item: $sheet) { presented in
            switch presented {
            case .editScenario(let sc):
                ScenarioEditor(scenario: sc, store: vm.agentStore,
                               onClose: { sheet = nil })
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
                Text(title).font(.oCaptionBold).foregroundStyle(Theme.onSurfaceVar)
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
    @State private var hovered = false
    var body: some View {
        Button(action: onTap) {
            HStack(spacing: 8) {
                Image(systemName: scenario.icon)
                    .foregroundStyle(Theme.primary)
                    .frame(width: 22)
                Text(scenario.name)
                    .font(.oSubheadline)
                    .fontWeight(isCurrent ? .semibold : .regular)
                    .lineLimit(1)
                Spacer()
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12).padding(.vertical, 5)
            .background(isCurrent ? Theme.primaryCont.opacity(0.5)
                       : (hovered ? Theme.secondaryCont : Color.clear))
            .clipShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .pointerCursor()
        .onHover { hovered = $0 }
        .padding(.horizontal, 6)
    }
}

/// Inline topic-intake page rendered in place of the conversation when
/// `vm.pendingScenario` is set (a flatter flow than the old modal sheet — the
/// intake lives where the conversation will live). Collects the scenario's
/// `topicFields`; the values are baked into each member's system prompt as
/// background and into the session title by `Scenario.specView`. Blank fields
/// are allowed — empty values are dropped.
struct TopicIntakeView: View {
    let scenario: Scenario
    @ObservedObject var vm: ChatViewModel
    @State private var values: [String: String] = [:]
    @FocusState private var focusedField: String?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                VStack(alignment: .leading, spacing: 8) {
                    HStack(spacing: 10) {
                        Image(systemName: scenario.icon)
                            .foregroundStyle(Theme.primary).font(.system(size: 30))
                        Text(scenario.name).font(.oTitle2).foregroundStyle(Theme.onBg)
                    }
                    if let desc = scenarioDescription {
                        Text(desc).font(.oSubheadline).foregroundStyle(Theme.onSurfaceVar)
                    }
                }
                .padding(.bottom, 2)

                ForEach(scenario.topicFields ?? []) { f in
                    VStack(alignment: .leading, spacing: 4) {
                        HStack(spacing: 6) {
                            Text(f.label).font(.oCaption).foregroundStyle(Theme.onSurfaceVar)
                            if let v = f.visibleTo, !v.isEmpty {
                                Text("· 仅 \(v.compactMap { scenario.agent($0)?.name }.joined(separator: "/")) 可见")
                                    .font(.oCaption2).foregroundStyle(Theme.tertiary)
                            }
                        }
                        TextField(f.placeholder ?? f.label, text: Binding(
                            get: { values[f.id] ?? "" },
                            set: { values[f.id] = $0 }
                        ), axis: .vertical)
                        .textFieldStyle(.roundedBorder)
                        .lineLimit(1...6)
                        .focused($focusedField, equals: f.id)
                        .onSubmit { start() }
                    }
                }
                Text("开场角色会围绕你输入的信息发言;这些值会作为各角色背景,并写入会话名。留空可直接开始。")
                    .font(.oCaption2).foregroundStyle(Theme.onSurfaceVar)
                HStack(spacing: 12) {
                    Button("取消", role: .cancel) { vm.cancelPendingScenario() }
                        .keyboardShortcut(.escape)
                    Spacer()
                    Button("开始") { start() }
                        .keyboardShortcut(.defaultAction)
                        .buttonStyle(.borderedProminent)
                }
            }
            .padding(28)
            .frame(maxWidth: 560)
            .frame(maxWidth: .infinity)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.background)
        .onAppear { focusedField = scenario.topicFields?.first?.id }
    }

    /// One-line description for known presets (purely UX — falls back to none).
    private var scenarioDescription: String? {
        switch scenario.id {
        case "preset-interview": return "填入岗位与项目信息,面试官提问、指导员据此点评。项目经历仅指导员可见。"
        case "preset-language-partner": return "指定语言与话题,陪练会按该语言自然对话并纠正你。"
        case "preset-debate": return "输入辩题,主持人开场后正反方轮流立论。"
        case "preset-writing-workshop": return "给定写作主题,写手起草、编辑审阅,直到定稿。"
        case "preset-brainstorm": return "给出主题,创意官发散、评审收敛。"
        default: return nil
        }
    }

    private func start() {
        let v = values
        Task { await vm.confirmStartScenario(topicValues: v) }
    }
}

private struct SessionRow: View {
    let info: SessionInfoView
    let isCurrent: Bool
    let onTap: () -> Void
    let onDelete: () -> Void
    @State private var hovered = false
    var body: some View {
        Button(action: onTap) {
            HStack(alignment: .center) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(info.title?.isEmpty == false ? info.title! : "新对话")
                        .font(.oSubheadline)
                        .fontWeight(isCurrent ? .semibold : .regular)
                        .lineLimit(1)
                    Text("\(info.messageCount) 条 · \(relativeTime(info.updatedAtMs))")
                        .font(.oCaption)
                        .foregroundStyle(Theme.onSurfaceVar)
                        .lineLimit(1)
                }
                Spacer()
                Button(action: onDelete) {
                    Image(systemName: "trash")
                        .foregroundStyle(Theme.onSurfaceVar)
                }
                .buttonStyle(.plain)
                .pointerCursor()
                .help("删除")
            }
            .padding(.horizontal, 12).padding(.vertical, 5)
            .background(isCurrent ? Theme.primaryCont.opacity(0.5)
                       : (hovered ? Theme.secondaryCont : Color.clear))
            .clipShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .pointerCursor()
        .onHover { hovered = $0 }
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

/// Bottom-of-content anchor's global maxY — used to detect how far the
/// content bottom sits below the viewport (→ user scrolled up).
private struct BottomAnchorKey: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}
/// ScrollView viewport's global maxY (its bottom edge).
private struct ViewportBottomKey: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}

private struct ChatDetail: View {
    @ObservedObject var vm: ChatViewModel
    @EnvironmentObject var artifacts: ArtifactStore
    let onOpenSettings: () -> Void
    @State private var stickToBottom = true
    /// ScrollView's bottom edge in global coordinates (updated by the
    /// viewport GeometryReader). Read by the bottom-anchor preference change
    /// handler to compute "how far is the content bottom below the viewport".
    @State private var viewportBottom: CGFloat = 0

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

    @ViewBuilder
    private var detailContent: some View {
        // Inline topic-intake page takes over the detail until confirmed/cancelled
        // — a flatter flow than a modal sheet (the intake lives where the
        // conversation will live).
        if let pending = vm.pendingScenario {
            TopicIntakeView(scenario: pending, vm: vm)
        } else {
            conversationContent
        }
    }

    private var conversationContent: some View {
        VStack(spacing: 0) {
            // Top bar
            HStack {
                if let sc = vm.currentScenario {
                    Image(systemName: sc.icon).foregroundStyle(Theme.primary)
                    Text(sc.name).font(.oTitle3).foregroundStyle(Theme.onBg)
                    if vm.debriefActive {
                        // Debrief phase indicator.
                        Text("· 总结阶段").font(.oCaption).foregroundStyle(Theme.onSurfaceVar)
                    } else if let debrief = sc.debrief {
                        // "结束面试" button — switches to the debrief member only.
                        Button {
                            Task { await vm.endScenarioDebrief() }
                        } label: {
                            Label(debrief.buttonLabel, systemImage: "checkmark.circle")
                                .font(.oCaption)
                        }
                        .buttonStyle(.bordered)
                        .pointerCursor()
                        .disabled(vm.running)
                        .help("结束并进入总结阶段")
                    }
                } else {
                    // Single-agent: the brand lives in the top bar now (the old
                    // "会话" sidebar title is gone). 3D pixel tiles match the TUI
                    // brand; the slogan sits beside it.
                    HStack(spacing: 10) {
                        BrandLogo(cell: 4.5, gap: 1, depth: 1)
                        Text("One AI, Every Platform")
                            .font(.oSubheadline).foregroundStyle(Theme.onSurfaceVar)
                    }
                }
                Spacer()
                if vm.lastTurnTokens > 0 {
                    Label("\(vm.lastTurnTokens) tok", systemImage: "flame")
                        .font(.oCaption).foregroundStyle(Theme.onSurfaceVar)
                        .help("本轮约 token 数")
                }
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

            // Empty conversation → welcome screen (like Doubao/Kimi's default
            // chat surface). Once the first message lands it disappears.
            if vm.items.isEmpty && !vm.running {
                WelcomeScreen(vm: vm, onOpenSettings: onOpenSettings)
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
                            .background(GeometryReader { g in
                                Color.clear.preference(key: BottomAnchorKey.self,
                                                       value: g.frame(in: .global).maxY)
                            })
                    }
                    .padding(12)
                }
                .background(GeometryReader { g in
                    Color.clear.preference(key: ViewportBottomKey.self,
                                           value: g.frame(in: .global).maxY)
                })
                // Smart stick-to-bottom: track the content bottom's distance
                // below the viewport. A real user scroll-up pushes it well
                // past 200pt → break following so they can read history in
                // peace. The hysteresis (false at >200, true at <80) plus the
                // high false-threshold keeps a single per-flush content growth
                // (a couple lines, well under 200pt) from tripping mid-stream
                // and re-yanking a streaming reply back to the bottom. The
                // scroll-to-bottom button offers a manual resume either way.
                .onPreferenceChange(BottomAnchorKey.self) { bottomY in
                    let dist = bottomY - viewportBottom
                    if dist > 200 { stickToBottom = false }
                    else if dist < 80 { stickToBottom = true }
                }
                .onPreferenceChange(ViewportBottomKey.self) { viewportBottom = $0 }
                .onChange(of: vm.streamTick) { _ in
                    // Non-animated snap. `withAnimation` here stacks ~20×/sec
                    // during streaming and forces extra layout passes, which
                    // visibly jitters the chat (the "上下晃动" + scrollbar
                    // twitch). When the user scrolled up, stickToBottom is
                    // false and this is a no-op.
                    if stickToBottom { proxy.scrollTo("bottom", anchor: .bottom) }
                }
                .onChange(of: vm.items.count) { _ in
                    if stickToBottom { proxy.scrollTo("bottom", anchor: .bottom) }
                }
                .overlay(alignment: .bottom) {
                    if !stickToBottom && !vm.items.isEmpty {
                        Button {
                            withAnimation { proxy.scrollTo("bottom", anchor: .bottom) }
                            stickToBottom = true
                        } label: {
                            Image(systemName: "arrow.down.circle.fill")
                                .font(.oTitle2)
                                .foregroundStyle(Theme.primary, Theme.surface)
                                .shadow(radius: 3)
                        }
                        .buttonStyle(.plain)
                        .pointerCursor()
                        .padding(.bottom, 8)
                        .help("回到底部")
                    }
                }
            }

            if let msg = vm.error {
                Text("✗ \(msg)").foregroundStyle(Theme.errorC).font(.oCaption)
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

// MARK: - Welcome screen (empty-conversation default surface)

/// Default chat surface when a conversation has no messages yet — the macOS
/// counterpart of Doubao/Kimi's landing screen. Tells the user what the app is
/// and what they can ask, and offers one-tap starter prompts. Disappears the
/// moment the first message lands (or a turn starts running).
private struct WelcomeScreen: View {
    @ObservedObject var vm: ChatViewModel
    let onOpenSettings: () -> Void

    private struct Suggestion: Identifiable { let id = UUID(); let icon: String; let text: String }

    private var suggestions: [Suggestion] {
        [
            .init(icon: "doc.text.magnifyingglass", text: "帮我总结一段笔记的核心要点"),
            .init(icon: "hammer", text: "用 Rust 写一个读取 JSON 的命令行小工具"),
            .init(icon: "globe", text: "解释一下 Agent 与 RAG 的区别"),
            .init(icon: "sparkles", text: "把这段话改写得更简洁专业"),
        ]
    }

    var body: some View {
        ScrollView {
            VStack(spacing: 16) {
                Color.clear.frame(height: 4)
                BrandLogo(cell: 8, gap: 1.5, depth: 1.5)
                VStack(spacing: 2) {
                    Text("One AI, Every Platform")
                        .font(.oSubheadline.weight(.semibold)).foregroundStyle(Theme.onBg)
                    Text("跨平台 AI Agent 框架 · 单 Agent 对话与多角色场景都在这里")
                        .font(.oCaption).foregroundStyle(Theme.onSurfaceVar)
                }
                if vm.needsKeyConfig {
                    Button(action: onOpenSettings) {
                        Label("先配置 Provider 再开始", systemImage: "key.fill")
                            .font(.oCaption)
                            .padding(.horizontal, 12).padding(.vertical, 6)
                            .background(Theme.primaryCont, in: Capsule())
                    }
                    .buttonStyle(.plain).pointerCursor()
                }
                // 2-column grid so all suggestions fit above the input bar
                // without scrolling on a default-size window.
                LazyVGrid(columns: [GridItem(.flexible(), spacing: 8),
                                    GridItem(.flexible(), spacing: 8)], spacing: 8) {
                    ForEach(suggestions) { s in
                        Button {
                            let t = s.text
                            vm.input = ""
                            Task { await vm.runTask(t) }
                        } label: {
                            HStack(spacing: 6) {
                                Image(systemName: s.icon).foregroundStyle(Theme.primary)
                                Text(s.text).font(.oFootnote).foregroundStyle(Theme.onBg)
                                    .lineLimit(2)
                                Spacer()
                            }
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.horizontal, 10).padding(.vertical, 9)
                            .background(Theme.secondaryCont, in: RoundedRectangle(cornerRadius: 10))
                        }
                        .buttonStyle(.plain).pointerCursor()
                    }
                }
                .padding(.horizontal, 4)
            }
            .padding(.horizontal, 28).padding(.bottom, 12)
            .frame(maxWidth: 560)
            .frame(maxWidth: .infinity)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
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
            Text(text).font(.oBody).foregroundStyle(Theme.onBg)
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
                Text("编辑消息").font(.oHeadline)
                TextEditor(text: $draft)
                    .font(.oBody).scrollContentBackground(.hidden)
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
                .font(.oSubheadlineBold)
                .foregroundStyle(Color(hex: meta.1))
            if let a = scenario?.agent(speakerId ?? "") {
                Text(a.role)
                    .font(.oCaption2)
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
        HStack(spacing: 6) {
            if vm.running, let sid = vm.activeSpeakerId {
                let meta = AgentStore.speakerMeta(for: sid, in: vm.currentScenario)
                Image(systemName: meta.2).foregroundStyle(Color(hex: meta.1))
                Text("\(meta.0) 正在发言").font(.oCaption).foregroundStyle(Theme.onSurfaceVar)
                ThreeDots()
            } else {
                Image(systemName: "hand.raised").foregroundStyle(Theme.onSurfaceVar)
                Text("轮到你 — 发送你的回答").font(.oCaption).foregroundStyle(Theme.onSurfaceVar)
            }
            Spacer()
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
                    //
                    // Cap the displayed length: a plain `Text` re-lays-out its
                    // ENTIRE content every flush (Core Text shapes/wraps the
                    // whole growing string). For a long CJK reply the layout
                    // cost grows past the flush interval and the main thread
                    // saturates → persistent beachball mid-stream. Showing only
                    // the tail keeps layout O(cap); the full text renders once
                    // on completion.
                    //
                    // Inline steady caret "▍" appended to the SAME Text (not a
                    // separate row): a separate BlinkingCursor row read as an
                    // extra blank line + flicker. One flowable Text with a
                    // steady caret at the tail avoids both. Trailing whitespace
                    // trimmed so a partial trailing newline doesn't open an
                    // empty line mid-stream.
                    Text(Self.streamingDisplay(of: item.text)
                            .trimmingCharacters(in: .whitespacesAndNewlines) + "▍")
                        .foregroundStyle(Theme.onBg)
                        .font(.oBody)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    MarkdownText(text: item.text.trimmingCharacters(in: .whitespacesAndNewlines))
                        .equatable()
                        .contextMenu {
                            Button("重新生成") { onRetry() }
                            Button("复制") { copyText(item.text) }
                            Button("分享") { shareText(item.text) }
                        }
                }
            }
            if let msg = item.error {
                HStack {
                    Text("✗ \(msg)").foregroundStyle(Theme.errorC).font(.oCaption)
                    Spacer()
                    Button("重试", action: onRetry).buttonStyle(.borderless)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        // Pad the content off the left accent bar so the speaker-colored bar
        // (group-chat only) doesn't overlap the header/text — it sat at the
        // very leading edge, directly under the first ~3px of content.
        .padding(.leading, 12)
        // Left accent bar in the speaker's color (group-chat only).
        .overlay(alignment: .leading) {
            if let sid = item.speakerId {
                let meta = AgentStore.speakerMeta(for: sid, in: scenario)
                Color(hex: meta.1).frame(width: 3).clipShape(RoundedRectangle(cornerRadius: 1.5))
            }
        }
        // Extra top gap for group-chat speaker bubbles so consecutive roles
        // (e.g. 指导员 → 面试官) read as distinct turns rather than one block.
        .padding(.top, item.speakerId != nil ? 8 : 0)
    }

    /// The text to render while streaming, capped to the last `cap` characters.
    /// A plain `Text` re-lays-out its whole content every flush; capping bounds
    /// the Core Text work so a long reply doesn't saturate the main thread
    /// mid-stream. The full text renders once on completion (MarkdownText).
    /// No "…" prefix: it only appeared once the text crossed the cap, causing a
    /// one-line layout jump mid-stream (the chat "晃动"). Showing the bare tail
    /// keeps the row count stable across the cap boundary.
    static let streamingCap = 1800
    static func streamingDisplay(of text: String) -> String {
        if text.count <= streamingCap { return text }
        return String(text.suffix(streamingCap))
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
            // Show the first 1-2 lines of the model's reasoning at all times —
            // collapsed (default) or expanded. Previously the card showed only
            // "思考中…" dots while active and nothing after, so during the long
            // wait before the first answer token the chat looked frozen even
            // though reasoning was streaming in. Surfacing the leading lines
            // (and a caret while active) gives visible progress; the rest stays
            // collapsed behind a chevron until the user expands it.
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 6) {
                    Image(systemName: "brain.head.profile").foregroundStyle(Theme.primary)
                    Text(item.thinkingActive ? "思考中" : "已深度思考")
                        .foregroundStyle(Theme.onSurfaceVar).font(.oCaption)
                    if item.thinkingActive { ThreeDots() }
                    Spacer()
                    Button {
                        item.thinkingExpanded.toggle()
                    } label: {
                        Image(systemName: item.thinkingExpanded ? "chevron.down" : "chevron.right")
                            .foregroundStyle(Theme.onSurfaceVar)
                    }
                    .buttonStyle(.plain)
                    .pointerCursor()
                }
                if item.thinkingExpanded {
                    ScrollView {
                        Text(item.thinking)
                            .foregroundStyle(Theme.onSurfaceVar).font(.oCaption)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .textSelection(.enabled)
                    }
                    .frame(maxHeight: 260)
                } else {
                    // Collapsed preview: first 1-2 lines. A steady caret while
                    // active signals "still thinking" without the layout jump a
                    // separate cursor row caused. Leading whitespace trimmed so a
                    // partial first line doesn't render as an indent.
                    Text(Self.preview(of: item.thinking, active: item.thinkingActive))
                        .foregroundStyle(Theme.onSurfaceVar).font(.oCaption)
                        .lineLimit(2)
                        .truncationMode(.tail)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .textSelection(.enabled)
                }
            }
            .padding(10)
            .background(Theme.secondaryCont)
            .clipShape(RoundedRectangle(cornerRadius: 10))
        }
    }

    /// The collapsed-preview string: the first two lines of `thinking`, with a
    /// caret appended while the model is still producing reasoning.
    private static func preview(of thinking: String, active: Bool) -> String {
        var lines = thinking.split(separator: "\n", omittingEmptySubsequences: false)
        // Drop leading blank lines (a model often opens with one) so the first
        // real line is what the user sees.
        while lines.first == "" { lines.removeFirst() }
        let head = lines.prefix(2).joined(separator: "\n")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return active ? head + "▍" : head
    }
}

private struct ToolStepsCard: View {
    let steps: [ToolStep]
    @State private var expanded: Bool = false
    var body: some View {
        let ok = steps.filter { $0.ok == true }.count
        let fail = steps.filter { $0.ok == false }.count
        let pending = steps.filter { $0.ok == nil }.count
        VStack(alignment: .leading, spacing: 3) {
            Button { withAnimation { expanded.toggle() } } label: {
                HStack(spacing: 4) {
                    Image(systemName: expanded ? "chevron.down" : "chevron.right")
                        .font(.oCaption2).foregroundStyle(Theme.onSurfaceVar)
                    Image(systemName: "wrench.and.screwdriver")
                        .font(.oCaption2).foregroundStyle(Theme.primary)
                    Text("调用了 \(steps.count) 个工具")
                        .font(.oCaption).foregroundStyle(Theme.onSurfaceVar)
                    if ok > 0 { Text("✓\(ok)").font(.oCaption2).foregroundStyle(Theme.tertiary) }
                    if fail > 0 { Text("✗\(fail)").font(.oCaption2).foregroundStyle(Theme.errorC) }
                    if pending > 0 { ThreeDots() }
                }
            }.buttonStyle(.plain).pointerCursor()
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
                Image(systemName: icon).foregroundStyle(color).font(.oCaption2)
                // Collapsed: just the tool name (clean). Expanded: name + args.
                Text(expanded && !step.args.isEmpty ? "\(step.name)(\(step.args))" : step.name)
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
                        .font(.oBody)
                        .textSelection(.enabled)
                case .blockquote(let body):
                    HStack(alignment: .top, spacing: 8) {
                        Rectangle().fill(Theme.primary.opacity(0.5)).frame(width: 3)
                        Text(buildInline(body, codeBg: Theme.surfaceVar))
                            .font(.oBodyItalic)
                            .foregroundStyle(Theme.onSurfaceVar)
                            .textSelection(.enabled)
                    }
                case .bulletList(let items):
                    VStack(alignment: .leading, spacing: 3) {
                        ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                            HStack(alignment: .firstTextBaseline, spacing: 6) {
                                Text("•")
                                Text(buildInline(item, codeBg: Theme.surfaceVar))
                                    .foregroundStyle(Theme.onBg).font(.oBody).textSelection(.enabled)
                            }
                        }
                    }
                case .orderedList(let items):
                    VStack(alignment: .leading, spacing: 3) {
                        ForEach(Array(items.enumerated()), id: \.offset) { idx, item in
                            HStack(alignment: .firstTextBaseline, spacing: 6) {
                                Text("\(idx + 1).")
                                Text(buildInline(item, codeBg: Theme.surfaceVar))
                                    .foregroundStyle(Theme.onBg).font(.oBody).textSelection(.enabled)
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
                        .font(.oSubheadlineBold).foregroundStyle(Theme.onBg)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(6)
                }
            }
            .background(Theme.surfaceVar)
            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                HStack(alignment: .top, spacing: 0) {
                    ForEach(Array(row.enumerated()), id: \.offset) { _, cell in
                        Text(buildInline(cell, codeBg: Theme.surfaceVar))
                            .font(.oSubheadline).foregroundStyle(Theme.onBg)
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
                        .font(.oCaption2).foregroundStyle(Theme.onSurfaceVar)
                }.buttonStyle(.plain).pointerCursor().help("复制代码")
                if code.count > 80 {
                    Button {
                        artifacts.open(Artifact(title: lang.isEmpty ? "代码" : lang,
                                                lang: lang, content: code))
                    } label: {
                        Image(systemName: "rectangle.split.3x1")
                            .font(.oCaption2).foregroundStyle(Theme.onSurfaceVar)
                    }
                    .buttonStyle(.plain)
                    .pointerCursor()
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
            Text("未配置 API Key,点击设置 → 填入 base url / api key / model 后保存")
                .foregroundStyle(Theme.onBg).font(.oCaption)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(Theme.primaryCont)
                .clipShape(RoundedRectangle(cornerRadius: 10))
        }
        .buttonStyle(.plain)
        .pointerCursor()
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
    /// Voice dictation. The mic button starts/stops it; recognized text fills
    /// the field (prefixed with whatever the user had already typed).
    @StateObject private var speech = SpeechRecognizer()
    /// Text present when dictation started — preserved as a prefix so dictating
    /// appends to, not replaces, any existing draft.
    @State private var dictationPrefix: String = ""

    var body: some View {
        HStack(alignment: .bottom, spacing: 8) {
            // Mic button: toggle dictation. While running it's red + shows a
            // waveform; recognized text flows into the field via the transcript
            // onChange below.
            Button {
                if speech.isRunning {
                    speech.stop()
                    dictationPrefix = ""
                    focused = true
                } else {
                    let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
                    dictationPrefix = trimmed.isEmpty ? "" : value
                    speech.start()
                }
            } label: {
                Image(systemName: speech.isRunning ? "waveform.circle.fill" : "mic.fill")
                    .font(.title3)
                    .foregroundStyle(speech.isRunning ? .white : Theme.onSurfaceVar)
                    .frame(width: 36, height: 36)
                    .background(speech.isRunning ? Theme.errorC : Theme.surfaceVar)
                    .clipShape(Circle())
            }
            .buttonStyle(.plain)
            .pointerCursor()
            .disabled(running || !speech.available)
            .help(speech.available ? "语音输入(点击说话,再点结束)" : "语音识别不可用(检查权限/系统设置)")
            TextEditor(text: $value)
                .font(.oBody)
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
                .pointerCursor()
            } else {
                Button(action: onSend) {
                    Image(systemName: "paperplane.fill").font(.title3)
                        .foregroundStyle(.white)
                        .frame(width: 36, height: 36)
                        .background(value.isEmpty ? Theme.surfaceVar : Theme.primary)
                        .clipShape(Circle())
                }
                .buttonStyle(.plain)
                .pointerCursor()
                .keyboardShortcut(.return, modifiers: .command)
                .disabled(value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(8)
        .background(Theme.surface)
        .onChange(of: focused) { focus.focused = $0 }
        // Fold the live dictation transcript into the field, preserving the
        // pre-dictation prefix.
        .onChange(of: speech.transcript) { t in
            let sep = dictationPrefix.isEmpty || t.isEmpty ? "" : " "
            value = dictationPrefix + sep + t
        }
        .onChange(of: speech.isRunning) { running in
            // When dictation stops, reclaim focus so Return-to-send works.
            if !running { focused = true }
        }
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

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Provider 设置").font(.oHeadline)
            TextField("base url (如 https://api.openai.com/v1;留空=默认)", text: $vm.baseUrl)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            SecureField("api key (openai / anthropic)", text: $vm.apiKey).textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            TextField("model (如 gpt-4o-mini / claude-sonnet-4-6 / llama3)", text: $vm.model)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            // The provider protocol (kind) is inferred from the base url so the
            // user never has to pick it: api.anthropic.com → anthropic,
            // :11434 / 含 ollama → ollama, everything else (incl. OpenAI-compat
            // relays) → openai. A blank base url + key → openai default.
            Text("协议按 base url 自动识别：含 anthropic → anthropic；含 ollama 或 :11434 → ollama；其余按 openai 兼容。留空 base url 走各协议默认端点。保存后重建 App(历史保留)。")
                .font(.oCaption2).foregroundStyle(Theme.onSurfaceVar)

            Divider().padding(.vertical, 4)

            Text("Embedding 设置(记忆语义召回;默认 auto,通常无需改动)").font(.oHeadline)
            Picker("provider", selection: $vm.embProvider) {
                ForEach(["auto", "openai", "voyage", "ollama", "fastembed", "openai-compat"], id: \.self) {
                    Text($0).tag($0)
                }
            }
            .pickerStyle(.menu)
            TextField("model (空 = provider 默认)", text: $vm.embModel)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            SecureField("embedding api key (VOYAGE_API_KEY / OPENAI_API_KEY)", text: $vm.embApiKey)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            TextField("base url (openai-compat 必填;ollama → host:port)", text: $vm.embBaseUrl)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            Text("auto 探测链:openai-compat → voyage → openai → ollama → fastembed;全无可用时降级为关键词召回。embedding key 与主模型 key 相互独立。")
                .font(.oCaption2).foregroundStyle(Theme.onSurfaceVar)

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
