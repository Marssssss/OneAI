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
/// v6: presets are now locale-aware (zh / en); an English-system user re-seeds
/// to the English preset set (names, prompts, approve marker).
private let SCENARIO_SCHEMA_VERSION = 6

final class AgentStore: ObservableObject {
    @Published var scenarios: [Scenario] = []

    /// The sidecar JSON-RPC client (set by `ChatViewModel` once the app-server
    /// child is up). `nil` ⇒ FFI transport ⇒ everything stays local-file.
    /// `weak` so tearing down the VM's `rpcClient` auto-nils the store's ref
    /// without a second write site.
    weak var rpcClient: OneAiRpcClient?

    private let fileURL: URL = {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("oneai_scenarios.json")
    }()

    init() {
        load()
        if scenarios.isEmpty {
            scenarios = Self.presets(locale: AppLocale.current)
            save()
        }
    }

    // MARK: - CRUD

    /// Insert or replace by id. In sidecar mode custom scenarios (id does not
    /// start with `preset-`) are upserted to the shared server store first,
    /// then mirrored into the local cache; presets stay local (they're
    /// per-frontend, locale-bound defaults — not shared library). Async so the
    /// RPC round-trip can suspend off the main actor.
    func upsert(_ scenario: Scenario) async {
        let isPreset = scenario.id.hasPrefix("preset-")
        if !isPreset, let rpc = rpcClient, let dto = scenario.toBusDTO().asJSONObject {
            do {
                let res = try await rpc.call("scenario/upsert", params: ["scenario": dto])
                // upsert returns {ok:true, id} or {ok:false, errors:[…]} when
                // the scenario fails validate. The editor validates first, so
                // a false-ok is unexpected — log + still cache locally.
                if (res["ok"] as? Bool) == false {
                    storeLog("scenario/upsert rejected by server validate")
                }
            } catch {
                storeLog("scenario/upsert err=\(error); caching locally only")
            }
        }
        await MainActor.run {
            self.upsertLocal(scenario)
            self.save()
        }
    }

    /// Remove by id. Customs are deleted from the server store in sidecar
    /// mode; presets are removed from the local cache only.
    func delete(_ scenario: Scenario) async {
        let isPreset = scenario.id.hasPrefix("preset-")
        if !isPreset, let rpc = rpcClient {
            do {
                _ = try await rpc.call("scenario/delete", params: ["id": scenario.id])
            } catch {
                storeLog("scenario/delete err=\(error); removing locally only")
            }
        }
        await MainActor.run {
            self.scenarios.removeAll { $0.id == scenario.id }
            self.save()
        }
    }

    private func upsertLocal(_ scenario: Scenario) {
        if let idx = scenarios.firstIndex(where: { $0.id == scenario.id }) {
            scenarios[idx] = scenario
        } else {
            scenarios.append(scenario)
        }
    }

    // MARK: - Sidecar scenario library (Phase G3)

    /// Pull the shared scenario library from the server store and merge with
    /// the local preset set. Called by the VM once the sidecar client is up.
    ///
    /// Merge rule: `scenarios = localPresets + serverCustoms`. Presets are
    /// ALWAYS local (locale-bound, per-frontend defaults — the server's own
    /// `preset-*` entries, e.g. the engine's minimal builtin seed, are ignored
    /// so the richer localized presets win). Customs (user-created, non-preset
    /// ids) are server-authoritative + shared across frontends.
    ///
    /// First sidecar connect: the server has no customs yet, so the local
    /// customs are migrated to the shared store (a user coming from FFI mode
    /// doesn't lose their custom scenarios). After that, the server is the
    /// authority for customs; a custom deleted on another frontend disappears
    /// here on the next refresh.
    func refresh() async {
        guard let rpc = rpcClient else { return }
        do {
            let res = try await rpc.call("scenario/list", params: [String: String]())
            var serverCustoms = Scenario.fromListResult(res)
                .filter { !$0.id.hasPrefix("preset-") }
            if serverCustoms.isEmpty {
                // First connect — push the local customs up so they're shared.
                let localCustoms = scenarios.filter { !$0.id.hasPrefix("preset-") }
                for c in localCustoms {
                    if let dto = c.toBusDTO().asJSONObject {
                        _ = try? await rpc.call("scenario/upsert", params: ["scenario": dto])
                    }
                }
                serverCustoms = localCustoms
            }
            let localPresets = scenarios.filter { $0.id.hasPrefix("preset-") }
            let merged = localPresets + serverCustoms
            await MainActor.run {
                self.scenarios = merged
                self.save()
            }
        } catch {
            // Server unavailable — keep the local cache (loaded at init). The
            // sidebar still works offline; edits fall back to local-only.
            storeLog("scenario/list err=\(error); using local cache")
        }
    }

