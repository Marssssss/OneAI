// ScenarioEditor — build/edit a multi-agent scenario (cast + turn policy +
// opener). Shown as a sheet from the sidebar's "新场景"/"编辑场景" actions.

import SwiftUI

struct ScenarioEditor: View {
    @State var scenario: Scenario
    @ObservedObject var store: AgentStore
    let onClose: () -> Void
    /// Inline validation message. When non-nil, the 保存 button refuses to
    /// close — the user must fix the named problem first. Prevents saving a
    /// malformed scenario (empty cast / empty prompts / dangling turn-order
    /// ids) that the engine would later reject on launch as "场景启动失败"
    /// and that the user couldn't recover from the list.
    @State private var saveError: String? = nil

    var body: some View {
        VStack(spacing: 12) {
            // Scrollable editor body — adding actors / fields / debrief can grow
            // the form past the fixed sheet height; without a ScrollView the
            // overflow was clipped (users couldn't reach fields off the bottom).
            // The action row stays pinned below the scroll area.
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
            HStack {
                TextField("场景名", text: $scenario.name).textFieldStyle(.roundedBorder)
                Picker("", selection: $scenario.icon) {
                    ForEach(["person.2", "person.crop.circle.badge.questionmark", "globe", "scalemass", "pencil.line", "lightbulb", "brain.head.profile"], id: \.self) {
                        Image(systemName: $0).tag($0)
                    }
                }.frame(width: 60).labelsHidden()
            }

            // ── Cast ──
            Text("演员表").font(.headline)
            ForEach($scenario.agents) { $a in
                AgentCard(agent: $a, allowDelete: scenario.agents.count > 1) {
                    scenario.agents.removeAll { $0.id == a.id }
                    if scenario.openerAgentId == a.id { scenario.openerAgentId = nil }
                    if scenario.moderatorId == a.id { scenario.moderatorId = nil }
                    scenario.scriptOrder = scenario.scriptOrder?.filter { $0 != a.id }
                }
            }
            Button { scenario.agents.append(Agent(id: UUID().uuidString.prefix(8).description, name: "新角色", role: "", systemPrompt: "", model: nil, color: "#4D6BFE", avatar: "person.crop.circle", kind: nil, apiKey: nil, baseUrl: nil)) } label: {
                Label("添加智能体", systemImage: "plus")
            }.buttonStyle(.bordered)

            Divider()

            // ── Topic intake fields ──
            Text("主题输入字段(开始场景时弹出,值会嵌入各角色背景)").font(.headline)
            ForEach($scenario.topicFields.boundList) { $f in
                VStack(alignment: .leading, spacing: 4) {
                    HStack {
                        TextField("字段名(如:应聘岗位)", text: $f.label).textFieldStyle(.roundedBorder)
                        TextField("占位提示(可选)", text: Binding(
                            get: { f.placeholder ?? "" },
                            set: { f.placeholder = $0.isEmpty ? nil : $0 }
                        )).textFieldStyle(.roundedBorder)
                        Button { scenario.topicFields?.removeAll { $0.id == f.id } } label: {
                            Image(systemName: "trash")
                        }.buttonStyle(.borderless)
                    }
                    // Per-member visibility: nil = all members see this value
                    // (shared context); otherwise only the checked members see it
                    // (e.g. interviewee's project info → coach only).
                    HStack {
                        Image(systemName: "eye").font(.caption2).foregroundStyle(Theme.onSurfaceVar)
                        Menu {
                            Button { f.visibleTo = nil } label: { Label("全员可见", systemImage: f.visibleTo == nil ? "checkmark" : "") }
                            ForEach(scenario.agents) { a in
                                let on = f.visibleTo?.contains(a.id) ?? false
                                Button {
                                    var v = f.visibleTo ?? []
                                    if v.contains(a.id) { v.removeAll { $0 == a.id } }
                                    else { v.append(a.id) }
                                    f.visibleTo = v.isEmpty ? nil : v
                                } label: { Label(a.name, systemImage: on ? "checkmark" : "") }
                            }
                        } label: {
                            Text(visibilityLabel(for: f, in: scenario))
                                .font(.caption2).foregroundStyle(Theme.onSurfaceVar)
                        }
                        .menuStyle(.borderlessButton).fixedSize()
                    }
                }
            }
            Button {
                let id = UUID().uuidString.prefix(8).description
                scenario.topicFields = (scenario.topicFields ?? []) + [TopicField(id: id, label: "", placeholder: nil)]
            } label: { Label("添加字段", systemImage: "plus") }.buttonStyle(.bordered)

            Divider()

            // ── Debrief phase (optional) ──
            Text("结束阶段(可选,如面试结束后指导员总结)").font(.headline)
            Toggle("启用结束阶段", isOn: Binding(
                get: { scenario.debrief != nil },
                set: { on in
                    if on, scenario.debrief == nil {
                        scenario.debrief = DebriefConfig(
                            buttonLabel: "结束",
                            summaryPrompt: "请对本次对话进行全场总结与建议。",
                            debriefMemberId: scenario.agents.first?.id ?? "")
                    } else if !on {
                        scenario.debrief = nil
                    }
                }
            ))
            if scenario.debrief != nil {
                HStack {
                    TextField("按钮文字", text: Binding(
                        get: { scenario.debrief!.buttonLabel },
                        set: { scenario.debrief!.buttonLabel = $0 }
                    )).textFieldStyle(.roundedBorder)
                    Picker("接管角色", selection: Binding(
                        get: { scenario.debrief!.debriefMemberId },
                        set: { scenario.debrief!.debriefMemberId = $0 }
                    )) {
                        ForEach(scenario.agents) { Text($0.name).tag($0.id) }
                    }
                }
                VStack(alignment: .leading) {
                    Text("总结提示词(发给接管角色)").font(.caption).foregroundStyle(Theme.onSurfaceVar)
                    TextEditor(text: Binding(
                        get: { scenario.debrief!.summaryPrompt },
                        set: { scenario.debrief!.summaryPrompt = $0 }
                    ))
                    .font(.system(size: 12)).frame(minHeight: 60, maxHeight: 120)
                    .scrollContentBackground(.hidden).background(Theme.surfaceVar)
                    .clipShape(RoundedRectangle(cornerRadius: 6))
                }
            }

            Divider()

            // ── Turn policy ──
            Text("轮次策略").font(.headline)
            Picker("", selection: $scenario.turnPolicy) {
                ForEach(TurnPolicy.allCases, id: \.self) { Text($0.label).tag($0) }
            }.pickerStyle(.segmented)
            switch scenario.turnPolicy {
            case .scripted:
                Text("用户作答后按此顺序发言(用 id),到用户回合停下。").font(.caption).foregroundStyle(Theme.onSurfaceVar)
                TextField("顺序,逗号分隔", text: Binding(
                    get: { scenario.scriptOrder?.joined(separator: ",") ?? "" },
                    set: { scenario.scriptOrder = $0.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) } }
                )).textFieldStyle(.roundedBorder)
            case .roundRobin:
                Text("演员表顺序轮流发言。").font(.caption).foregroundStyle(Theme.onSurfaceVar)
            case .moderator:
                Text("由主持人决定下一位发言者。").font(.caption).foregroundStyle(Theme.onSurfaceVar)
                Picker("主持人", selection: Binding(
                    get: { scenario.moderatorId ?? scenario.agents.first?.id ?? "" },
                    set: { scenario.moderatorId = $0 }
                )) {
                    ForEach(scenario.agents) { Text($0.name).tag($0.id) }
                }
            }

