// ScenarioEditor — build/edit a multi-agent scenario (cast + turn policy +
// opener). Shown as a sheet from the sidebar's "新场景"/"编辑场景" actions.

import SwiftUI

struct ScenarioEditor: View {
    @State var scenario: Scenario
    @ObservedObject var store: AgentStore
    let onClose: () -> Void

    var body: some View {
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

            HStack {
                Spacer()
                Button("取消", role: .cancel, action: onClose).keyboardShortcut(.escape)
                Button("保存") {
                    store.upsert(scenario)
                    onClose()
                }.keyboardShortcut(.defaultAction)
            }
        }
        .frame(width: 560, height: 640)
        .padding(16)
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
