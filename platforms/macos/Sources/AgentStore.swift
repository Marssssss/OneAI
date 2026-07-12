// AgentStore — CRUD + persistence for Agents & Scenarios, plus the built-in
// preset scenarios (面试演练 / 语言伙伴 / 辩论 / 写作工坊 / 头脑风暴).
// Persists to ~/Library/Application Support/oneai_scenarios.json so
// user-edited scenarios survive restarts.

import Foundation

/// On-disk wrapper: a schema version + the scenario list. Bumping `version`
/// re-seeds the built-in presets (preserving user-added custom scenarios) so
/// structural preset changes (new fields, debrief config) reach users whose
/// disk already holds an older scenario file.
private struct ScenarioStoreData: Codable {
    var version: Int
    var scenarios: [Scenario]
}

/// Bump when the preset structure changes — triggers a preset re-seed on load.
private let SCENARIO_SCHEMA_VERSION = 5

final class AgentStore: ObservableObject {
    @Published var scenarios: [Scenario] = []

    private let fileURL: URL = {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("oneai_scenarios.json")
    }()

    init() {
        load()
        if scenarios.isEmpty {
            scenarios = Self.presets
            save()
        }
    }

    // MARK: - CRUD

    func upsert(_ scenario: Scenario) {
        if let idx = scenarios.firstIndex(where: { $0.id == scenario.id }) {
            scenarios[idx] = scenario
        } else {
            scenarios.append(scenario)
        }
        save()
    }

    func delete(_ scenario: Scenario) {
        scenarios.removeAll { $0.id == scenario.id }
        save()
    }

    // MARK: - Persistence

    private func load() {
        guard let data = try? Data(contentsOf: fileURL) else { return }

        // New wrapper format: { version, scenarios }.
        if let wrapped = try? JSONDecoder().decode(ScenarioStoreData.self, from: data) {
            scenarios = wrapped.scenarios
            if wrapped.version < SCENARIO_SCHEMA_VERSION {
                reseedPresets()
            }
            return
        }
        // Legacy format: bare [Scenario] (pre-wrapper). Decode, then re-seed
        // presets to migrate to the new schema.
        if let decoded = try? JSONDecoder().decode([Scenario].self, from: data) {
            scenarios = decoded
            reseedPresets()
        }
    }

    private func save() {
        let wrapped = ScenarioStoreData(version: SCENARIO_SCHEMA_VERSION, scenarios: scenarios)
        guard let data = try? JSONEncoder().encode(wrapped) else { return }
        try? data.write(to: fileURL, options: .atomic)
    }

    /// Replace every built-in preset (id starts with "preset-") with the
    /// current code-defined version, leaving user-added custom scenarios
    /// untouched. Used when the on-disk schema is older than the current one.
    private func reseedPresets() {
        let customs = scenarios.filter { !$0.id.hasPrefix("preset-") }
        scenarios = Self.presets + customs
        save()
    }

    // MARK: - Built-in presets

