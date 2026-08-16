// OneAI app-server JSON-RPC 2.0 wire types.
//
// Grounded in the Rust sources (the single source of truth):
//  - crates/oneai-app-server/src/protocol.rs   — envelope (Request/Response/Notification)
//  - crates/oneai-app-server/src/adapter.rs    — method→params→result mapping
//  - crates/oneai-bus/src/protocol.rs          — EngineYield (serde `tag="kind"`, snake_case)
//  - crates/oneai-core/src/types.rs            — ContentBlock (serde `tag="type"`)
//
// The app-server is framing-agnostic; over ws one WebSocket text frame = one
// JSON-RPC message (either direction). Outbound the app-server emits exactly
// one method — `event` — whose params is the whole EngineYield (with its
// `kind` tag). New yield variants arrive as unknown `kind`s an old frontend
// ignores, so the protocol grows with the bus without breaking old frontends.

// ─── Envelope ────────────────────────────────────────────────────────────────

export interface JsonRpcRequest<P = unknown> {
  jsonrpc: '2.0'
  id: RpcId
  method: string
  params: P
}
export interface JsonRpcNotification<P = unknown> {
  jsonrpc: '2.0'
  method: string
  params: P
}
export interface JsonRpcResponse {
  jsonrpc: '2.0'
  id: RpcId | null
  result?: unknown
  error?: JsonRpcError
}
export interface JsonRpcError {
  code: number
  message: string
  data?: unknown
}

export type RpcId = number | string

// ─── ContentBlock (turn/run `content` param) ────────────────────────────────
// Mirrors oneai-core::ContentBlock — `#[serde(tag = "type")]`. Only the shapes
// the webUI emits/renders are typed here; the enum is #[non_exhaustive].

export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'image'; mime_type: string; data: string } // base64
  | { type: 'file'; mime_type: string; uri: string }
  | { type: 'thinking'; text: string }

// ─── EngineYield (the `event` notification params) ───────────────────────────
// Mirrors oneai-bus::EngineYield — `#[serde(tag = "kind", rename_all = "snake_case")]`,
// `#[non_exhaustive]`. Only the variants the webUI currently consumes are
// enumerated; a frontend MUST ignore unknown `kind`s (matches the Rust
// contract — extra variants are transparent).

export type EngineYieldKind =
  | 'turn_start'
  | 'iteration_start'
  | 'stream_chunk'
  | 'thinking'
  | 'direct_answer'
  | 'tool_calls'
  | 'tool_result'
  | 'delegate'
  | 'delegate_complete'
  | 'speaker_turn'
  | 'paradigm_switch'
  | 'approval_request'
  | 'working_state'
  | 'context_accounting'
  | 'plan_update'
  | 'tools_added'
  | 'init_result'
  | 'compact_result'
  | 'token_usage'
  | 'error'
  | 'turn_complete'
  | 'session_created'
  | 'session_loaded'
  | 'session_cleared'
  | 'session_deleted'
  | 'session_ended'

export interface EngineYieldBase {
  kind: EngineYieldKind
  /** discriminator narrowing is done by `kind`; keep this for completeness. */
  [k: string]: unknown
}

export interface YieldTurnStart {
  kind: 'turn_start'
  turn_id: string
  task: string
}
export interface YieldStreamChunk {
  kind: 'stream_chunk'
  turn_id: string
  text: string
  speaker: string | null
}
export interface YieldThinking {
  kind: 'thinking'
  turn_id: string
  text: string
  speaker: string | null
}
export interface YieldDirectAnswer {
  kind: 'direct_answer'
  turn_id: string
  text: string
  speaker: string | null
}
export interface YieldToolCalls {
  kind: 'tool_calls'
  turn_id: string
  calls: BusToolCall[]
  speaker: string | null
}
export interface YieldToolResult {
  kind: 'tool_result'
  turn_id: string
  call_id: string
  tool_name: string
  output: ToolOutput
  speaker: string | null
}
export interface YieldSpeakerTurn {
  kind: 'speaker_turn'
  turn_id: string
  speaker: string
}
export interface YieldError {
  kind: 'error'
  recoverable: boolean
  message: string
}
export interface YieldTurnComplete {
  kind: 'turn_complete'
  turn_id: string
  summary: unknown
}
export interface YieldApprovalRequest {
  kind: 'approval_request'
  request_id: string
  request: InteractionRequest
}
export interface YieldParadigmSwitch {
  kind: 'paradigm_switch'
  turn_id: string
  from: BusParadigmKind
  to: BusParadigmKind
}
export interface YieldPlanUpdate {
  kind: 'plan_update'
  turn_id: string
  plan: PlanState | null
}
export interface YieldIterationStart {
  kind: 'iteration_start'
  turn_id: string
  iteration: number
  paradigm: BusParadigmKind
}
export interface YieldSessionCreated {
  kind: 'session_created'
  id: string
}
export interface YieldSessionLoaded {
  kind: 'session_loaded'
  id: string
  messages: unknown[]
}
export interface YieldSessionCleared {
  kind: 'session_cleared'
  id: string
}
export interface YieldSessionDeleted {
  kind: 'session_deleted'
  id: string
}