    /// Validate a scenario before saving. Sidecar: ask the engine for the
    /// authoritative check (`scenario/validate`) — kills the per-frontend
    /// mirror drift. FFI: the local mirror (the engine validate lives in
    /// `oneai-bus`, not exposed over FFI for a direct call, so the mirror is
    /// the pragmatic local check). Returns the first problem as a localized
    /// message, or nil if launchable.
    func validate(_ scenario: Scenario) async -> String? {
        if let rpc = rpcClient, let dto = scenario.toBusDTO().asJSONObject {
            do {
                let res = try await rpc.call("scenario/validate", params: ["scenario": dto])
                let ok = (res["ok"] as? Bool) ?? true
                if ok { return nil }
                if let errs = res["errors"] as? [[String: Any]], !errs.isEmpty {
                    let first = errs.compactMap { dict -> ScenarioErrorDTO? in
                        guard let data = try? JSONSerialization.data(withJSONObject: dict) else { return nil }
                        return try? JSONDecoder().decode(ScenarioErrorDTO.self, from: data)
                    }.first
                    if let first { return ScenarioErrorLocalizer.message(for: first) }
                }
                return nil
            } catch {
                storeLog("scenario/validate err=\(error); falling back to local mirror")
            }
        }
        return Self.validateLocal(scenario)
    }

    private func storeLog(_ msg: String) {
        var s = msg
        s.append("\n")
        FileHandle.standardError.write(s.data(using: .utf8) ?? Data())
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
        scenarios = Self.presets(locale: AppLocale.current) + customs
        save()
    }

    // MARK: - Built-in presets