            Divider()
            // ── Opener ──
            Text("首轮发起").font(.headline)
            HStack {
                Picker("开场角色", selection: Binding(
                    get: { scenario.openerAgentId ?? "" },
                    set: { scenario.openerAgentId = $0.isEmpty ? nil : $0 }
                )) {
                    Text("(用户先发言)").tag("")
                    ForEach(scenario.agents) { Text($0.name).tag($0.id) }
                }
            }
            TextField("开场白(可选)", text: Binding(
                get: { scenario.openerLine ?? "" },
                set: { scenario.openerLine = $0.isEmpty ? nil : $0 }
            )).textFieldStyle(.roundedBorder)
                }   // close inner editor VStack
            }   // close ScrollView

            Divider()
            // Inline validation message — shown when 保存 found a problem and
            // refused to close. Names the first failing field so the user
            // knows exactly what to fill in.
            if let err = saveError {
                HStack(spacing: 6) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(Theme.errorC)
                    Text(err).font(.oCaption).foregroundStyle(Theme.errorC)
                }
                .padding(.horizontal, 10).padding(.vertical, 6)
                .background(Theme.errorC.opacity(0.12), in: RoundedRectangle(cornerRadius: 8))
            }
            HStack {
                Spacer()
                Button("取消", role: .cancel, action: onClose).keyboardShortcut(.escape)
                Button("保存") {
                    if let err = Self.validate(scenario) {
                        saveError = err
                        return
                    }
                    saveError = nil
                    store.upsert(scenario)
                    onClose()
                }.keyboardShortcut(.defaultAction)
            }
        }
        .frame(width: 560, height: 640)
        .padding(16)
    }

    /// Validate a scenario before saving. Returns the first problem found, or
    /// nil if the scenario is launchable. Mirrors the engine's own checks
    /// (group_chat.rs: `members.is_empty()`, scripted order / moderator /
    /// opener must reference existing members) so a saved scenario is always
    /// startable from the sidebar without hitting "场景启动失败".
    static func validate(_ sc: Scenario) -> String? {
        if sc.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return "请填写场景名。"
        }
        if sc.agents.isEmpty {
            return "至少需要一个智能体(演员表不能为空)。"
        }
        let ids = Set(sc.agents.map { $0.id })
        for (i, a) in sc.agents.enumerated() {
            if a.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                return "第 \(i + 1) 个智能体缺少名字。"
            }
            if a.systemPrompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                return "智能体「\(a.name)」缺少系统提示词。"
            }
        }
        if let order = sc.scriptOrder {
            for id in order where !ids.contains(id) {
                return "轮次顺序引用了不存在的角色 id「\(id)」。"
            }
        }
        if let mid = sc.moderatorId, !mid.isEmpty, !ids.contains(mid) {
            return "主持人 id「\(mid)」不在演员表中。"
        }
        if let op = sc.openerAgentId, !op.isEmpty, !ids.contains(op) {
            return "开场角色 id「\(op)」不在演员表中。"
        }
        if sc.turnPolicy == .moderator {
            let mid = sc.moderatorId ?? ""
            if mid.isEmpty {
                return "主持人策略需要选择一个主持人。"
            }
        }
        if let debrief = sc.debrief, !ids.contains(debrief.debriefMemberId) {
            return "结束阶段的接管角色不在演员表中。"
        }
        return nil
    }

    /// One-line summary of a topic field's per-member visibility for the Menu label.
    private func visibilityLabel(for f: TopicField, in sc: Scenario) -> String {
        guard let v = f.visibleTo, !v.isEmpty else { return "全员可见" }
        let names = v.compactMap { sc.agent($0)?.name }
        let shown = names.isEmpty ? v.joined(separator: ",") : names.joined(separator: "/")
        return "仅 \(shown) 可见"
    }
}

