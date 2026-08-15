// ScenarioRpc — the wire DTOs + conversions for the `scenario/*` JSON-RPC
// methods (the shared, front-end-agnostic scenario library surfaced by
// `oneai app-server`). Phase G3: macOS scenario management (CRUD + validate)
// is server-authoritative in sidecar mode; the FFI transport keeps the local
// `oneai_scenarios.json` file + the client-side validate mirror.
//
// The DTOs mirror `crates/oneai-bus/src/protocol.rs` (`BusScenario` and
// friends) field-for-field, using snake_case property names so Swift's
// synthesized Codable produces/consumes the exact serde JSON the engine
// expects — no hand-built dicts (the rich editor shape is too nested to
// build by hand safely). `Scenario` itself stays camelCase Codable (its
// local-file format is unchanged), so these DTOs are the only boundary that
// crosses casing.

import Foundation

// MARK: - Wire DTOs (snake_case == serde field names)

struct BusScenarioMemberDTO: Codable {
    var id: String
    var name: String
    /// Short UI-only label the engine drops on compile (`to_group_scenario`).
    var role: String?
    var system_prompt: String
    /// Provider kind. `""` ⇒ inherit the app's configured provider (the
    /// engine default is `openai`, but the launch path resolves `nil`→app
    /// setting via `specView`; the store only persists, so `""` round-trips
    /// back to `nil` = inherit).
    var kind: String
    /// Model name. `""` ⇒ inherit the app's configured model.
    var model: String
    var api_key: String?
    var base_url: String?
    var color: String?
    var avatar: String?
}

struct BusTopicFieldDTO: Codable {
    var id: String
    var label: String
    var placeholder: String?
    /// Member ids allowed to see this field's value. `nil` = all members.
    var visible_to: [String]?
}

struct BusDebriefDTO: Codable {
    var button_label: String
    var summary_prompt: String
    var debrief_member_id: String
}

struct BusReviewLoopDTO: Codable {
    var reviewer_id: String
    var approve_marker: String
    var max_rounds: Int
}

/// The rich scenario editor unit — mirrors `BusScenario`. The engine compiles
/// it to `BusGroupScenario` at launch (dropping `icon`/`name`/`role`/
/// `topic_fields`/`debrief`); the store persists this directly.
struct BusScenarioDTO: Codable {
    var id: String
    var name: String
    var icon: String?
    var members: [BusScenarioMemberDTO]
    /// `scripted` | `moderator` | `roundrobin`.
    var turn_policy: String
    var script_order: [String]?
    var moderator_id: String?
    var opener_agent_id: String?
    var opener_line: String?
    var topic_fields: [BusTopicFieldDTO]?
    var debrief: BusDebriefDTO?
    var review_loop: BusReviewLoopDTO?
    /// `en` | `zh` (mirrors `BusLocale` serde + `AppLocale.rawValue`).
    var locale: String?
}

/// One validation problem returned by `scenario/validate` / `scenario/upsert`.
/// `code` is the stable machine key frontends localize off; `message` is the
/// English fallback.
struct ScenarioErrorDTO: Codable {
    var field: String
    var code: String
    var message: String
}

// MARK: - Scenario ↔ DTO

extension Agent {
    /// Encode for the wire. `kind`/`model` `nil` (inherit) → `""` so the
    /// store holds a concrete (default) value; the launch path re-resolves
    /// inherit from `specView`, independent of the stored value.
    var busDTO: BusScenarioMemberDTO {
        BusScenarioMemberDTO(
            id: id,
            name: name,
            role: role.isEmpty ? nil : role,
            system_prompt: systemPrompt,
            kind: kind ?? "",
            model: model ?? "",
            api_key: apiKey,
            base_url: baseUrl,
            color: color,
            avatar: avatar
        )
    }

    /// Decode from the wire. `kind`/`model` `""` → `nil` (inherit) so the
    /// editor's "空=继承" semantics survive a server round-trip.
    init?(dto: BusScenarioMemberDTO) {
        self.init(
            id: dto.id,
            name: dto.name,
            role: dto.role ?? "",
            systemPrompt: dto.system_prompt,
            model: dto.model.isEmpty ? nil : dto.model,
            color: dto.color ?? "#4D6BFE",
            avatar: dto.avatar ?? "person.crop.circle",
            kind: dto.kind.isEmpty ? nil : dto.kind,
            apiKey: dto.api_key,
            baseUrl: dto.base_url
        )
    }
}

extension TopicField {
    var busDTO: BusTopicFieldDTO {
        BusTopicFieldDTO(id: id, label: label, placeholder: placeholder, visible_to: visibleTo)
    }
    init(dto: BusTopicFieldDTO) {
        self.init(id: dto.id, label: dto.label, placeholder: dto.placeholder, visibleTo: dto.visible_to)
    }
}

extension DebriefConfig {
    var busDTO: BusDebriefDTO {
        BusDebriefDTO(button_label: buttonLabel, summary_prompt: summaryPrompt, debrief_member_id: debriefMemberId)
    }
    init(dto: BusDebriefDTO) {
        self.init(buttonLabel: dto.button_label, summaryPrompt: dto.summary_prompt, debriefMemberId: dto.debrief_member_id)
    }
}