    /// The FFI/local validate mirror — the pragmatic launchability check when
    /// the sidecar server isn't available. Mirrors `BusScenario::validate`
    /// (`crates/oneai-bus`): name non-empty, ≥1 member, each member named +
    /// has a system prompt, scripted order / moderator / opener / debrief /
    /// reviewer ids must reference existing members. Returns the first problem
    /// as a localized message, or nil if launchable.
    static func validateLocal(_ sc: Scenario) -> String? {
        if sc.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return NSLocalizedString("请填写场景名。", comment: "")
        }
        if sc.agents.isEmpty {
            return NSLocalizedString("至少需要一个智能体(演员表不能为空)。", comment: "")
        }
        let ids = Set(sc.agents.map { $0.id })
        for (i, a) in sc.agents.enumerated() {
            if a.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                return String(format: NSLocalizedString("第 %d 个智能体缺少名字。", comment: ""), i + 1)
            }
            if a.systemPrompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                return String(format: NSLocalizedString("智能体「%@」缺少系统提示词。", comment: ""), a.name)
            }
        }
        if let order = sc.scriptOrder {
            for id in order where !ids.contains(id) {
                return String(format: NSLocalizedString("轮次顺序引用了不存在的角色 id「%@」。", comment: ""), id)
            }
        }
        if let mid = sc.moderatorId, !mid.isEmpty, !ids.contains(mid) {
            return String(format: NSLocalizedString("主持人 id「%@」不在演员表中。", comment: ""), mid)
        }
        if let op = sc.openerAgentId, !op.isEmpty, !ids.contains(op) {
            return String(format: NSLocalizedString("开场角色 id「%@」不在演员表中。", comment: ""), op)
        }
        if sc.turnPolicy == .moderator {
            let mid = sc.moderatorId ?? ""
            if mid.isEmpty {
                return NSLocalizedString("主持人策略需要选择一个主持人。", comment: "")
            }
        }
        if let debrief = sc.debrief, !ids.contains(debrief.debriefMemberId) {
            return NSLocalizedString("结束阶段的接管角色不在演员表中。", comment: "")
        }
        return nil
    }

    /// The five preset scenarios shipped with the app, localized for the
    /// effective `locale`. IDs are stable so a user can edit a preset (it
    /// overwrites in place via `upsert`); the locale only selects which
    /// language variant of the names / prompts / markers ships. `zh` is the
    /// historical Chinese set; `en` ships English names + English persona
    /// prompts + an English review-loop marker (`"approved"` ↔ `"定稿"`) so an
    /// English-locale scenario drives the LLM in English end-to-end (the
    /// engine prompt locale in `ScenarioSpecView.locale` matches — see
    /// `AppLocale.current` / `Scenario.specView`).
    static func presets(locale: AppLocale) -> [Scenario] {
        switch locale {
        case .en: return enPresets
        case .zh: return zhPresets
        }
    }

    static let zhPresets: [Scenario] = [
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

    /// English-locale preset set — names, persona prompts, topic-field labels,
    /// opener lines, debrief text, and the review-loop marker (`"approved"`)
    /// all in English. Structure (ids, colors, icons, turn policies, script
    /// orders) is identical to `zhPresets` so behavior matches; only the
    /// human-/LLM-facing text is translated. The `"approved"` marker pairs
    /// with `ChatLocale.en` engine prompts (the editor is told to emit
    /// `"approved"`; the engine matches it substring-wise to approve).
    static let enPresets: [Scenario] = [
        Scenario(
            id: "preset-interview",
            name: "Interview Practice",
            icon: "person.crop.circle.badge.questionmark",
            agents: [
                Agent(id: "interviewer", name: "Interviewer", role: "Asks questions",
                      systemPrompt: """
                      You are a senior technical interviewer. Your job is to ask in-depth, \
                      progressive questions about the position the candidate is applying for. \
                      Ask only one question at a time; follow up or change direction only after \
                      the candidate answers. Do not answer for the candidate, and do not give \
                      coaching feedback — that is the coach's job. Keep a professional, measured tone.
                      """,
                      model: nil, color: "#4D6BFE",
                      avatar: "person.crop.circle.badge.questionmark", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "coach", name: "Coach", role: "Feedback",
                      systemPrompt: """
                      You are an interview coach. After each of the candidate's answers, give \
                      targeted feedback: what was strong, what was weak, how to improve, plus a \
                      short, actionable "next step". Be specific and practical. Do not answer \
                      the interviewer's questions for the candidate. If the candidate's project \
                      experience is provided in [Scenario Background], tie your advice to those \
                      projects at a project-specific level (the interviewer does not see this — \
                      you use it only for coaching).
                      """,
                      model: nil, color: "#3B8C5A",
                      avatar: "person.crop.circle.badge.checkmark", kind: nil, apiKey: nil, baseUrl: nil),
            ],
            turnPolicy: .scripted,
            // candidate answers → coach feedback → interviewer follow-up
            scriptOrder: ["coach", "interviewer"],
            moderatorId: nil,
            openerAgentId: "interviewer",
            openerLine: "Let's begin the interview. Please give a brief self-introduction.",
            topicFields: [
                TopicField(id: "position", label: "Position", placeholder: "e.g. Frontend Engineer, 3 years"),
                TopicField(id: "company", label: "Target company", placeholder: "e.g. ByteDance"),
                TopicField(id: "level", label: "Level", placeholder: "e.g. Mid-level"),
                // Project experience is injected only into the coach's background
                // (visibleTo: ["coach"]) — the interviewer never sees it and won't
                // ask about it, but the coach can give project-specific advice.
                TopicField(id: "projects", label: "Project experience", placeholder: "e.g. E-commerce order platform, owned inventory & payment modules; multiple allowed",
                           visibleTo: ["coach"]),
            ],
            debrief: DebriefConfig(
                buttonLabel: "End interview",
                summaryPrompt: "(Interview over.) As the coach, give an overall summary of the candidate's performance in this interview: highlights, weaknesses, areas to improve, and follow-up learning and practice recommendations.",
                debriefMemberId: "coach"
            ),
            reviewLoop: nil
        ),
        Scenario(
            id: "preset-language-partner",
            name: "Language Partner",
            icon: "globe",
            agents: [
                Agent(id: "partner", name: "Language Partner", role: "Practice",
                      systemPrompt: """
                      You are a foreign-language practice partner. Hold a natural conversation \
                      with the user, adjust difficulty to their level, gently correct wording \
                      and grammar mistakes as they come up, and offer more idiomatic phrasing. \
                      Advance the topic one step at a time. Speak the language specified under \
                      "Language · Topic" in [Scenario Background]; if the user didn't specify \
                      a language, default to English.
                      """,
                      model: nil, color: "#B68C2E",
                      avatar: "globe", kind: nil, apiKey: nil, baseUrl: nil),
            ],
            turnPolicy: .roundRobin,
            scriptOrder: nil, moderatorId: nil,
            openerAgentId: "partner",
            openerLine: "Open naturally in the language and topic specified in the background, and start chatting with the user.",
            topicFields: [
                TopicField(id: "topic", label: "Language · Topic", placeholder: "e.g. Chinese · Travel"),
            ],
            debrief: nil,
            reviewLoop: nil
        ),
        Scenario(
            id: "preset-debate",
            name: "Debate",
            icon: "scalemass",
            agents: [
                Agent(id: "pro", name: "Pro Debater", role: "For",
                      systemPrompt: "You are the pro debater. Argue from the supporting position — sharp viewpoints, strong supporting reasoning.",
                      model: nil, color: "#4D6BFE",
                      avatar: "arrow.up.circle", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "con", name: "Con Debater", role: "Against",
                      systemPrompt: "You are the con debater. Argue from the opposing position — counter the pro side point for point, with solid reasoning.",
                      model: nil, color: "#E5484D",
                      avatar: "arrow.down.circle", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "moderator", name: "Moderator", role: "Facilitator",
                      systemPrompt: "You are the debate moderator. In the first round, state today's motion and invite the pro side to open; thereafter, each round reply only with the next speaker's role id (pro/con/user) — nothing else — and keep both sides balanced.",
                      model: nil, color: "#8A8A8A",
                      avatar: "scalemass", kind: nil, apiKey: nil, baseUrl: nil),
            ],
            turnPolicy: .moderator,
            scriptOrder: nil,
            moderatorId: "moderator",
            openerAgentId: "moderator",
            openerLine: "Please open: state today's motion and invite the pro side to begin.",
            topicFields: [
                TopicField(id: "motion", label: "Debate motion", placeholder: "e.g. Will AI replace humans"),
            ],
            debrief: nil,
            reviewLoop: nil
        ),
        Scenario(
            id: "preset-writing-workshop",
            name: "Writing Workshop",
            icon: "pencil.line",
            agents: [
                Agent(id: "writer", name: "Writer", role: "Drafts",
                      systemPrompt: """
                      You are the writer. Draft an initial piece based on the user's topic, \
                      focused on structure and expression. When the editor gives revision \
                      feedback, revise your draft accordingly and output the complete draft — \
                      don't just describe the changes.
                      """,
                      model: nil, color: "#4D6BFE",
                      avatar: "pencil.line", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "editor", name: "Editor", role: "Refines",
                      systemPrompt: """
                      You are the editor. Give specific, actionable revision suggestions on \
                      the writer's draft and explain your reasoning. After each review you \
                      must state a clear verdict: if the draft has reached a quality ready \
                      to finalize, include the word "approved" in your reply to signal \
                      approval; otherwise point out what needs changing and hand it back to \
                      the writer. Do not rewrite the whole piece for the writer.
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
                TopicField(id: "topic", label: "Writing topic", placeholder: "e.g. An essay about autumn"),
            ],
            debrief: nil,
            // writer drafts → editor reviews → writer revises → editor re-reviews → …,
            // until the editor includes "approved" or the 3-round cap (incl. the
            // first pass) is reached, preventing infinite revision.
            reviewLoop: ReviewLoopConfig(reviewerId: "editor", approveMarker: "approved", maxRounds: 3)
        ),
        Scenario(
            id: "preset-brainstorm",
            name: "Brainstorm",
            icon: "lightbulb",
            agents: [
                Agent(id: "ideator", name: "Ideator", role: "Diverge",
                      systemPrompt: "You are the ideator. Quickly produce diverse, unconventional ideas around the user's topic — give 3 each time with a brief rationale.",
                      model: nil, color: "#B68C2E",
                      avatar: "lightbulb", kind: nil, apiKey: nil, baseUrl: nil),
                Agent(id: "critic", name: "Critic", role: "Converge",
                      systemPrompt: "You are the critic. Surface risks and feasibility issues with the ideator's ideas, and flag the single most promising one.",
                      model: nil, color: "#3B8C5A",
                      avatar: "checkmark.seal", kind: nil, apiKey: nil, baseUrl: nil),
            ],
            turnPolicy: .scripted,
            scriptOrder: ["ideator", "critic"],
            moderatorId: nil,
            openerAgentId: "ideator",
            openerLine: "Please give the first batch of ideas on today's topic, with a brief rationale for each.",
            topicFields: [
                TopicField(id: "topic", label: "Brainstorm topic", placeholder: "e.g. Ideas to improve user retention"),
            ],
            debrief: nil,
            reviewLoop: nil
        ),
    ]

    /// Resolve an agent across all scenarios by id (for rendering speaker names
    /// in a running conversation). Returns (name, color, avatar).
    static func speakerMeta(for speakerId: String, in scenario: Scenario?) -> (String, String, String) {
        if speakerId == "user" || speakerId.isEmpty {
            return (NSLocalizedString("你", comment: ""), "#8A8A8A", "person.crop.circle")
        }
        if let a = scenario?.agent(speakerId) {
            return (a.name, a.color, a.avatar)
        }
        return (speakerId, "#8A8A8A", "person.crop.circle")
    }
}
