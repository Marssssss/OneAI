// CommandPalette — ⌘K palette: quick switch between scenarios / recent
// sessions / models, and open settings. Fuzzy-filtered. Presented in-page
// (vm.overlay == .commandPalette) — the backdrop + centering come from
// `OverlayLayer`; this view is just the card.

import SwiftUI

struct CommandPalette: View {
    @ObservedObject var vm: ChatViewModel
    @State private var query: String = ""
    @FocusState private var focused: Bool

    private var filteredScenarios: [Scenario] {
        let q = query.lowercased()
        let all = vm.agentStore.scenarios
        return q.isEmpty ? all : all.filter { $0.name.lowercased().contains(q) }
    }

    private var filteredSessions: [SessionInfoView] {
        let q = query.lowercased()
        let all = vm.sessions
        return q.isEmpty ? all : all.filter { ($0.title ?? "").lowercased().contains(q) }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Image(systemName: "magnifyingglass").foregroundStyle(Theme.onSurfaceVar)
                TextField("切换场景 / 会话 / 模型…", text: $query)
                    .textFieldStyle(.plain).font(.oBody)
                    .focused($focused)
                    .onSubmit { vm.overlay = nil }
                Button("关闭") { vm.overlay = nil }.buttonStyle(.borderless).keyboardShortcut(.escape)
            }
            .padding(12)
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    if !filteredScenarios.isEmpty {
                        sectionHeader("场景")
                        ForEach(filteredScenarios) { sc in
                            cmdRow(icon: sc.icon, title: sc.name, subtitle: "\(sc.agents.count) 个智能体") {
                                Task { await vm.newConversation(scenario: sc) }
                                vm.overlay = nil
                            }
                        }
                    }
                    if !filteredSessions.isEmpty {
                        sectionHeader("最近会话")
                        ForEach(filteredSessions, id: \.id) { s in
                            cmdRow(icon: "bubble.left", title: s.title?.isEmpty == false ? s.title! : "新对话",
                                   subtitle: "\(s.messageCount) 条") {
                                Task { await vm.loadSession(s.id) }
                                vm.overlay = nil
                            }
                        }
                    }
                    sectionHeader("操作")
                    cmdRow(icon: "plus.bubble", title: "新对话(单 Agent)", subtitle: nil) {
                        Task { await vm.newConversation() }
                        vm.overlay = nil
                    }
                    cmdRow(icon: "gearshape", title: "打开设置", subtitle: nil) {
                        vm.overlay = .settings
                    }
                }
                .padding(.vertical, 6)
            }
        }
        .frame(width: 460, height: 420)
        .background(Theme.surface)
        .clipShape(RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.surfaceVar, lineWidth: 1))
        .onAppear { focused = true }
    }

    private func sectionHeader(_ t: String) -> some View {
        Text(t).font(.caption2.bold()).foregroundStyle(Theme.onSurfaceVar)
            .padding(.horizontal, 14).padding(.top, 8).padding(.bottom, 2)
    }

    private func cmdRow(icon: String, title: String, subtitle: String?, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(systemName: icon).foregroundStyle(Theme.primary).frame(width: 20)
                Text(title).foregroundStyle(Theme.onBg)
                Spacer()
                if let s = subtitle { Text(s).font(.oCaption).foregroundStyle(Theme.onSurfaceVar) }
            }
            .padding(.horizontal, 14).padding(.vertical, 6)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
