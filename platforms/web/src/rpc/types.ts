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
  output: unknown
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
  request: unknown
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
  | YieldSessionCreated
  | YieldSessionLoaded
  | YieldSessionCleared
  | YieldSessionDeleted

export interface BusToolCall {
  call_id: string
  name: string
  args: unknown
}

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
