// Trajectory + capability projection types.
//
// The trajectory ledger is a turn-aware append-only event stream the projection
// accumulates from the EngineYield kinds the W1–W3 projection previously
// dropped into its `default` branch: `iteration_start`, `delegate` /
// `delegate_complete`, `working_state`, `context_accounting`, `token_usage`,
// `tools_added`, plus `turn_start` / `turn_complete` / `tool_calls` /
// `tool_result` / `plan_update` / `paradigm_switch` boundaries. It is a
// *pure consumer* of already-flowing events — no Rust change. The TrajectoryTab
// renders this ledger as a timeline + a timing overview.

import type {
  BusSubAgentKind,
  BusUsageRecord,
  ContextAccounting,
  EngineYieldKind,
  StepStatus,
  WorkingStep,
  Decision,
  Blocker,
  Note,
} from '../rpc/types'

/** One line in the trajectory ledger. `ms` is elapsed since `turn_start`
 *  (browser-side `performance.now()` delta — sufficient for an overview, not
 *  engine-authoritative timing). */
export interface TrajectoryEntry {
  /** Monotonic arrival order — stable for sort. */
  seq: number
  /** The turn this entry belongs to (null for session-scoped events). */
  turnId: string | null
  /** Wall-clock ms at arrival (epoch, for relative display). */
  at: number
  /** Elapsed ms since the entry's turn_start (null if no active turn). */
  ms: number | null
  /** Which EngineYield kind produced this line. */
  kind: EngineYieldKind | 'plan_revision' | 'round_boundary'
  /** One-line human title (tool name, paradigm, token count, …). */
  title: string
  /** Optional structured detail for the detail drill-in. */
  detail?: unknown
  /** Iteration number, when kind === iteration_start. */
  iter?: number
  /** Paradigm, when relevant. */
  paradigm?: string
}

/** A delegated sub-agent in flight or completed. */
export interface SubagentNode {
  /** Stable key — the delegate event's turn_id + a per-turn counter (delegate
   *  carries no call id, so we synthesize one on arrival). */
  id: string
  turnId: string
  task: string
  agentKind: BusSubAgentKind
  status: 'active' | 'done'
  summary?: BusSubAgentNode
}

/** The `BusSubAgent` summary attached on `delegate_complete`. */
export interface BusSubAgentNode {
  summary: string
  keyFindings: string[]
  budgetExceeded: boolean
  tokensUsed: number
  completed: boolean
}

/** A background (or foreground) delegated sub-agent tracked across turns.
 *  Unlike `SubagentNode` (turn-scoped, cleared at turn boundaries), this slice
 *  persists across turns so a background sub-agent's lifecycle — launched in
 *  turn A, completes in turn B — stays visible. Matched by `task_id`. */
export interface BackgroundTaskNode {
  taskId: string
  turnId: string
  task: string
  agentKind: BusSubAgentKind
  status: 'active' | 'done' | 'failed'
  iteration?: number
  paradigm?: string
  lastTool?: string
  lastToolSnapshot?: string
  usage?: { prompt: number; completion: number }
  summary?: BusSubAgentNode
}

/** A working-state step, normalized for the goal bar. */
export interface GoalStep {
  id: string
  description: string
  status: StepStatus
  activeForm?: string | null
  order: number
}

/** The working-state projection — the live goal/steps/decisions/blockers. */
export interface WorkingProjection {
  goal: string | null
  intent: string | null
  steps: WorkingStep[]
  decisions: Decision[]
  blockers: Blocker[]
  notes: Note[]
}

export const EMPTY_WORKING: WorkingProjection = {
  goal: null,
  intent: null,
  steps: [],
  decisions: [],
  blockers: [],
  notes: [],
}

/** Per-turn timing for the overview bar. */
export interface TurnTiming {
  turnId: string
  /** performance.now() at turn_start. */
  startedAt: number | null
  /** performance.now() at turn_complete (null while in flight). */
  endedAt: number | null
  iterationCount: number
}

/** Aggregated token usage for the overview bar (latest snapshot). */
export interface UsageSnapshot {
  usage: BusUsageRecord | null
  context: ContextAccounting | null
}

let trajSeq = 0
export function nextTrajectorySeq(): number {
  trajSeq += 1
  return trajSeq
}

let subagentSeq = 0
export function nextSubagentId(turnId: string): string {
  subagentSeq += 1
  return `sub:${turnId}:${subagentSeq}`
}

/** Helper for the goal bar: summarize step progress as done/total. */
export function stepProgress(steps: WorkingStep[]): { done: number; total: number; active: boolean } {
  let done = 0
  let active = false
  for (const s of steps) {
    if (s.status === 'Completed') done += 1
    if (s.status === 'InProgress') active = true
  }
  return { done, total: steps.length, active }
}