extension ReviewLoopConfig {
    var busDTO: BusReviewLoopDTO {
        BusReviewLoopDTO(reviewer_id: reviewerId, approve_marker: approveMarker, max_rounds: maxRounds)
    }
    init(dto: BusReviewLoopDTO) {
        self.init(reviewerId: dto.reviewer_id, approveMarker: dto.approve_marker, maxRounds: dto.max_rounds)
    }
}

extension TurnPolicy {
    /// Map the engine `turn_policy` string back to the enum. Unknown ⇒
    /// `.scripted` (the engine default) so a future policy string a newer
    /// engine emits degrades to a launchable scenario rather than failing
    /// the whole list decode.
    static func fromSpecValue(_ s: String) -> TurnPolicy {
        switch s {
        case "roundrobin": return .roundRobin
        case "moderator": return .moderator
        default: return .scripted
        }
    }
}

extension Scenario {
    /// Compile to the wire DTO. `locale` is the app's effective locale so the
    /// engine picks the matching group-chat prompt locale on launch.
    func toBusDTO() -> BusScenarioDTO {
        BusScenarioDTO(
            id: id,
            name: name,
            icon: icon,
            members: agents.map { $0.busDTO },
            turn_policy: turnPolicy.specValue,
            script_order: scriptOrder,
            moderator_id: moderatorId,
            opener_agent_id: openerAgentId,
            opener_line: openerLine,
            topic_fields: topicFields?.map { $0.busDTO },
            debrief: debrief?.busDTO,
            review_loop: reviewLoop?.busDTO,
            locale: AppLocale.current.rawValue
        )
    }
}

extension BusScenarioDTO {
    /// Decode the wire DTO back to the Swift editor model. Members that fail
    /// to decode are dropped (defensive — a malformed member shouldn't hide
    /// the rest of the scenario).
    func toScenario() -> Scenario {
        Scenario(
            id: id,
            name: name,
            icon: icon ?? "person.2",
            agents: members.compactMap { Agent(dto: $0) },
            turnPolicy: TurnPolicy.fromSpecValue(turn_policy),
            scriptOrder: script_order,
            moderatorId: moderator_id,
            openerAgentId: opener_agent_id,
            openerLine: opener_line,
            topicFields: topic_fields?.map { TopicField(dto: $0) },
            debrief: debrief.map { DebriefConfig(dto: $0) },
            reviewLoop: review_loop.map { ReviewLoopConfig(dto: $0) }
        )
    }
}

// MARK: - RPC result decoding

extension Scenario {
    /// Decode a `scenario/list` result `{scenarios: [BusScenario]}` into
    /// Swift scenarios. Each element is a serde JSON object (snake_case); we
    /// re-serialize the dict → decode via the DTO (mirrors the FFI
    /// `sessionInfoView(from:)` dict-decode idiom but via Codable for the
    /// richer nested shape).
    static func fromListResult(_ res: [String: Any]) -> [Scenario] {
        let arr = (res["scenarios"] as? [[String: Any]]) ?? []
        return arr.compactMap { dict -> Scenario? in
            guard JSONSerialization.isValidJSONObject(dict),
                  let data = try? JSONSerialization.data(withJSONObject: dict),
                  let dto = try? JSONDecoder().decode(BusScenarioDTO.self, from: data)
            else { return nil }
            return dto.toScenario()
        }
    }
}

extension Encodable {
    /// Encode to a JSON object suitable as a JSON-RPC `params` value (the
    /// `call(method, params:)` client takes `[String: Any]`).
    var asJSONObject: [String: Any]? {
        guard let data = try? JSONEncoder().encode(self) else { return nil }
        return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    }
}

// MARK: - Validate-error localization

/// Map an engine `ScenarioError` (field+code) to a localized user message, so
/// the sidecar validate path stays in the UI language instead of surfacing the
/// English `message` fallback. Codes are a bounded set (see
/// `BusScenario::validate`); unknown codes fall back to the engine message.
enum ScenarioErrorLocalizer {
    static func message(for e: ScenarioErrorDTO) -> String {
        switch e.code {
        case "empty":
            if e.field == "name" { return NSLocalizedString("请填写场景名。", comment: "") }
            if e.field == "members" { return NSLocalizedString("至少需要一个智能体(演员表不能为空)。", comment: "") }
            if e.field.hasSuffix(".name") { return NSLocalizedString("存在缺少名字的智能体。", comment: "") }
            if e.field.hasSuffix(".system_prompt") { return NSLocalizedString("存在缺少系统提示词的智能体。", comment: "") }
        case "unknown_id":
            if e.field == "script_order" { return NSLocalizedString("轮次顺序引用了不存在的角色。", comment: "") }
            if e.field == "moderator_id" { return NSLocalizedString("主持人不在演员表中。", comment: "") }
            if e.field == "opener_agent_id" { return NSLocalizedString("开场角色不在演员表中。", comment: "") }
            if e.field.hasPrefix("review_loop") { return NSLocalizedString("评审角色不在演员表中。", comment: "") }
        case "missing":
            if e.field == "moderator_id" { return NSLocalizedString("主持人策略需要选择一个主持人。", comment: "") }
        case "invalid":
            if e.field == "review_loop.max_rounds" { return NSLocalizedString("评审轮次至少为 1。", comment: "") }
        default:
            break
        }
        return e.message
    }
}