/// Binding helper: present an optional `[Element]` as a non-optional list,
/// materializing an empty array (and writing it back) when the source is nil.
/// Lets `ForEach($scenario.topicFields.boundList)` edit a `nil`-able array.
private extension Binding where Value == [TopicField]? {
    var boundList: Binding<[TopicField]> {
        Binding<[TopicField]>(
            get: { self.wrappedValue ?? [] },
            set: { newValue in
                self.wrappedValue = newValue.isEmpty ? nil : newValue
            }
        )
    }
}

private struct AgentCard: View {
    @Binding var agent: Agent
    let allowDelete: Bool
    let onDelete: () -> Void
    @State private var expanded: Bool = true

    private let palettes = ["#4D6BFE", "#3B8C5A", "#B68C2E", "#E5484D", "#8A8A8A", "#9B59B6"]

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Image(systemName: agent.avatar).foregroundStyle(Color(hex: agent.color))
                TextField("名字", text: $agent.name).textFieldStyle(.roundedBorder)
                Spacer()
                Button { withAnimation { expanded.toggle() } } label: {
                    Image(systemName: expanded ? "chevron.down" : "chevron.right")
                }.buttonStyle(.borderless)
                if allowDelete {
                    Button(action: onDelete) { Image(systemName: "trash") }.buttonStyle(.borderless)
                }
            }
            if expanded {
                TextField("角色(简短)", text: $agent.role).textFieldStyle(.roundedBorder)
                VStack(alignment: .leading) {
                    Text("系统提示词").font(.caption).foregroundStyle(Theme.onSurfaceVar)
                    TextEditor(text: $agent.systemPrompt)
                        .font(.system(size: 12)).frame(minHeight: 60, maxHeight: 120)
                        .scrollContentBackground(.hidden).background(Theme.surfaceVar)
                        .clipShape(RoundedRectangle(cornerRadius: 6))
                }
                HStack {
                    TextField("model(空=继承)", text: Binding(
                        get: { agent.model ?? "" },
                        set: { agent.model = $0.isEmpty ? nil : $0 }
                    )).textFieldStyle(.roundedBorder)
                    Picker("配色", selection: $agent.color) {
                        ForEach(palettes, id: \.self) { c in
                            HStack { Circle().fill(Color(hex: c)).frame(width: 12, height: 12); Text(c) }.tag(c)
                        }
                    }
                }
                HStack {
                    TextField("头像 SF symbol", text: $agent.avatar).textFieldStyle(.roundedBorder)
                    Spacer()
                    Text("kind/model 留空继承设置").font(.caption2).foregroundStyle(Theme.onSurfaceVar)
                }
            }
        }
        .padding(10)
        .background(Theme.secondaryCont)
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }
}
