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
struct BrandLogo: View, Equatable {
    /// Edge length of one pixel tile (width).
    var cell: CGFloat = 5
    /// Gap between tiles (and between rows).
    var gap: CGFloat = 1
    /// Extrusion depth — how far the side face drops below the front face.
    var depth: CGFloat = 1
    /// Tile height / width. The per-character bitmap is 7 cols × 5 rows, so
    /// square tiles make each letter wider than tall (the wordmark reads flat
    /// / squished). 1.4 compensates: 5 × 1.4 = 7, so a char's bounding box
    /// becomes square and the letters stand up instead of stretching wide.
    var aspect: CGFloat = 1.4

    private var h: CGFloat { cell * aspect }

    /// Per-char precomputed tile palette: top-lit face gradient, extrusion
    /// side color, and shadow color. Computing these in `tile`'s body meant
    /// every render re-parsed the hex string (`Color(hex:)` → `Scanner`),
    /// re-ran `mixedLight()` (an `NSColor` color-space conversion) and rebuilt
    /// the `LinearGradient` — for ~85 filled tiles across 5 chars. The top-bar
    /// logo re-evaluates its body on every `streamTick` flush (≈20 fps while
    /// streaming) and on every window resize, so that per-tile work dominated
    /// those passes. Precomputing once turns a render into pure shape work.
    private struct TilePalette {
        let face: LinearGradient
        let side: Color
        let shadow: Color
    }
    private static let palettes: [TilePalette] = Brand.charColors.indices.map { i in
        let base = Color(hex: String(Brand.charColors[i], radix: 16))
        let darker = base.opacity(0.55)
        return TilePalette(
            face: LinearGradient(colors: [base.mixedLight(), base, darker],
                                 startPoint: .top, endPoint: .bottom),
            side: darker,
            shadow: base.opacity(0.35)
        )
    }

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
        [[false,true, true, true, true, true, true ],
         [false,true, true, false,false,true, true ],
         [false,true, true, false,false,true, true ],
         [false,true, true, false,false,true, true ],
         [false,true, true, false,false,true, true ]],
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
                                    Color.clear.frame(width: cell, height: h)
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Params are constant at each call site, so equality holds across the
    // parent's streamTick/resize re-renders → `.equatable()` skips `body`
    // entirely after the first render. That's the win: 0 tile work per flush.
    static func == (lhs: BrandLogo, rhs: BrandLogo) -> Bool {
        lhs.cell == rhs.cell && lhs.gap == rhs.gap
            && lhs.depth == rhs.depth && lhs.aspect == rhs.aspect
    }