    /// The five preset scenarios shipped with the app. IDs are stable so a
    /// user can edit a preset (it overwrites in place via `upsert`).
    static let presets: [Scenario] = [
        Scenario(
            id: "preset-interview",
            name: "面试演练",
            icon: "person.crop.circle.badge.questionmark",
            agents: [
                Agent(id: "interviewer", name: "面试官", role: "提问",
                      systemPrompt: """
                      你是一名资深技术面试官。你的任务是就用户应聘的岗位提出有深度、循序渐进的问题。\
                      每次只问一个问题，等用户回答后再追问或换方向。不要替用户回答，\
                      不要给出指导性评价——那是指导员的工作。语气专业、克制。
                      """,
                      model: nil, color: "#4D6BFE",
                      avatar: "person.crop.circle.badge.questionmark", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "coach", name: "指导员", role: "点评",
                      systemPrompt: """
                      你是一名面试指导教练。在用户每次回答后，你给出针对性点评：哪里回答得好、\
                      哪里不足、可以怎样改进，并给出一个简短的「行动建议」。点评要具体、可执行。\
                      不要替用户回答面试官的问题。若【场景背景】中提供了候选人的项目经历，\
                      请结合其项目内容给出项目级、有针对性的建议（这些信息面试官看不到，仅你用于点评）。
                      """,
                      model: nil, color: "#3B8C5A",
                      avatar: "person.crop.circle.badge.checkmark", kind: nil, apiKey: nil, baseUrl: nil),
            ],
            turnPolicy: .scripted,
            // 用户作答 → 指导员点评 → 面试官追问
            scriptOrder: ["coach", "interviewer"],
            moderatorId: nil,
            openerAgentId: "interviewer",
            openerLine: "我们开始面试吧。请先做个简短的自我介绍。",
            topicFields: [
                TopicField(id: "position", label: "应聘岗位", placeholder: "如:前端工程师 3 年"),
                TopicField(id: "company", label: "目标公司", placeholder: "如:字节跳动"),
                TopicField(id: "level", label: "职位级别", placeholder: "如:社招 P5"),
                // 项目经历只注入指导员的背景（visibleTo:["coach"]），面试官看不到、
                // 也不会据此提问，但指导员能据此给出项目级建议。
                TopicField(id: "projects", label: "项目经历", placeholder: "如:电商订单中台,负责库存与支付模块;可写多条",
                           visibleTo: ["coach"]),
            ],
            debrief: DebriefConfig(
                buttonLabel: "结束面试",
                summaryPrompt: "（面试结束）请以指导员身份,对候选人本次面试的整体表现进行全场总结:亮点、不足、可改进之处,并给出后续学习与练习建议。",
                debriefMemberId: "coach"
            ),
            reviewLoop: nil
        ),
        Scenario(
            id: "preset-language-partner",
            name: "语言伙伴",
            icon: "globe",
            agents: [
                Agent(id: "partner", name: "语言伙伴", role: "陪练",
                      systemPrompt: """
                      你是一名外语陪练伙伴。与用户进行自然对话，根据用户水平调整难度，\
                      适时温和地纠正用词与语法错误，并给出更地道的说法。一次只推进话题一步。\
                      请使用【场景背景】中“语言·话题”所指定的语言与用户交谈；若用户未指定语言，默认用英语。
                      """,
                      model: nil, color: "#B68C2E",
                      avatar: "globe", kind: nil, apiKey: nil, baseUrl: nil),
            ],
            turnPolicy: .roundRobin,
            scriptOrder: nil, moderatorId: nil,
            openerAgentId: "partner",
            openerLine: "请按背景中指定的语言与话题自然开场，与用户聊起来。",
            topicFields: [
                TopicField(id: "topic", label: "语言·话题", placeholder: "如:中文·旅行"),
            ],
            debrief: nil,
            reviewLoop: nil
        ),
        Scenario(
            id: "preset-debate",
            name: "辩论赛",
            icon: "scalemass",
            agents: [
                Agent(id: "pro", name: "正方辩手", role: "支持",
                      systemPrompt: "你是正方辩手，从支持立场出发进行论证，观点鲜明、论据有力。",
                      model: nil, color: "#4D6BFE",
                      avatar: "arrow.up.circle", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "con", name: "反方辩手", role: "反对",
                      systemPrompt: "你是反方辩手，从反对立场出发进行论证，针锋相对、有理有据。",
                      model: nil, color: "#E5484D",
                      avatar: "arrow.down.circle", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "moderator", name: "主持人", role: "调度",
                      systemPrompt: "你是辩论主持人。首轮请点明今日辩题并邀请正方先开始立论；其后每轮只回复下一个发言者的角色 id（pro/con/user），不要回复其他内容，并确保双方均衡发言。",
                      model: nil, color: "#8A8A8A",
                      avatar: "scalemass", kind: nil, apiKey: nil, baseUrl: nil),
            ],
            turnPolicy: .moderator,
            scriptOrder: nil,
            moderatorId: "moderator",
            openerAgentId: "moderator",
            openerLine: "请开场:点明今日辩题,邀请正方先开始立论。",
            topicFields: [
                TopicField(id: "motion", label: "辩论主题", placeholder: "如:AI 是否会取代人类"),
            ],
            debrief: nil,
            reviewLoop: nil
        ),
        Scenario(
            id: "preset-writing-workshop",
            name: "写作工坊",
            icon: "pencil.line",
            agents: [
                Agent(id: "writer", name: "写手", role: "起草",
                      systemPrompt: """
                      你是写手，根据用户主题起草初稿，注重结构与表达。\
                      当编辑给出修改意见时，请据此修改你的稿件，并输出完整稿件，不要只描述改动。
                      """,
                      model: nil, color: "#4D6BFE",
                      avatar: "pencil.line", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "editor", name: "编辑", role: "润色",
                      systemPrompt: """
                      你是编辑，对写手的稿件给出具体、可执行的修改建议并说明理由。\
                      每轮审阅后必须明确表态：若稿件已达到可定稿的质量，请在回复中包含「定稿」二字以示通过；\
                      否则指出需修改之处，交回写手继续修改。不要替写手重写全文。
                      """,
                      model: nil, color: "#3B8C5A",
                      avatar: "pencil.tip.crop.circle", kind: nil, apiKey: nil, baseUrl: nil),
            ],
            turnPolicy: .scripted,
            scriptOrder: ["writer", "editor"],
            moderatorId: nil,
            openerAgentId: nil,
            openerLine: nil,
            topicFields: [
                TopicField(id: "topic", label: "写作主题", placeholder: "如:一篇关于秋天的散文"),
            ],
            debrief: nil,
            // 写手起草 → 编辑审阅 → 写手修改 → 编辑复审 → …，直到编辑在回复中
            // 包含「定稿」或达到最多 3 轮（含首轮），防止无限修改。
            reviewLoop: ReviewLoopConfig(reviewerId: "editor", approveMarker: "定稿", maxRounds: 3)
        ),
        Scenario(
            id: "preset-brainstorm",
            name: "头脑风暴",
            icon: "lightbulb",
            agents: [
                Agent(id: "ideator", name: "创意官", role: "发散",
                      systemPrompt: "你是创意官，围绕用户主题快速产出多样、不落俗套的点子，每次给 3 条并简述理由。",
                      model: nil, color: "#B68C2E",
                      avatar: "lightbulb", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "critic", name: "评审", role: "收敛",
                      systemPrompt: "你是评审，对创意官的点子挑出风险与可行性问题，并圈出最有潜力的一条。",
                      model: nil, color: "#3B8C5A",
                      avatar: "checkmark.seal", kind: nil, apiKey: nil, baseUrl: nil),
            ],
            turnPolicy: .scripted,
            scriptOrder: ["ideator", "critic"],
            moderatorId: nil,
            openerAgentId: "ideator",
            openerLine: "请围绕今天的主题,给出第一批点子,每条简述理由。",
            topicFields: [
                TopicField(id: "topic", label: "头脑风暴主题", placeholder: "如:提升产品留存的点子"),
            ],
            debrief: nil,
            reviewLoop: nil
        ),
    ]

    /// Resolve an agent across all scenarios by id (for rendering speaker names
    /// in a running conversation). Returns (name, color, avatar).
    static func speakerMeta(for speakerId: String, in scenario: Scenario?) -> (String, String, String) {
        if speakerId == "user" || speakerId.isEmpty {
            return ("你", "#8A8A8A", "person.crop.circle")
        }
        if let a = scenario?.agent(speakerId) {
            return (a.name, a.color, a.avatar)
        }
        return (speakerId, "#8A8A8A", "person.crop.circle")
    }
}