// EngineYield is the union of the *concrete* variants the webUI currently
// narrows on. It is intentionally NOT a catch-all: an overlapping
// `{kind: EngineYieldKind, ...}` member would defeat `switch (y.kind)`
// discriminated-union narrowing (TS can't exclude it for a given literal).
// Unknown `kind`s from the server arrive via the `as EngineYield` cast in
// client.ts and fall through to the projection's `default` branch — the bus
// contract is "new variants are transparent to old frontends".
export type EngineYield =
  | YieldTurnStart
  | YieldStreamChunk
  | YieldThinking
  | YieldDirectAnswer
  | YieldToolCalls
  | YieldToolResult
  | YieldSpeakerTurn
  | YieldError
  | YieldTurnComplete
  | YieldApprovalRequest
  | YieldParadigmSwitch
  | YieldPlanUpdate
  | YieldIterationStart
  | YieldSessionCreated
  | YieldSessionLoaded
  | YieldSessionCleared
  | YieldSessionDeleted

export interface BusToolCall {
  id: string
  name: string
  args: unknown
}

// ─── Tool result (oneai-core::ToolOutput) ───────────────────────────────────
export interface ToolOutput {
  success: boolean
  content: string
  error?: string
  added_tool_names?: string[]
}

// ─── Paradigm (oneai-bus::BusParadigmKind, snake_case) ──────────────────────
export type BusParadigmKind = 'plan' | 're_act' | 'reflect' | 'explore'

// ─── Plan state (oneai-agent::PlanState, carried as plan_update.plan) ────────
export type PlanStepStatus = 'pending' | 'in_progress' | 'completed' | 'failed'

export interface PlanStep {
  id: string
  description: string
  coupled: boolean
  depends_on?: string[]
  status?: PlanStepStatus
  active_form?: string | null
}

export interface PlanState {
  steps: PlanStep[]
  revision?: number
  [k: string]: unknown
}

// ─── Interaction gate (oneai-core::InteractionRequest/Response) ────────────
// Externally-tagged enums, NO rename — wire form is `{"ToolApproval": {...}}` /
// `{"Proceed": null}`. Narrow by the single object key.
//
// PreInfer/PostInfer are engine-internal (NoopGate default-proceed) and never
// reach a UI; omitted from the union.

export type RiskLevel = 'low' | 'medium' | 'high'
export type PermissionLevel = 'read' | 'standard' | 'full'

export interface ApprovalRequest {
  tool_name: string
  args: unknown
  risk_level: RiskLevel
  permission_level?: PermissionLevel
  justification: string
}

export interface DecisionOption {
  id: string
  label: string
  description: string
  tradeoffs: string
}

export interface PlanReviewRequest {
  plan: string
  steps: PlanStep[]
}

export interface PlanDecisionRequest {
  decision_id: string
  question: string
  context: string
  options: DecisionOption[]
}

export interface NetworkApprovalRequest {
  host: string
  requested_by: string
}

export interface McpElicitationRequest {
  server: string
  message: string
  requested_schema: unknown
}

export type InteractionRequest =
  | { ToolApproval: { approval: ApprovalRequest } }
  | { PlanReview: PlanReviewRequest }
  | { PlanDecision: PlanDecisionRequest }
  | { NetworkApproval: NetworkApprovalRequest }
  | { McpElicitation: McpElicitationRequest }

export type ElicitationAction = 'accept' | 'decline' | 'cancel'

export interface InteractionResponseProceed {
  Proceed: null
}
export interface InteractionResponseProceedWith {
  ProceedWith: { modification: unknown }
}
export interface InteractionResponseRevise {
  Revise: { feedback: string }
}
export interface InteractionResponseChoose {
  Choose: { option_id: string }
}
export interface InteractionResponseAbort {
  Abort: { reason: string }
}
export interface InteractionResponseElicitation {
  ElicitationReply: { action: ElicitationAction; data?: unknown }
}

export type InteractionResponse =
  | InteractionResponseProceed
  | InteractionResponseProceedWith
  | InteractionResponseRevise
  | InteractionResponseChoose
  | InteractionResponseAbort
  | InteractionResponseElicitation

// ─── Method params / results (the subset W1 uses) ───────────────────────────

