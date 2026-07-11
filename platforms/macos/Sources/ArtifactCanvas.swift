// ArtifactCanvas — right-docked panel that renders long code / documents
// surfaced by an assistant message, so they don't挤占对话流. Short code stays
// inline in the bubble; long content (or anything the user clicks "在画布打开"
// on) is promoted to a canvas tab.

import SwiftUI
import AppKit

/// One artifact (a code block or long text surfaced by a message).
struct Artifact: Identifiable, Hashable {
    let id = UUID()
    let title: String       // tab label, e.g. "main.rs" or "代码"
    let lang: String
    let content: String
}

/// Shared canvas state — CodeCards push artifacts here; ChatDetail renders the
/// panel when non-empty.
final class ArtifactStore: ObservableObject {
    @Published var artifacts: [Artifact] = []
    @Published var selectedId: UUID? = nil
    @Published var visible: Bool = false

    func open(_ artifact: Artifact) {
        if let idx = artifacts.firstIndex(where: { $0.content == artifact.content && $0.lang == artifact.lang }) {
            selectedId = artifacts[idx].id
        } else {
            artifacts.append(artifact)
            selectedId = artifact.id
        }
        visible = true
    }

    func close(_ id: UUID) {
        artifacts.removeAll { $0.id == id }
        if selectedId == id { selectedId = artifacts.first?.id }
        if artifacts.isEmpty { visible = false }
    }
}

struct ArtifactCanvas: View {
    @ObservedObject var store: ArtifactStore

    var body: some View {
        Group {
            if store.artifacts.isEmpty {
                ContentUnavailableViewCompat()
            } else if let sel = binding {
                artifactView(sel)
            } else {
                ContentUnavailableViewCompat()
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.background)
    }

    private var binding: Artifact? {
        store.artifacts.first { $0.id == store.selectedId } ?? store.artifacts.first
    }

    @ViewBuilder
    private func artifactView(_ a: Artifact) -> some View {
        VStack(spacing: 0) {
            // Tab bar
            HStack(spacing: 0) {
                ForEach(store.artifacts) { tab in
                    tabButton(tab)
                }
                Spacer()
                Button { if let id = store.selectedId { store.close(id) } } label: {
                    Image(systemName: "xmark").font(.caption).foregroundStyle(Theme.onSurfaceVar)
                }.buttonStyle(.plain).padding(.horizontal, 8).help("关闭")
            }
            .padding(.vertical, 6)
            .background(Theme.surface)
            Divider()
            // Toolbar: copy / export
            HStack {
                if !a.lang.isEmpty {
                    Text(a.lang).font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(Theme.onSurfaceVar)
                }
                Spacer()
                Button("复制") { copy(a.content) }.buttonStyle(.bordered).controlSize(.small)
                Button("导出…") { exportFile(a) }.buttonStyle(.bordered).controlSize(.small)
            }
            .padding(.horizontal, 12).padding(.vertical, 6)
            .background(Theme.surface)
            Divider()
            // Content
            ScrollView {
                Text(a.content)
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundStyle(Theme.onBg)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(12)
            }
        }
    }

    private func tabButton(_ tab: Artifact) -> some View {
        let isSel = tab.id == store.selectedId
        return Button { store.selectedId = tab.id } label: {
            HStack(spacing: 4) {
                Image(systemName: "doc.text").font(.caption2)
                Text(tab.title).font(.caption)
                Button { store.close(tab.id) } label: {
                    Image(systemName: "xmark").font(.system(size: 8))
                }.buttonStyle(.plain)
            }
            .padding(.horizontal, 10).padding(.vertical, 5)
            .background(isSel ? Theme.primaryCont.opacity(0.6) : Color.clear)
            .foregroundStyle(isSel ? Theme.primary : Theme.onSurfaceVar)
        }.buttonStyle(.plain)
    }

    private func copy(_ s: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(s, forType: .string)
    }

    private func exportFile(_ a: Artifact) {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = suggestedFilename(a)
        panel.canCreateDirectories = true
        if panel.runModal() == .OK, let url = panel.url {
            try? a.content.write(to: url, atomically: true, encoding: .utf8)
        }
    }

    private func suggestedFilename(_ a: Artifact) -> String {
        let ext = extFor(a.lang)
        let base = a.title.split(separator: ".").dropLast().joined(separator: ".")
        let name = base.isEmpty ? a.title : base
        return ext.isEmpty ? name : "\(name).\(ext)"
    }

    private func extFor(_ lang: String) -> String {
        switch lang.lowercased() {
        case "rust", "rs": return "rs"
        case "swift": return "swift"
        case "python", "py": return "py"
        case "javascript", "js": return "js"
        case "typescript", "ts": return "ts"
        case "shell", "bash", "sh": return "sh"
        case "json": return "json"
        case "yaml", "yml": return "yml"
        case "markdown", "md": return "md"
        case "html": return "html"
        case "css": return "css"
        case "sql": return "sql"
        case "go": return "go"
        case "java": return "java"
        case "kotlin", "kt": return "kt"
        default: return lang.isEmpty ? "" : lang
        }
    }
}

/// A compact "no artifact" placeholder (compatible with macOS 13 — avoids
/// `ContentUnavailableView` which is macOS 14+).
private struct ContentUnavailableViewCompat: View {
    var body: some View {
        VStack(spacing: 6) {
            Image(systemName: "doc.text.magnifyingglass")
                .font(.title2).foregroundStyle(Theme.onSurfaceVar)
            Text("点击代码块上的「在画布打开」").font(.caption).foregroundStyle(Theme.onSurfaceVar)
            Text("长内容会在这里渲染,不挤占对话").font(.caption2).foregroundStyle(Theme.onSurfaceVar)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// Threshold above which a code block is auto-promoted to the canvas.
let artifactThreshold = 600
