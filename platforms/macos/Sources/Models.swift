// Agent + Scenario + TurnPolicy — Swift-side models for multi-agent
// scenarios. Codable so a scenario library persists to JSON in Application
// Support. An Agent is an AI persona (the user is an implicit extra
// participant, never stored as an Agent).

import Foundation

/// Turn policy for a scenario — mirrors the engine `TurnPolicy`.
enum TurnPolicy: String, Codable, CaseIterable {
    case scripted    // fixed order after each user input
    case roundRobin  // members cycle in list order
    case moderator   // a moderator member picks the next speaker

    var label: String {
        switch self {
        case .scripted:   return "脚本式"
        case .roundRobin: return "轮询"
        case .moderator:  return "主持人选择"
        }
    }
    /// The string the FFI `ScenarioSpecView.turn_policy` expects.
    var specValue: String {
        switch self {
        case .scripted:   return "scripted"
        case .roundRobin: return "roundrobin"
        case .moderator:  return "moderator"
        }
    }
}

/// An AI persona in a scenario.
struct Agent: Codable, Identifiable, Hashable {
    var id: String
    var name: String
    var role: String          // short role label
    var systemPrompt: String
    /// Model name. `nil` ⇒ inherit the app's configured model (so a user who
    /// set up glm5.2 in Settings gets glm5.2 for every scenario member without
    /// editing each one). Override per-agent to mix models/vendors.
    var model: String?
    var color: String         // hex, e.g. "#4D6BFE"
    var avatar: String        // SF symbol name
    // Provider overrides — nil ⇒ inherit the app's configured provider
    // (kind/apiKey/baseUrl from Settings). Set per-agent to mix models/vendors.
    var kind: String?
    var apiKey: String?
    var baseUrl: String?

    /// Build the FFI `AgentSpecView`, inheriting provider config from the app
    /// settings where the agent leaves it nil. When `background` is non-empty
    /// it is appended to the persona system prompt as scenario background —
    /// so every member *knows* the topic (the interviewer asks targeted
    /// questions about the position; the coach critiques in that context)
    /// rather than asking the user to supply it.
    func specView(defaultKind: String, defaultApiKey: String, defaultBaseUrl: String,
                  defaultModel: String, background: String) -> AgentSpecView {
        let prompt = background.isEmpty ? systemPrompt : "\(systemPrompt)\n\n\(background)"
        return AgentSpecView(
            id: id,
            name: name,
            systemPrompt: prompt,
            kind: kind ?? defaultKind,
            model: model ?? defaultModel,
            apiKey: apiKey ?? (defaultApiKey.isEmpty ? nil : defaultApiKey),
            baseUrl: baseUrl ?? (defaultBaseUrl.isEmpty ? nil : defaultBaseUrl),
            color: color,
            avatar: avatar
        )
    }
}

/// One input field the user must fill before starting a scenario (e.g.
/// "应聘岗位"). A scenario with `topicFields` non-empty prompts the inline
/// intake page on start; the collected values are baked into every member's
/// system prompt as background and into the session title.
///
/// `visibleTo` controls per-member visibility of the collected value: `nil`
/// means the value is folded into ALL members' background (default — e.g. the
/// interview's "应聘岗位" is shared context). A non-empty array restricts the
/// value to only those member ids (e.g. the interviewee's "项目经历" is
/// `["coach"]` so the coach can give specific advice but the interviewer never
/// sees it and can't ask about it).
struct TopicField: Codable, Identifiable, Hashable {
    var id: String
    var label: String
    var placeholder: String?
    /// Member ids allowed to see this field's value in their system prompt.
    /// `nil` = visible to all members. Non-empty = only those members.
    var visibleTo: [String]? = nil
}

/// Optional "debrief" phase config for a scenario. After the user triggers
/// the debrief (a button in the top bar), the turn policy is switched to a
/// scripted order containing only `debriefMemberId` (e.g. coach), and the
/// `summaryPrompt` is sent to that member for a full-session summary. The
/// user can then keep asking that member follow-up questions — the other
/// members (e.g. the interviewer) no longer participate.
struct DebriefConfig: Codable, Hashable {
    var buttonLabel: String          // e.g. "结束面试"
    var summaryPrompt: String        // sent to the debrief member
    var debriefMemberId: String      // the member that takes over (e.g. "coach")
}