export interface TurnRunParams {
  content: ContentBlock[]
}
export interface TurnRunResult {
  turn_id: string
  task: string
}

export interface SessionListResult {
  sessions: SessionInfo[]
}
export interface SessionInfo {
  id: string
  created_at_ms: number
  updated_at_ms: number
  message_count: number
  title: string
}

export interface OkResult {
  ok: boolean
}

// ─── Method params / results (W2: approval / paradigm / config / cancel) ─────

export interface ApprovalRespondParams {
  request_id: string
  response: InteractionResponse
}

export interface ParadigmSwitchParams {
  to: BusParadigmKind
}

export interface ConfigUpdateParams {
  plan_mode?: boolean
}

export interface TurnCancelParams {
  reason?: unknown
}

export interface ConversationCompactParams {
  keep_recent_turns: number
}

// ─── Scenario library (scenario/*) + group chat (group/*) ────────────────────
// Mirrors oneai-bus::protocol — `BusScenario` (rich editor unit) /
// `BusGroupScenario` (compiled engine launch payload) and friends. Field-for-
// field, snake_case (serde rename_all = "snake_case" on the locale enum only;
// the structs use default serde = field name). The engine consumes the
// *compiled* `BusGroupScenario` over `group/start`; it never sees the editor-
// only fields (`icon`/`name`/`role`/`topic_fields`/`debrief`).

export type BusLocale = 'en' | 'zh'

export interface BusReviewLoop {
  reviewer_id: string
  approve_marker: string
  /** Default 1 when absent on the wire (server default_one). */
  max_rounds?: number
}

export interface BusDebriefConfig {
  button_label: string
  summary_prompt: string
  debrief_member_id: string
}

export interface BusTopicField {
  id: string
  label: string
  placeholder?: string
  /** Member ids allowed to see this field's value in their background. Absent
   *  ⇒ all members; present ⇒ only those. */
  visible_to?: string[]
}

export interface BusScenarioMember {
  id: string
  name: string
  /** Short UI-only label the engine drops on compile. */
  role?: string
  system_prompt: string
  /** Provider kind; `""`/absent ⇒ inherit the app's configured provider. */
  kind?: string
  /** Model name; `""`/absent ⇒ inherit. */
  model?: string
  api_key?: string
  base_url?: string
  color?: string
  avatar?: string
}

/** The rich scenario editor unit — what `scenario/*` stores return/edit. */
export interface BusScenario {
  id: string
  name: string
  icon?: string
  members: BusScenarioMember[]
  /** `scripted` | `moderator` | `roundrobin` (anything else ⇒ round-robin). */
  turn_policy: string
  script_order?: string[]
  moderator_id?: string
  opener_agent_id?: string
  opener_line?: string
  topic_fields?: BusTopicField[]
  debrief?: BusDebriefConfig
  review_loop?: BusReviewLoop
  locale?: BusLocale
}

/** The compiled engine launch payload — `group/start`'s `scenario` param.
 *  The frontend compiles a `BusScenario` (+ collected topic values) into this
 *  via `compileGroupScenario`, baking the topic background into member
 *  `system_prompt`s per `visible_to` and dropping UI-only fields. Mirrors
 *  `BusGroupScenario` (members = `BusAgentSpec`, no `role`). */
export interface BusGroupScenario {
  members: {
    id: string
    name: string
    system_prompt: string
    kind?: string
    model?: string
    api_key?: string
    base_url?: string
    color?: string
    avatar?: string
  }[]
  turn_policy: string
  script_order?: string[]
  moderator_id?: string
  opener_agent_id?: string
  opener_line?: string
  title?: string
  review_loop?: BusReviewLoop
  locale?: BusLocale
}

/** One scenario-validation problem, surfaced by `scenario/validate` /
 *  `scenario/upsert` (when `ok:false`). Frontends localize off `code`. */
export interface ScenarioError {
  /** Dot-path to the offending field, e.g. `members.0.name`, `script_order`. */
  field: string
  /** Stable machine code: `empty` | `unknown_id` | `missing` | `invalid`. */
  code: string
  /** English fallback message. */
  message: string
}

// ── scenario/* RPC results ──

export interface ScenarioListResult {
  scenarios: BusScenario[]
}
export interface ScenarioValidateResult {
  ok: boolean
  errors: ScenarioError[]
}
export interface ScenarioUpsertResult {
  ok: boolean
  /** Present when ok:true. */
  id?: string
  /** Present when ok:false. */
  errors?: ScenarioError[]
}

// ── group/* RPC params (all ack — results stream as `event` notifications) ──

export interface GroupStartParams {
  scenario: BusGroupScenario
}
export interface GroupRunParams {
  user_input: string
}
export interface GroupSetOrderParams {
  order: string[]
}
