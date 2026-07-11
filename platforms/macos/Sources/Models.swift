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
/// "应聘岗位"). A scenario with `topicFields` non-empty prompts a form sheet
/// on start; the collected values are baked into every member's system prompt
/// as background and into the session title.
struct TopicField: Codable, Identifiable, Hashable {
    var id: String
    var label: String
    var placeholder: String?
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

    /// Build the FFI `ScenarioSpecView`, inheriting provider config per-agent.
    /// `topicValues` (keyed by field id) is rendered into a background block
    /// ("【场景背景】\n岗位: X\n目标公司: Y\n…") folded into each member's
    /// system prompt, and into the session title ("场景名·v1·v2…").
    func specView(defaultKind: String, defaultApiKey: String, defaultBaseUrl: String,
                  defaultModel: String, topicValues: [String: String]?) -> ScenarioSpecView {
        // Render the field values into a background block + a compact title suffix.
        var lines: [String] = []
        var titleParts: [String] = []
        if let fields = topicFields, let vals = topicValues {
            for f in fields {
                let v = (vals[f.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
                if v.isEmpty { continue }
                lines.append("\(f.label): \(v)")
                titleParts.append(v)
            }
        }
        let background = lines.isEmpty ? "" : "【场景背景】\n" + lines.joined(separator: "\n")
        let title = titleParts.isEmpty ? name : "\(name)·" + titleParts.joined(separator: "·")
        return ScenarioSpecView(
            members: agents.map { $0.specView(defaultKind: defaultKind,
                                              defaultApiKey: defaultApiKey,
                                              defaultBaseUrl: defaultBaseUrl,
                                              defaultModel: defaultModel,
                                              background: background) },
            turnPolicy: turnPolicy.specValue,
            scriptOrder: scriptOrder,
            moderatorId: moderatorId,
            openerAgentId: openerAgentId,
            openerLine: openerLine,
            title: title
        )
    }

    /// Resolve an agent by id (for speaker-name + color lookup during rendering).
    func agent(_ id: String) -> Agent? {
        agents.first { $0.id == id }
    }
}
