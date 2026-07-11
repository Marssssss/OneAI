// SwiftUI views — macOS port of the Android Compose chat UI.
// NavigationSplitView (sidebar = session list) + detail (chat). Settings,
// delete-confirm, first-run hint, scroll-to-bottom, copy/share, retry all
// reproduced. Dark theme follows the system via the adaptive Theme palette.

import SwiftUI
import AppKit

// MARK: - Root screen

struct ChatScreen: View {
    @StateObject private var vm = ChatViewModel()
    @State private var showSettings = false
    @State private var pendingDeleteId: String? = nil

    var body: some View {
        NavigationSplitView {
            Sidebar(vm: vm, onOpenSettings: { showSettings = true },
                    onDelete: { pendingDeleteId = $0 })
                .navigationSplitViewColumnWidth(min: 220, ideal: 260)
        } detail: {
            ChatDetail(vm: vm, onOpenSettings: { showSettings = true })
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

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("会话").font(.headline)
                Spacer()
                Button {
                    Task { await vm.newConversation() }
                } label: {
                    Label("新对话", systemImage: "plus")
                }
                .buttonStyle(.borderless)
                .help("新对话")
            }
            .padding(.horizontal, 12).padding(.vertical, 10)
            Divider()
            if vm.sessions.isEmpty {
                Text("还没有会话\n发一条消息开始吧")
                    .foregroundStyle(Theme.onSurfaceVar)
                    .font(.footnote)
                    .padding(20)
                Spacer()
            } else {
                List(selection: Binding(
                    get: { vm.currentSessionId },
                    set: { if let id = $0 { Task { await vm.loadSession(id) } } }
                )) {
                    ForEach(vm.sessions, id: \.id) { s in
                        SessionRow(info: s, isCurrent: s.id == vm.currentSessionId,
                                   onDelete: { onDelete(s.id) })
                            .tag(s.id)
                    }
                }
                .listStyle(.sidebar)
            }
            Divider()
            Button {
                onOpenSettings()
            } label: {
                Label("设置", systemImage: "gearshape")
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .padding(.horizontal, 12).padding(.vertical, 12)
        }
        .background(Theme.surface)
    }
}

private struct SessionRow: View {
    let info: SessionInfoView
    let isCurrent: Bool
    let onDelete: () -> Void
    var body: some View {
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
        .padding(.vertical, 4)
        .listRowBackground(isCurrent ? Theme.primaryCont.opacity(0.5) : Color.clear)
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
    let onOpenSettings: () -> Void
    @State private var stickToBottom = true

    var body: some View {
        VStack(spacing: 0) {
            // Top bar
            HStack {
                Text("OneAI").font(.title3.bold()).foregroundStyle(Theme.onBg)
                Spacer()
                Button { onOpenSettings() } label: { Image(systemName: "gearshape") }
                    .help("Provider 设置")
            }
            .padding(.horizontal, 16).padding(.vertical, 8)
            Divider()

            if vm.needsKeyConfig {
                FirstRunHint(onOpen: onOpenSettings).padding(.horizontal, 12).padding(.vertical, 6)
            }

            // Message list
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 14) {
                        ForEach(vm.items) { entry in
                            switch entry {
                            case .user(let u): UserBubble(text: u.text)
                            case .assistant(let a): AssistantBubble(item: a, onRetry: { Task { await vm.retryLast() } })
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
                     onStop: { Task { await vm.stop() } })
        }
        .background(Theme.background)
    }
}

// MARK: - Bubbles

private struct UserBubble: View {
    let text: String
    var body: some View {
        HStack { Spacer(minLength: 60)
            Text(text).foregroundStyle(Theme.onBg)
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(Theme.primaryCont)
                .clipShape(RoundedRectangle(cornerRadius: 14))
                .frame(maxWidth: 360, alignment: .trailing)
        }
    }
}

private struct AssistantBubble: View {
    let item: AssistantItem
    let onRetry: () -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ThinkingCard(item: item)
            if !item.steps.isEmpty {
                VStack(alignment: .leading, spacing: 2) {
                    ForEach(item.steps) { StepLine(step: $0) }
                }
            }
            if !item.text.isEmpty {
                MarkdownText(text: item.text)
                    .contextMenu {
                        Button("复制") { copyText(item.text) }
                        Button("分享") { shareText(item.text) }
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
            let expanded = item.thinkingActive || item.thinkingExpanded
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

private struct StepLine: View {
    let step: ToolStep
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
                Text("    └ \(String(r.prefix(200)))")
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(Theme.onSurfaceVar)
                    .lineLimit(3)
            }
        }
    }
}

// MARK: - Markdown

private struct MarkdownText: View {
    let text: String
    var body: some View {
        let segs = splitMarkdown(text)
        return VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(segs.enumerated()), id: \.offset) { _, seg in
                switch seg {
                case .prose(let body):
                    Text(buildInline(body, codeBg: Theme.surfaceVar))
                        .foregroundStyle(Theme.onBg)
                        .font(.body)
                        .textSelection(.enabled)
                case .code(let lang, let code):
                    CodeCard(lang: lang, code: code)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct CodeCard: View {
    let lang: String
    let code: String
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if !lang.isEmpty {
                Text(lang).font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(Theme.onSurfaceVar)
            }
            ScrollView(.horizontal) {
                Text(code).font(.system(size: 13, design: .monospaced))
                    .foregroundStyle(Theme.onBg)
                    .textSelection(.enabled)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.surfaceVar)
        .clipShape(RoundedRectangle(cornerRadius: 8))
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
    @FocusState private var focused: Bool

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
    }
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