    /// One raised pixel: a darker "side" face offset below the front gradient
    /// face, plus a thin top highlight and a soft shadow. The two-layer ZStack
    /// is what turns a flat square into an extruded 3D block.
    private func tile(charIdx: Int, radius: CGFloat) -> some View {
        let p = Self.palettes[charIdx]
        return ZStack(alignment: .top) {
            // extrusion side — sits behind/below the front face
            RoundedRectangle(cornerRadius: radius)
                .fill(p.side)
                .frame(width: cell, height: h)
                .offset(y: depth)
            // front face with a top-lit gradient (precomputed per char)
            RoundedRectangle(cornerRadius: radius)
                .fill(p.face)
                .frame(width: cell, height: h)
                .overlay(alignment: .top) {
                    RoundedRectangle(cornerRadius: radius)
                        .fill(Color.white.opacity(0.35))
                        .frame(width: cell, height: h * 0.45)
                        .opacity(0.5)
                }
        }
        .frame(width: cell, height: h + depth)
        .shadow(color: p.shadow, radius: cell * 0.25, x: 0, y: depth)
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

    var body: some View {
        // In-page overlay layer sits ABOVE the split view — dialogs (settings /
        // scenario editor / edit-message / ⌘K palette / delete-confirm) render
        // here instead of as native .sheet/.alert. Native sheets rebuild their
        // content tree on every open (heavy editors stuttered); an in-page
        // layer is a state flip + cheap opacity transition, and it stays in
        // the view hierarchy. Mirrors the `pendingScenario` topic-intake page.
        ZStack {
            NavigationSplitView {
                Sidebar(vm: vm, agentStore: vm.agentStore,
                        onOpenSettings: { vm.overlay = .settings },
                        onDelete: { vm.overlay = .deleteSession($0) })
                    .navigationSplitViewColumnWidth(min: 220, ideal: 260)
            } detail: {
                ChatDetail(vm: vm, streamTick: vm.streamTick,
                           onOpenSettings: { vm.overlay = .settings })
                    .environmentObject(artifacts)
            }
            .environmentObject(artifacts)
            // Extend into the (now hidden) title-bar region so the chat + sidebar
            // headers sit at the very top instead of below a large reserved gap.
            .ignoresSafeArea(.container, edges: .top)
            .background(
                // ⌘K opens the command palette.
                Button("") { vm.overlay = .commandPalette }
                    .keyboardShortcut("k", modifiers: .command)
                    .opacity(0)
            )
            .task {
                await vm.ensureApp()
                await vm.refreshSessions()
                // Cold start → a fresh single-agent conversation (not the most
                // recent history). The empty chat renders the welcome screen; the
                // user's past sessions remain reachable from the sidebar.
                await vm.newConversation()
            }

            if let ov = vm.overlay {
                OverlayLayer(overlay: ov, vm: vm)
                    .transition(.opacity)
            }
        }
        .animation(.easeInOut(duration: 0.15), value: vm.overlay == nil)
    }
}

// MARK: - Sidebar (session drawer equivalent)

private struct Sidebar: View {
    @ObservedObject var vm: ChatViewModel
    /// Observed DIRECTLY (not via `vm`) so scenario mutations (delete/upsert)
    /// re-render this list. `AgentStore` is a nested `ObservableObject`; if the
    /// Sidebar only observed `vm`, `vm.agentStore.delete(...)` would fire
    // `AgentStore.objectWillChange` (not `ChatViewModel.objectWillChange`) and
    // the row would stay visible until restart — the "场景无法删除" bug.
    @ObservedObject var agentStore: AgentStore
    let onOpenSettings: () -> Void
    let onDelete: (String) -> Void

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
            ForEach(agentStore.scenarios) { sc in
                ScenarioRow(scenario: sc, isCurrent: vm.currentScenario?.id == sc.id,
                            onTap: { startScenario(sc) },
                            onDelete: { agentStore.delete(sc) })
                    .equatable()
                    .contextMenu {
                        Button("编辑场景") { vm.overlay = .scenarioEditor(sc) }
                        Button("删除场景", role: .destructive) { agentStore.delete(sc) }
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
            vm.overlay = .scenarioEditor(sc)
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
                        .equatable()
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
        // No `.sheet` here — scenario editor + delete-confirm are in-page
        // overlays driven by `vm.overlay` (see ChatScreen). The Sidebar only
        // needs `agentStore` observation for live delete/upsert refresh.
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

private struct ScenarioRow: View, Equatable {
    let scenario: Scenario
    let isCurrent: Bool
    let onTap: () -> Void
    let onDelete: () -> Void
    @State private var hovered = false
    // Equatable so `.equatable()` skips `body` when the row's data is unchanged
    // — the Sidebar re-renders on any VM @Published change, but a scenario row
    // only needs to re-render when ITS scenario or the current-selection flag
    // changed. The closures aren't part of ==; they're recaptured only when the
    // data DID change (so an edited scenario's tap uses the fresh value).
    static func == (lhs: ScenarioRow, rhs: ScenarioRow) -> Bool {
        lhs.scenario == rhs.scenario && lhs.isCurrent == rhs.isCurrent
    }
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
                // Visible delete affordance (mirrors SessionRow). Without it
                // deletion was only reachable via right-click context menu, and
                // because the Sidebar didn't observe `agentStore` directly the
                // row never disappeared (see Issue 3). Now both are fixed.
                Button(action: onDelete) {
                    Image(systemName: "trash")
                        .foregroundStyle(Theme.onSurfaceVar)
                }
                .buttonStyle(.plain)
                .pointerCursor()
                .help("删除场景")
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

private struct SessionRow: View, Equatable {
    let info: SessionInfoView
    let isCurrent: Bool
    let onTap: () -> Void
    let onDelete: () -> Void
    @State private var hovered = false
    // Equatable so `.equatable()` skips `body` when the row is unchanged. The
    // Sidebar re-renders on VM @Published changes; a session row only needs to
    // re-render when ITS info (messageCount / updatedAtMs) or selection flag
    // changed. SessionInfoView is Equatable (uniffi-generated). Closures aren't
    // part of ==; recaptured only when info changes (so a re-sorted row's tap
    // uses the fresh id).
    static func == (lhs: SessionRow, rhs: SessionRow) -> Bool {
        lhs.info == rhs.info && lhs.isCurrent == rhs.isCurrent
    }
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

// ── Scroll control via the SwiftUI ScrollView's backing NSScrollView ─────────
// Why not SwiftUI's preference-key + `onPreferenceChange`: that inferred "did
// the user scroll?" from the DIRECTION of the content frame's movement — an
// unobservable signal. `proxy.scrollTo(bottom)` (the per-flush auto-follow)
// ALSO moves the content, in the same direction as a user scrolling back to the
// bottom; during streaming the auto-follow's motion and a gentle user wheel-up
// coalesce into one preference delivery, the net delta reads negative, the
// logic re-truths stickToBottom, the next flush yanks the user down. Tug-of-
// war, streaming (faster than the wheel) always won.
//
// Why not a hand-rolled NSScrollView + NSHostingView documentView (tried
// first): NSHostingView in an NSScrollView mis-sizes DYNAMIC content — its
// intrinsic height tracks the clip's PROPOSED (viewport) height, so tall
// content gets squeezed (code blocks / ASCII diagrams collapse to one line,
// the blockquote accent bar spans the viewport, no overflow to scroll); and
// the manual sizeThatFits re-measure needed to fix that fed back into a
// layout loop (freeze). SwiftUI's own ScrollView sizes its content correctly,
// so we KEEP it for rendering and only take over scroll CONTROL.
//
// `ScrollController` (an NSViewRepresentable placed as the ScrollView's
// `.background`) introspects the backing `NSScrollView` by walking its
// superview chain, then uses the two clean signals SwiftUI can't:
//   - synchronous, absolute `isAtBottom` from `documentVisibleRect` vs
//     `documentView.bounds` — a per-frame STATE, never a delta;
//   - a real "the user scrolled" event: a `boundsDidChange` on the clip view
//     that we did NOT cause (gated by `programmaticScroll`), so auto-follow's
//     own motion can never masquerade as a user gesture.
// The at-bottom latch is updated ONLY from user-caused bounds changes; content
// growth (documentView height change — does NOT fire the CLIP's boundsDidChange)
// and programmatic snaps never touch it. This is the general macOS-native
// pattern (iMessage/Telegram/Discord: isAtBottom latch + jump-to-latest +
// suspend-during-gesture).

/// Introspects and controls the enclosing `NSScrollView`. Attached as a
/// `.background` of the SwiftUI `ScrollView`; on attach it walks the superview
/// chain to find the backing `NSScrollView`, then observes its clip view.
private final class ScrollControllerView: NSView {
    var onAtBottomChange: ((Bool) -> Void)?
    /// True when the content bottom is within the viewport. Source of truth for
    /// auto-follow; updated ONLY from user-caused bounds changes.
    private(set) var atBottom: Bool = true
    /// Guards `boundsDidChange` notifications caused by our own snaps so
    /// auto-follow never re-touches the latch. Set around a snap, cleared on
    /// the next runloop (covers any deferred notification delivery).
    private var programmaticScroll = false
    private var boundsObserver: NSObjectProtocol?
    private weak var scrollView: NSScrollView?
    private let bottomEpsilon: CGFloat = 2

    override func viewDidMoveToWindow() { super.viewDidMoveToWindow(); attachIfNeeded() }
    override func viewDidMoveToSuperview() { super.viewDidMoveToSuperview(); attachIfNeeded() }

    /// Walk the superview chain to the enclosing `NSScrollView` (the SwiftUI
    /// `ScrollView`'s AppKit backing on macOS), then observe its clip view.
    /// Idempotent — safe to call from both move-to-window and move-to-superview.
    func attachIfNeeded() {
        guard scrollView == nil, window != nil else { return }
        var ancestor: NSView? = superview
        while let a = ancestor {
            if let sv = a as? NSScrollView { scrollView = sv; break }
            ancestor = a.superview
        }
        guard let sv = scrollView, let cv = sv.contentView as? NSClipView else { return }
        cv.drawsBackground = false
        cv.backgroundColor = .clear
        cv.postsBoundsChangedNotifications = true
        boundsObserver = NotificationCenter.default.addObserver(
            forName: NSView.boundsDidChangeNotification, object: cv, queue: .main
        ) { [weak self] _ in self?.handleBoundsChanged() }
    }

    /// Absolute, per-frame "is the content bottom within the viewport".
    /// Handles both flipped (SwiftUI's backing is flipped: bottom = max-y) and
    /// non-flipped (bottom = min-y) document orientations.
    private var isAtBottom: Bool {
        guard let sv = scrollView, let doc = sv.documentView, doc.bounds.height > 0 else { return true }
        let visH = sv.contentView.bounds.height
        guard visH > 0 else { return true }
        let vis = sv.documentVisibleRect
        return doc.isFlipped
            ? vis.maxY >= doc.bounds.height - bottomEpsilon
            : vis.minY <= bottomEpsilon
    }

    private func handleBoundsChanged() {
        guard !programmaticScroll else { return }   // our own snap — ignore
        let now = isAtBottom
        guard now != atBottom else { return }
        atBottom = now
        onAtBottomChange?(now)
    }

    /// Programmatic scroll to the content bottom. Guarded so the resulting
    /// `boundsDidChange` does not flip the latch; truthes the latch because a
    /// snap means "we are now at the bottom". `NSClipView.scroll(to:)` sets the
    /// bounds origin AND updates the scroller knob (and stays in sync with the
    /// SwiftUI ScrollView's backing).
    func snapToBottom() {
        guard let sv = scrollView, let doc = sv.documentView else { return }
        programmaticScroll = true
        atBottom = true
        let cv = sv.contentView
        let targetY = doc.isFlipped
            ? max(0, doc.bounds.height - cv.bounds.height)
            : min(0, cv.bounds.height - doc.bounds.height) // non-flipped: origin 0 = bottom
        cv.scroll(to: NSPoint(x: 0, y: targetY))
        DispatchQueue.main.async { [weak self] in self?.programmaticScroll = false }
    }

    deinit { if let o = boundsObserver { NotificationCenter.default.removeObserver(o) } }
}

/// `NSViewRepresentable` for `ScrollControllerView`. Drives auto-follow / force-
/// snap via the backing `NSScrollView` (NOT SwiftUI's `ScrollViewReader` — its
/// `scrollTo` is async and re-introduces the programmatic-vs-user ambiguity).
private struct ScrollController: NSViewRepresentable {
    /// Bumped to request an UNCONDITIONAL snap-to-bottom (session load,
    /// "回到底部" button, send). Re-pins regardless of the current latch.
    let forceToken: Int64
    /// Bumped ~20fps during streaming + on a new bubble. Snap-to-bottom only
    /// fires when the user is currently pinned (`view.atBottom`).
    let followToken: Int64
    /// Mirror of the latch — drives the "回到底部" overlay visibility.
    @Binding var atBottom: Bool

    func makeNSView(context: Context) -> ScrollControllerView {
        let v = ScrollControllerView()
        v.onAtBottomChange = { now in DispatchQueue.main.async { atBottom = now } }
        context.coordinator.view = v
        return v
    }

    func updateNSView(_ v: ScrollControllerView, context: Context) {
        v.attachIfNeeded()   // superview chain may materialize after the first pass
        let followChanged = followToken != context.coordinator.lastFollow
        context.coordinator.lastFollow = followToken
        if forceToken != context.coordinator.lastForce {
            context.coordinator.lastForce = forceToken
            // Force snaps are rare (load/button/send): snap now + after a tick so
            // the post-layout document height is reflected (load: content hasn't
            // laid out on the first pass — a non-streaming session has no follow
            // tokens to self-correct).
            DispatchQueue.main.async { v.snapToBottom() }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) { v.snapToBottom() }
        } else if followChanged {
            if v.atBottom { DispatchQueue.main.async { v.snapToBottom() } }
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator {
        weak var view: ScrollControllerView?
        var lastForce: Int64 = -1
        var lastFollow: Int64 = -1
    }
}

private struct ChatDetail: View {
    @ObservedObject var vm: ChatViewModel
    /// Observed in ADDITION to `vm` so the streaming bubble re-renders on
    /// each token without firing `vm.objectWillChange` (which would re-render
    /// the Sidebar too). See `StreamTick`.
    @ObservedObject var streamTick: StreamTick
    @EnvironmentObject var artifacts: ArtifactStore
    let onOpenSettings: () -> Void
    /// Bumped to request an UNCONDITIONAL scroll-to-bottom: session (re)load
    /// (via `vm.scrollRequest`), the "回到底部" overlay button, and send. See
    /// `NSScrollList` / `ChatScrollView` — a forced snap re-pins the latch.
    @State private var bottomRequest: Int = 0
    /// Mirror of the `ChatScrollView`'s at-bottom latch, used ONLY to drive the
    /// "回到底部" overlay's visibility. The latch itself lives in the scroll
    /// view (updated from user-caused bounds changes only).
    @State private var atBottom: Bool = true

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
                        BrandLogo(cell: 4.5, gap: 1, depth: 1).equatable()
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

            // Message list — SwiftUI's `ScrollView` (sizes its own content
            // correctly) with a non-lazy VStack + `.equatable()` bubbles for
            // stable geometry + bounded per-flush cost. `ScrollController`
            // (background of the VStack — INSIDE the scroll content, so its
            // superview chain reaches the backing NSScrollView; an outer
            // `.background` would land as a SIBLING of the NSScrollView and
            // never find it) drives auto-follow / snap WITHOUT the old
            // preference-key frame-direction logic (which couldn't tell a
            // programmatic auto-follow from a user scrolling back, yanking the
            // user down mid-stream). See `ScrollController` above.
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    ForEach(vm.items) { entry in
                        switch entry {
                        case .user(let u): UserBubble(text: u.text, item: u, vm: vm).equatable()
                        case .assistant(let a): AssistantBubble(item: a, scenario: vm.currentScenario, onRetry: { Task { await vm.retryLast() } }).equatable()
                        }
                    }
                }
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(ScrollController(
                    // Force-snap (re-pin) on session load (vm.scrollRequest) or
                    // the "回到底部" button (bottomRequest).
                    forceToken: Int64(vm.scrollRequest) + Int64(bottomRequest),
                    // Follow-snap (only while pinned) on streaming flush
                    // (streamTick.value) + new bubble (items.count).
                    followToken: streamTick.value + Int64(vm.items.count),
                    atBottom: $atBottom
                ))
            }
            .overlay(alignment: .bottom) {
                if !atBottom && !vm.items.isEmpty {
                    Button {
                        atBottom = true
                        bottomRequest += 1
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

            if let msg = vm.error {
                Text("✗ \(msg)").foregroundStyle(Theme.errorC).font(.oCaption)
                    .padding(.horizontal, 12).padding(.vertical, 4)
            }

            InputBar(value: $vm.input, running: vm.running,
                     onChange: { vm.input = $0 },
                     onSend: {
                         let task = vm.input.trimmingCharacters(in: .whitespacesAndNewlines)
                         if !task.isEmpty && !vm.running {
                             vm.input = ""; bottomRequest += 1
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

    private struct Suggestion: Identifiable { let id: UUID; let icon: String; let text: String }

    /// Stable across renders. A computed `var` with `let id = UUID()` minted
    /// fresh UUIDs every body evaluation — and the welcome screen re-renders on
    /// every keystroke in the input bar (vm.input is @Published), so the whole
    /// suggestion list was rebuilt per character typed. A `static let` builds
    /// it once.
    private static let suggestions: [Suggestion] = [
        .init(id: UUID(), icon: "doc.text.magnifyingglass", text: "帮我总结一段笔记的核心要点"),
        .init(id: UUID(), icon: "hammer", text: "用 Rust 写一个读取 JSON 的命令行小工具"),
        .init(id: UUID(), icon: "globe", text: "解释一下 Agent 与 RAG 的区别"),
        .init(id: UUID(), icon: "sparkles", text: "把这段话改写得更简洁专业"),
    ]

    var body: some View {
        ScrollView {
            VStack(spacing: 16) {
                Color.clear.frame(height: 4)
                BrandLogo(cell: 8, gap: 1.5, depth: 1.5).equatable()
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
                // Stacked vertically (one full-width row per suggestion) —
                // not the old 2-column grid. Each row is a single prompt.
                VStack(spacing: 8) {
                    ForEach(Self.suggestions) { s in
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

private struct UserBubble: View, Equatable {
    let text: String
    let item: UserItem
    // Plain `let` (not @ObservedObject): UserBubble reads no VM state in its
    // body — it only calls `vm` methods on user action — so it doesn't need
    // reactivity. Combined with `.equatable()` below this keeps done user
    // bubbles from re-evaluating on every streamTick flush.
    let vm: ChatViewModel

    static func == (lhs: UserBubble, rhs: UserBubble) -> Bool {
        lhs.text == rhs.text && lhs.item === rhs.item
    }

    var body: some View {
        HStack { Spacer(minLength: 60)
            Text(text).font(.oBody).foregroundStyle(Theme.onBg)
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(Theme.primaryCont)
                .clipShape(RoundedRectangle(cornerRadius: 14))
                .frame(maxWidth: 360, alignment: .trailing)
                .contextMenu {
                    Button("编辑并重发") { vm.overlay = .editMessage(item) }
                    Button("复制") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(text, forType: .string)
                    }
                }
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

private struct AssistantBubble: View, Equatable {
    let item: AssistantItem
    let scenario: Scenario?
    let onRetry: () -> Void
    /// Per-render SNAPSHOT of `item.version` (a value, copied at init — NOT a
    /// read-through). `.equatable()` compares this against the PREVIOUS render's
    /// captured value: a done (idle) bubble's version is stable → `==` true →
    /// `body` SKIPPED (just an Int compare); only the active streaming bubble
    /// (version bumped in `handle()` on every token) re-renders. That bounds
    /// the non-lazy VStack's per-flush cost to one bubble instead of O(all) —
    /// the fix for the streaming freeze on long conversations. (Comparing
    /// `item.version` directly through the shared reference would always read
    /// the current value and never detect change — hence the snapshot.)
    private let version: Int

    init(item: AssistantItem, scenario: Scenario?, onRetry: @escaping () -> Void) {
        self.item = item
        self.scenario = scenario
        self.onRetry = onRetry
        self.version = item.version
    }

    static func == (lhs: AssistantBubble, rhs: AssistantBubble) -> Bool {
        // Same item instance + unchanged version + same scenario → skip body.
        // `onRetry` (a closure) isn't part of equality; it's recaptured only
        // when the body DOES run (on change), which is fine — retry on a done
        // bubble uses the last-captured closure (still calls vm.retryLast).
        lhs.item === rhs.item && lhs.version == rhs.version && lhs.scenario == rhs.scenario
    }

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
            // Pre-first-token placeholder. When a turn is running but no
            // thinking, tool, or answer text has arrived yet, the bubble would
            // otherwise be blank — so the long wait before the first answer
            // token reads as a frozen app (issue 6). A "思考中" indicator gives
            // visible progress; it disappears the moment thinking or text
            // arrives (the ThinkingCard / MarkdownText take over).
            if item.streaming && !item.done
                && item.thinking.isEmpty && item.text.isEmpty && item.steps.isEmpty {
                HStack(spacing: 6) {
                    Text("思考中").font(.oCaption).foregroundStyle(Theme.onSurfaceVar)
                    ThreeDots()
                }
            }
            if !item.text.isEmpty {
                // Render markdown LIVE while streaming (issue 8) — not a plain
                // plain-text tail held back until `.done`. `MarkdownText` with
                // `streaming: true` bounds per-flush work to the in-progress
                // block via per-block `.equatable()`; the full uncapped render
                // lands once on completion. The steady caret is folded into the
                // last text block by `MarkdownText` itself.
                MarkdownText(text: item.text.trimmingCharacters(in: .whitespacesAndNewlines),
                             streaming: item.streaming && !item.done)
                    .equatable()
                    .contextMenu {
                        Button("重新生成") { onRetry() }
                        Button("复制") { copyText(item.text) }
                        Button("分享") { shareText(item.text) }
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
    /// Local @State (NOT `item.thinkingExpanded`) so the chevron toggle works
    /// even when `AssistantBubble.body` is skipped by `.equatable()` (done
    /// bubbles). SwiftUI preserves @State across both skipped-body and
    /// re-rendered-body passes by structural identity, matching
    /// `ToolStepsCard`'s pattern.
    @State private var expanded = false
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
                        expanded.toggle()
                    } label: {
                        Image(systemName: expanded ? "chevron.down" : "chevron.right")
                            .foregroundStyle(Theme.onSurfaceVar)
                    }
                    .buttonStyle(.plain)
                    .pointerCursor()
                }
                if expanded {
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

private struct MarkdownBlockView: View, Equatable {
    let block: MdBlock
    // Equatable (synthesized — `MdBlock: Equatable`) so `.equatable()` skips a
    // block whose source is unchanged. During streaming the growing bubble
    // re-evaluates each flush; without per-block equality every block would
    // re-run `buildInline` (→ `AttributedString(markdown:)`) every token.
    // With it, only the LAST (in-progress) block re-parses — the work is
    // bounded to one block per flush instead of the whole reply.
    var body: some View {
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

    private func headingFont(_ level: Int) -> Font {
        switch level {
        case 1: return .title2.bold()
        case 2: return .title3.bold()
        case 3: return .headline
        default: return .subheadline.bold()
        }
    }
}

private struct MarkdownText: View, Equatable {
    let text: String
    /// `true` while the model is still producing this reply. Renders the
    /// partial markdown live (instead of holding back a plain-text tail until
    /// `.done`) so the user sees formatted markdown stream in. Per-block
    /// `.equatable()` on `MarkdownBlockView` bounds the per-flush work to the
    /// last in-progress block; a runaway single-paragraph reply is itself
    /// capped so Core Text layout stays bounded. The full, uncapped markdown
    /// renders once on completion (when `streaming` flips false).
    var streaming: Bool = false
    // Top-level Equatable so a DONE bubble skips `body` entirely while a
    // SIBLING is streaming (streamTick bumps re-evaluate every visible
    // AssistantBubble; without this, every done bubble would re-parse its
    // markdown on every token of the active reply). The streaming bubble's
    // `text` changes each flush, so `==` is false and `body` runs — but the
    // per-block `.equatable()` inside bounds that run to the last block.
    static func == (lhs: MarkdownText, rhs: MarkdownText) -> Bool {
        lhs.text == rhs.text && lhs.streaming == rhs.streaming
    }

    /// While streaming, cap a runaway last paragraph to its tail so a single
    /// giant paragraph doesn't re-lay-out unbounded text every flush. The full
    /// text renders once on `.done`.
    private static let streamingCap = 1800

    var body: some View {
        var blocks = splitMarkdown(text)
        if streaming, let last = blocks.last, case .paragraph(let p) = last, p.count > Self.streamingCap {
            blocks[blocks.count - 1] = .paragraph(String(p.suffix(Self.streamingCap)))
        }
        // Append a steady inline caret to the last text-like block while
        // streaming — a separate cursor row read as an extra blank line +
        // flicker, so it's folded into the in-progress text instead.
        if streaming, let last = blocks.last {
            switch last {
            case .paragraph(let p):
                blocks[blocks.count - 1] = .paragraph(p + "▍")
            case .heading(let level, let h):
                blocks[blocks.count - 1] = .heading(level: level, text: h + "▍")
            default: break
            }
        }
        return VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                MarkdownBlockView(block: block).equatable()
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
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

    // Local @State mirrors of the provider/embedding config. Editing a
    // @Published field directly on `vm` ($vm.baseUrl / $vm.apiKey / …) fired
    // vm.objectWillChange on EVERY keystroke → the whole ChatScreen (sidebar +
    // message list + every visible markdown bubble) re-rendered per character
    // typed, which is the "设置弹框卡顿" the user saw. Local @State keeps
    // keystroke churn inside this sheet; nothing reaches `vm` until 保存.
    @State private var baseUrl = ""
    @State private var apiKey = ""
    @State private var model = ""
    @State private var embProvider = "auto"
    @State private var embModel = ""
    @State private var embApiKey = ""
    @State private var embBaseUrl = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Provider 设置").font(.oHeadline)
            TextField("base url (如 https://api.openai.com/v1;留空=默认)", text: $baseUrl)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            SecureField("api key (openai / anthropic)", text: $apiKey).textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            TextField("model (如 gpt-4o-mini / claude-sonnet-4-6 / llama3)", text: $model)
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
            Picker("provider", selection: $embProvider) {
                ForEach(["auto", "openai", "voyage", "ollama", "fastembed", "openai-compat"], id: \.self) {
                    Text($0).tag($0)
                }
            }
            .pickerStyle(.menu)
            TextField("model (空 = provider 默认)", text: $embModel)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            SecureField("embedding api key (VOYAGE_API_KEY / OPENAI_API_KEY)", text: $embApiKey)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            TextField("base url (openai-compat 必填;ollama → host:port)", text: $embBaseUrl)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 13, design: .monospaced))
            Text("auto 探测链:openai-compat → voyage → openai → ollama → fastembed;全无可用时降级为关键词召回。embedding key 与主模型 key 相互独立。")
                .font(.oCaption2).foregroundStyle(Theme.onSurfaceVar)

            HStack {
                Spacer()
                Button("保存") {
                    Task {
                        // Commit the local draft to the VM in one shot, then
                        // rebuild — a single objectWillChange burst instead of
                        // a per-keystroke flood.
                        vm.baseUrl = baseUrl
                        vm.apiKey = apiKey
                        vm.model = model
                        vm.embProvider = embProvider
                        vm.embModel = embModel
                        vm.embApiKey = embApiKey
                        vm.embBaseUrl = embBaseUrl
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
        .onAppear {
            // Seed the local fields once when the sheet opens. Subsequent
            // edits stay local; only 保存 writes back.
            baseUrl = vm.baseUrl
            apiKey = vm.apiKey
            model = vm.model
            embProvider = vm.embProvider
            embModel = vm.embModel
            embApiKey = vm.embApiKey
            embBaseUrl = vm.embBaseUrl
        }
    }
}

// MARK: - In-page overlay layer (replaces .sheet/.alert)

/// Full-coverage dimmed backdrop + the dialog card. Presented by `ChatScreen`
/// whenever `vm.overlay != nil`. Tap-outside dismisses for non-destructive
/// dialogs; the delete-confirm case requires its explicit button (so a stray
/// tap can't drop a conversation). The card chrome (background + clip +
/// shadow) is shared; `CommandPalette` supplies its own chrome + fixed size.
struct OverlayLayer: View {
    let overlay: AppOverlay
    @ObservedObject var vm: ChatViewModel

    var body: some View {
        ZStack {
            Color.black.opacity(0.35).ignoresSafeArea()
                .contentShape(Rectangle())
                .onTapGesture {
                    // Destructive confirm needs an explicit button — no dismiss.
                    if case .deleteSession = overlay { return }
                    vm.overlay = nil
                }
            content
        }
    }

    @ViewBuilder private var content: some View {
        switch overlay {
        case .settings:
            card { SettingsSheet(vm: vm, onClose: { vm.overlay = nil }) }
        case .scenarioEditor(let sc):
            card { ScenarioEditor(scenario: sc, store: vm.agentStore, onClose: { vm.overlay = nil }) }
        case .editMessage(let u):
            card { EditMessagePage(item: u, vm: vm) }
        case .commandPalette:
            // Has its own card chrome + fixed size; no shared wrapper.
            CommandPalette(vm: vm)
        case .deleteSession(let id):
            card { DeleteConfirmOverlay(sessionId: id, vm: vm) }
        }
    }

    /// Shared card chrome for the editor/confirm pages. Each page supplies its
    /// own size (`.frame`); this adds the surface background, rounded clip,
    /// hairline border, and drop shadow so the page reads as a raised card
    /// over the dimmed backdrop.
    @ViewBuilder private func card<C: View>(@ViewBuilder _ c: () -> C) -> some View {
        c()
            .background(Theme.surface)
            .clipShape(RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.surfaceVar, lineWidth: 1))
            .shadow(color: .black.opacity(0.3), radius: 24, y: 8)
    }
}

/// Edit-and-resend a user message — in-page replacement for the old
/// `UserBubble` edit `.sheet`. Draft seeded from the message; 重发 closes the
/// overlay and re-runs from the edited text via `vm.editAndResend`.
private struct EditMessagePage: View {
    let item: UserItem
    @ObservedObject var vm: ChatViewModel
    @State private var draft: String

    init(item: UserItem, vm: ChatViewModel) {
        self.item = item
        self.vm = vm
        _draft = State(initialValue: item.text)
    }

    var body: some View {
        VStack(spacing: 12) {
            Text("编辑消息").font(.oHeadline)
            TextEditor(text: $draft)
                .font(.oBody).scrollContentBackground(.hidden)
                .background(Theme.surfaceVar).clipShape(RoundedRectangle(cornerRadius: 8))
                .frame(minHeight: 120, maxHeight: 260)
            HStack {
                Spacer()
                Button("取消", role: .cancel) { vm.overlay = nil }.keyboardShortcut(.escape)
                Button("重发") {
                    let s = draft
                    vm.overlay = nil
                    Task { await vm.editAndResend(item, newText: s) }
                }.keyboardShortcut(.defaultAction)
            }
        }
        .frame(width: 460)
        .padding(16)
    }
}

/// Delete-session confirmation — in-page replacement for the old native
/// `.alert`. Mirrors its copy ("删除会话" / "确定删除这个会话?历史无法恢复。").
private struct DeleteConfirmOverlay: View {
    let sessionId: String
    @ObservedObject var vm: ChatViewModel

    var body: some View {
        VStack(spacing: 12) {
            Text("删除会话").font(.oHeadline)
            Text("确定删除这个会话?历史无法恢复。")
                .font(.oSubheadline).foregroundStyle(Theme.onSurfaceVar)
                .multilineTextAlignment(.center)
            HStack(spacing: 12) {
                Spacer()
                Button("取消", role: .cancel) { vm.overlay = nil }.keyboardShortcut(.escape)
                Button("删除", role: .destructive) {
                    let id = sessionId
                    vm.overlay = nil
                    Task { await vm.deleteSession(id) }
                }.keyboardShortcut(.defaultAction)
            }
        }
        .padding(20).frame(width: 360)
    }
}