/// Optional review-revise loop (e.g. writing workshop: writer drafts → editor
/// reviews → writer revises → editor re-reviews → … until the editor approves
/// or `maxRounds` is reached). The reviewer's persona prompt must instruct it
/// to emit `approveMarker` when satisfied. `nil` = single pass, no loop.
struct ReviewLoopConfig: Codable, Hashable {
    var reviewerId: String           // member id that decides approval (e.g. "editor")
    var approveMarker: String         // substring the reviewer emits when satisfied (e.g. "定稿")
    var maxRounds: Int                // total scripted passes to run at most (1 = no loop)
}

/// A multi-agent scenario — a cast of personas + a turn policy.
struct Scenario: Codable, Identifiable, Hashable {
    var id: String
    var name: String
    var icon: String                 // SF symbol for the sidebar
    var agents: [Agent]              // AI personas only (user is implicit)
    var turnPolicy: TurnPolicy
    var scriptOrder: [String]?       // .scripted — member ids after each user input
    var moderatorId: String?        // .moderator — member id that picks next
    var openerAgentId: String?      // who opens; nil = user first
    var openerLine: String?
    /// Topic-intake form fields. Non-empty ⇒ a form sheet prompts for these on
    /// start. `nil`/empty ⇒ start directly.
    var topicFields: [TopicField]?
    /// Optional debrief phase (e.g. interview → coach summary + Q&A). `nil`
    /// ⇒ no debrief button.
    var debrief: DebriefConfig?
    /// Optional review-revise loop (e.g. writing workshop). `nil` = single pass.
    var reviewLoop: ReviewLoopConfig?

    /// Build the FFI `ScenarioSpecView`, inheriting provider config per-agent.
    /// `topicValues` (keyed by field id) is rendered into a per-member background
    /// block ("【场景背景】\n岗位: X\n…") folded into each member's system prompt,
    /// and into the session title ("场景名·v1·v2…").
    ///
    /// Per-member visibility: a field with `visibleTo` is only folded into the
    /// background of the listed members (e.g. the interviewee's "项目经历" with
    /// `visibleTo: ["coach"]` reaches the coach but NOT the interviewer — so
    /// the coach can reference project specifics while the interviewer can't
    /// ask about them). Fields with `visibleTo == nil` reach everyone.
    func specView(defaultKind: String, defaultApiKey: String, defaultBaseUrl: String,
                  defaultModel: String, topicValues: [String: String]?) -> ScenarioSpecView {
        // Pre-render the per-field (label, value) pairs, dropping blanks.
        let fields = topicFields ?? []
        let pairs: [(field: TopicField, value: String)] = fields.compactMap { f in
            let v = (topicValues?[f.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            return v.isEmpty ? nil : (f, v)
        }
        // Title suffix uses every non-blank value regardless of visibility
        // (the title is conversation-scoped, not per-member).
        let titleParts = pairs.map { $0.value }
        let title = titleParts.isEmpty ? name : "\(name)·" + titleParts.joined(separator: "·")

        let members = self.agents.map { agent in
            // Background for THIS member: only fields it's allowed to see.
            let visible = pairs.filter { p in
                guard let allowed = p.field.visibleTo else { return true }   // nil → all
                return allowed.contains(agent.id)
            }
            let lines = visible.map { "\($0.field.label): \($0.value)" }
            let background = lines.isEmpty ? "" : "【场景背景】\n" + lines.joined(separator: "\n")
            return agent.specView(defaultKind: defaultKind,
                                  defaultApiKey: defaultApiKey,
                                  defaultBaseUrl: defaultBaseUrl,
                                  defaultModel: defaultModel,
                                  background: background)
        }
        return ScenarioSpecView(
            members: members,
            turnPolicy: turnPolicy.specValue,
            scriptOrder: scriptOrder,
            moderatorId: moderatorId,
            openerAgentId: openerAgentId,
            openerLine: openerLine,
            title: title,
            reviewLoop: reviewLoop.map { rl in
                ReviewLoopSpecView(reviewerId: rl.reviewerId,
                                   approveMarker: rl.approveMarker,
                                   maxRounds: UInt64(rl.maxRounds))
            }
        )
    }

    /// Resolve an agent by id (for speaker-name + color lookup during rendering).
    func agent(_ id: String) -> Agent? {
        agents.first { $0.id == id }
    }
}
