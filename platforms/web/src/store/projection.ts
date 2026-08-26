import { useSyncExternalStore } from 'react'
import type { OneAiRpcClient } from '../rpc/client'
import type {
  ApprovalRespondParams,
  Artifact,
  BackgroundTaskOpResult,
  BusParadigmKind,
  BusScenario,
  BusScenarioMember,
  BusLocale,
  BusUsageRecord,
  ConfigUpdateParams,
  ContentBlock,
  ContextSection,
  EngineYield,
  FeedbackEntry,
  FeedbackKind,
  GroupRunParams,
  GroupSetOrderParams,
  GroupStartParams,
  HostListResult,
  InteractionRequest,
  InteractionResponse,
  ParadigmSwitchParams,
  PlanState,
  PlanStep,
  SessionInfo,
  ToolOutput,
  TurnRunParams,
} from '../rpc/types'
import { contextKeyString } from '../rpc/types'
import { StreamCoalescer } from '../stream/coalescer'
import { compileGroupScenario, firstUserMessage } from '../scenario/compile'
import type {
  BackgroundTaskNode,
  ResolvedContextSection,
  SubagentNode,
  TrajectoryDetail,
  TrajectoryEntry,
  UsageSnapshot,
  WorkingProjection,
} from './trajectory'
import { EMPTY_WORKING, nextSubagentId, nextTrajectorySeq } from './trajectory'

// The projection store is the external store backing useSyncExternalStore.
//
// Mutable working state (the `state` field) is updated the instant an
// EngineYield arrives, so a synchronous read always sees the latest text. But
// the *snapshot* React subscribes to is only re-built (new reference) when the
// StreamCoalescer flushes — ≤20fps for hot fragments, immediate for terminal
// signals. Returning a cached snapshot reference on every getSnapshot call
// until the next flush is what keeps React from re-rendering per token.
//
// (Standard external-store-with-batching pattern; mirrors the macOS
// ChatViewModel StreamCoalescer semantics.)

export type NodeRole = 'user' | 'assistant' | 'system'
export type NodeKind =
  | 'user'
  | 'text'
  | 'thinking'
  | 'error'
  | 'tool'
  | 'plan'
  /** Client-local system note (slash-command feedback — /help text,
   *  `/session list` output, unknown-command notice). Never sent to the
   *  engine; gone on reload, like the TUI's `ChatRole::System` lines. */
  | 'note'
export type NodeState = 'streaming' | 'done' | 'error'

export type ToolState = 'pending' | 'done' | 'error'

export interface ChatNode {
  /** Stable per-node key — React list reconciliation uses this. */
  id: string
  role: NodeRole
  kind: NodeKind
  text: string
  speaker: string | null
  turnId: string | null
  state: NodeState
  // ── tool node (kind === 'tool') ──
  callId?: string
  toolName?: string
  toolArgs?: unknown
  toolOutput?: ToolOutput
  toolState?: ToolState
  // ── plan node (kind === 'plan') ──
  planSteps?: PlanStep[]
  planRevision?: number
  // ── W4: attachments / deliverables / feedback ──
  /** User-authored image/file blocks attached to a user node (drag-drop). */
  attachments?: ContentBlock[]
  /** Turn-end file artifacts collected from this turn's tool results — surfaced
   * on the final assistant text node of the turn (the "deliverable strip"). */
  deliverables?: Artifact[]
  /** Per-message 👍/👎/note recorded against this node (assistant text nodes). */
  feedback?: FeedbackEntry
  /** Optimistic flag while feedback/submit is in flight. */
  feedbackPending?: boolean
}

export interface ApprovalItem {
  request_id: string
  request: InteractionRequest
  /** Monotonic arrival order — stable for the queue head/promote-next logic. */
  seq: number
}

export interface ProjectionSnapshot {
  /** Bumped on every flush so callers can detect "anything changed". */
  version: number
  sessionId: string | null
  nodes: ChatNode[]
  /** A turn is in flight (turn_start seen, turn_complete not yet). */
  turnActive: boolean
  /** Last fatal engine error message, if any. */
  lastError: string | null
  /** Active paradigm (react by default; plan/reflect/explore on switch). */
  paradigm: BusParadigmKind
  /** Head of the approval queue — the request the UI is currently showing. */
  currentApproval: ApprovalItem | null
  /** Number of queued approvals behind the head (for the "N more" badge). */
  approvalQueueDepth: number
  /** Id of the tool node whose args/result the details rail shows. */
  selectedToolNodeId: string | null
  /** Active multi-agent scenario, or null in single-agent mode. Drives
   *  speaker-tagged bubbles + the debrief button. */
  currentScenario: BusScenario | null
  /** Members of the active scenario (for speaker name/color resolution). */
  scenarioMembers: BusScenarioMember[] | null
  /** True once the scenario's debrief phase has been triggered. */
  debriefActive: boolean
  /** True when a debrief is configured and not yet triggered. */
  debriefAvailable: boolean
  // ── W4 trajectory + capability panel (pure consumers of flowing events) ──
  /** Turn-aware event ledger — the trajectory timeline. Session-scoped
   *  (cleared on new session / scenario start). */
  trajectory: TrajectoryEntry[]
  /** Live working-state projection (goal/steps/decisions/blockers/notes). */
  working: WorkingProjection
  /** Sub-agents the model delegated to (active + completed). */
  subagents: SubagentNode[]
  /** Background delegated sub-agents tracked ACROSS turns (a background task
   *  launched in turn A completes in turn B). Matched by `task_id`; NOT
   *  cleared at turn boundaries — only on session/scenario reset. */
  backgroundTasks: BackgroundTaskNode[]
  /** Latest usage snapshot (token usage + context accounting). */
  usage: UsageSnapshot
  /** Performance.now() marks per turn, for timing-overview rendering. */
  turnTimings: { turnId: string; startedAt: number | null; endedAt: number | null; iterations: number }[]
  /** Aggregated session metrics for the composer metrics strip
   *  (turns/steps/first-token/tok-per-s/cache hit/in-out tokens). */
  metrics: SessionMetrics
}

/** Session-wide aggregate metrics, derived from per-turn timing + the latest
 *  per-turn usage map. Token counts (prompt/completion) are CUMULATIVE across
 *  the session (total spend); cache hit % is the LATEST inference step's rate,
 *  not a session average — cache behavior is per-step (cold first call, warm
 *  thereafter), so a cumulative average dilutes the signal. All fields default
 *  to 0/null so a fresh session renders an empty (hidden) strip rather than NaN. */
export interface SessionMetrics {
  /** Number of turns seen this session. */
  turns: number
  /** Sum of per-turn iteration counts (agent "steps"). */
  steps: number
  /** Mean ms from turn_start to the first stream_chunk, across turns that
   *  produced text. null when no turn has streamed yet. */
  firstTokenMs: number | null
  /** Sum of completion tokens across turns (for tok/s). */
  totalCompletion: number
  /** Sum of prompt tokens across turns (input total). */
  totalPrompt: number
  /** Cache hit % of the most recent inference step (the latest token_usage
   *  record) — `cache_read / prompt_tokens`. `prompt_tokens` is the total
   *  input footprint (already includes the cache subsets, per the provider
   *  layer's normalization), so the denominator is `prompt_tokens` alone.
   *  null until the first usage record arrives. */
  cacheHitPct: number | null
  /** Sum of completed-turn wall durations (ms). */
  totalDurationMs: number
  /** The most recent inference step's total input footprint (the latest
   *  `token_usage` record's `prompt_tokens`) — i.e. the current context size
   *  after the last turn. Surfaced in the metrics strip so the user can tell
   *  whether to compact / start a fresh session (issue #35). null until the
   *  first usage record arrives. */
  contextTokens: number | null
}

interface WorkingState {
  sessionId: string | null
  nodes: ChatNode[]
  turnActive: boolean
  lastError: string | null
  currentTurnId: string | null
  currentSpeaker: string | null
  paradigm: BusParadigmKind
  approvalQueue: ApprovalItem[]
  selectedToolNodeId: string | null
  currentScenario: BusScenario | null
  debriefActive: boolean
  // ── W4 trajectory + capability (session-scoped ledgers) ──
  trajectory: TrajectoryEntry[]
  working: WorkingProjection
  subagents: SubagentNode[]
  backgroundTasks: BackgroundTaskNode[]
  usage: UsageSnapshot
  /** per-turn performance.now() start marks (for elapsed-ms timing). */
  turnStartPerf: Map<string, number>
  turnEndPerf: Map<string, number>
  turnIterations: Map<string, number>
  /** performance.now() of the first stream_chunk per turn (for first-token
   *  latency). Absent until the turn produces text. */
  firstTokenPerf: Map<string, number>
  /** Latest per-turn usage record (TokenUsage carries no turn_id, so we
   *  attribute it to the active currentTurnId on arrival). */
  turnUsage: Map<string, BusUsageRecord>
  /** The most recently received usage record overall — drives the
   *  latest-step cache-hit % metric (a rate, not a cumulative sum). Updated on
   *  every token_usage event; cleared on session reset. */
  latestUsage: BusUsageRecord | null
  /** Auto-accept mode: silently Proceed on incoming ToolApproval requests
   *  (mirrors the TUI's InteractionMode::AutoAccept) instead of queueing them
   *  for the approval bar. Other variants (plan/network/elicitation) still
   *  surface — only tool execution is auto-allowed. */
  autoApprove: boolean
  /** Metrics baseline restored from `metricsCache` on session switch-in. The
   *  per-turn timing/usage maps (`turnStartPerf` etc.) only hold turns seen
   *  *live since the last switch* — historical turns replayed on
   *  `session_loaded` carry no usage, so without a baseline the metrics strip
   *  would reset to empty every time you switch away and back (issue #35).
   *  `emit()` merges this baseline with the live (post-switch) metrics:
   *  cumulative fields sum, per-step fields (latency/cache/context) prefer the
   *  live latest when one exists, else fall back to the baseline. */
  metricsBaseline: SessionMetrics
}

const EMPTY_METRICS: SessionMetrics = {
  turns: 0,
  steps: 0,
  firstTokenMs: null,
  totalCompletion: 0,
  totalPrompt: 0,
  cacheHitPct: null,
  totalDurationMs: 0,
  contextTokens: null,
}

const EMPTY: ProjectionSnapshot = {
  version: 0,
  sessionId: null,
  nodes: [],
  turnActive: false,
  lastError: null,
  paradigm: 're_act',
  currentApproval: null,
  approvalQueueDepth: 0,
  selectedToolNodeId: null,
  currentScenario: null,
  scenarioMembers: null,
  debriefActive: false,
  debriefAvailable: false,
  trajectory: [],
  working: EMPTY_WORKING,
  subagents: [],
  backgroundTasks: [],
  usage: { usage: null, context: null },
  turnTimings: [],
  metrics: EMPTY_METRICS,
}

let nodeSeq = 0
function nextNodeId(): string {
  nodeSeq += 1
  return `n${nodeSeq}`
}

/** Insert-or-update a background task by `taskId` (cross-turn lifecycle). */
function upsertBackgroundTask(
  list: BackgroundTaskNode[],
  task: BackgroundTaskNode,
): BackgroundTaskNode[] {
  const i = list.findIndex((t) => t.taskId === task.taskId)
  if (i < 0) return [...list, task]
  const copy = [...list]
  copy[i] = { ...list[i], ...task }
  return copy
}

let approvalSeq = 0

export class ProjectionStore {
  private rpc: OneAiRpcClient
  private state: WorkingState = {
    sessionId: null,
    nodes: [],
    turnActive: false,
    lastError: null,
    currentTurnId: null,
    currentSpeaker: null,
    paradigm: 're_act',
    approvalQueue: [],
    selectedToolNodeId: null,
    currentScenario: null,
    debriefActive: false,
    trajectory: [],
    working: { ...EMPTY_WORKING },
    subagents: [],
    backgroundTasks: [],
    usage: { usage: null, context: null },
    turnStartPerf: new Map(),
    turnEndPerf: new Map(),
    turnIterations: new Map(),
    firstTokenPerf: new Map(),
    turnUsage: new Map(),
    latestUsage: null,
    autoApprove: false,
    metricsBaseline: { ...EMPTY_METRICS },
  }
  private snapshot: ProjectionSnapshot = EMPTY
  private listeners = new Set<() => void>()
  private coalescer: StreamCoalescer
  /** Per-session cached metrics, keyed by sessionId. Stashed on switch-out
   *  (capturing baseline+live merged) and restored on switch-in so the metrics
   *  strip survives `session_loaded` round-trips (issue #35). Lives on the
   *  store, not in `state`, since it's a registry across sessions, not
   *  per-snapshot. */
  private metricsCache = new Map<string, SessionMetrics>()

  // ── Issue #40 trajectory buffering / replay ────────────────────────────────
  /** The current iteration's accumulated reasoning text, flushed into that
   *  iteration's trajectory entry at the next boundary event. */
  private iterBuffer: {
    turnId: string
    entrySeq: number
    inference: string
    thinking: string
  } | null = null
  /** Per-key resolved context content (context assembly hash-dedup). Keyed by
   *  `contextKeyString(key)`; filled when a section carries `content`, read
   *  back when it arrives deduped (`content` absent). */
  private ctxCache = new Map<string, string>()
  /** The seq of the current iteration's `iteration_start` entry. Unlike
   *  `iterBuffer` (nulled at each tool/direct-answer boundary), this persists
   *  until the next `iteration_start`, so the trailing `inference` /
   *  `token_usage` events — which arrive AFTER the mid-stream `tool_calls`
   *  finalizes the text buffer — can still patch the infer node (issue #40). */
  private currentIterSeq: number | null = null
  /** The iteration number of the current iteration (drives `pos`). */
  private currentIteration = 0

  constructor(rpc: OneAiRpcClient) {
    this.rpc = rpc
    this.coalescer = new StreamCoalescer(() => this.emit())
  }

  /**
   * Subscribe the store to engine events. Returns an unsubscribe.
   *
   * This MUST be driven from a useEffect (subscribe on mount, unsubscribe in
   * cleanup) rather than the constructor: under React StrictMode the effect
   * cleanup runs on a simulated unmount, and a subscription made in the
   * constructor is never re-established on remount (the useMemo singleton's
   * constructor doesn't re-run) — so engine events stop reaching the store.
   * Driving it from the effect makes subscribe/unsubscribe symmetric across
   * StrictMode's mount/unmount/remount cycle.
   */
  attach(): () => void {
    return this.rpc.onEvent((y) => this.consume(y))
  }

  dispose(): void {
    this.listeners.clear()
  }

  // ── external store contract ────────────────────────────────────────────────

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn)
    return () => this.listeners.delete(fn)
  }

  getSnapshot = (): ProjectionSnapshot => this.snapshot

  /** Flush the working state into a fresh snapshot + notify React. */
  private emit(): void {
    const q = this.state.approvalQueue
    const sc = this.state.currentScenario
    const debriefAvailable =
      sc !== null && sc.debrief !== undefined && !this.state.debriefActive
    const turnTimings = this.currentTurnTimings()
    const metrics = this.mergeMetrics(
      this.state.metricsBaseline,
      this.computeMetrics(turnTimings),
    )
    this.snapshot = {
      version: this.snapshot.version + 1,
      sessionId: this.state.sessionId,
      // Shallow-copy the nodes array so React sees a new reference; the node
      // objects themselves are replaced (not mutated) on each edit, so a
      // shallow copy is enough for memoized row components to diff.
      nodes: [...this.state.nodes],
      turnActive: this.state.turnActive,
      lastError: this.state.lastError,
      paradigm: this.state.paradigm,
      currentApproval: q.length > 0 ? q[0] : null,
      approvalQueueDepth: q.length > 0 ? q.length - 1 : 0,
      selectedToolNodeId: this.state.selectedToolNodeId,
      currentScenario: sc,
      scenarioMembers: sc !== null ? sc.members : null,
      debriefActive: this.state.debriefActive,
      debriefAvailable,
      trajectory: [...this.state.trajectory],
      working: this.state.working,
      subagents: [...this.state.subagents],
      backgroundTasks: [...this.state.backgroundTasks],
      usage: this.state.usage,
      turnTimings,
      metrics,
    }
    for (const l of this.listeners) l()
  }

  /** Fold per-turn timing + usage maps into the session metrics aggregate.
   *  Token totals are cumulative; cache hit % is the latest step's rate. */
  private computeMetrics(
    turnTimings: { turnId: string; startedAt: number | null; endedAt: number | null; iterations: number }[],
  ): SessionMetrics {
    let steps = 0
    let totalCompletion = 0
    let totalPrompt = 0
    let totalDurationMs = 0
    const latencies: number[] = []
    for (const t of turnTimings) {
      steps += t.iterations
      const u = this.state.turnUsage.get(t.turnId)
      if (u !== undefined) {
        totalPrompt += u.prompt_tokens
        totalCompletion += u.completion_tokens
      }
      if (t.startedAt !== null && t.endedAt !== null) {
        totalDurationMs += t.endedAt - t.startedAt
      }
      const ft = this.state.firstTokenPerf.get(t.turnId)
      if (t.startedAt !== null && ft !== undefined) {
        latencies.push(ft - t.startedAt)
      }
    }
    const firstTokenMs =
      latencies.length > 0 ? latencies.reduce((a, b) => a + b, 0) / latencies.length : null
    // Cache hit % from the LATEST inference step (most recent usage record),
    // not the session cumulative — a cold-first/warm-later average hides
    // whether caching is currently working. The provider layer normalizes
    // `prompt_tokens` to the TOTAL input footprint (already includes the cache
    // read + creation subsets — see `anthropic_usage`), so the denominator is
    // `prompt_tokens` alone; adding cache_read/cache_creation on top would
    // double-count them. Matches the engine's `cache_read / prompt_tokens`.
    let cacheHitPct: number | null = null
    let contextTokens: number | null = null
    const latest = this.state.latestUsage
    if (latest !== null && latest.prompt_tokens > 0) {
      cacheHitPct = (latest.cache_read_tokens / latest.prompt_tokens) * 100
      // The latest step's total input footprint = current context size after
      // the last turn. Drives the "上下文" metric in the strip (issue #35).
      contextTokens = latest.prompt_tokens
    }
    return {
      turns: turnTimings.length,
      steps,
      firstTokenMs,
      totalCompletion,
      totalPrompt,
      cacheHitPct,
      totalDurationMs,
      contextTokens,
    }
  }

  /** Merge a restored baseline with the live (post-switch) metrics.
   *  Cumulative fields (turns/steps/tokens/duration) SUM — the baseline
   *  covers history + earlier live turns, the live portion covers turns seen
   *  since the last switch. Per-step fields (first-token latency, cache hit,
   *  context size) PREFER live when the latest usage is present this switch,
   *  else fall back to the baseline's last-known value. */
  private mergeMetrics(baseline: SessionMetrics, live: SessionMetrics): SessionMetrics {
    const hasLiveUsage = live.cacheHitPct !== null || live.contextTokens !== null
    return {
      turns: baseline.turns + live.turns,
      steps: baseline.steps + live.steps,
      firstTokenMs: live.firstTokenMs ?? baseline.firstTokenMs,
      totalCompletion: baseline.totalCompletion + live.totalCompletion,
      totalPrompt: baseline.totalPrompt + live.totalPrompt,
      cacheHitPct: hasLiveUsage ? live.cacheHitPct : baseline.cacheHitPct,
      totalDurationMs: baseline.totalDurationMs + live.totalDurationMs,
      contextTokens: hasLiveUsage ? live.contextTokens : baseline.contextTokens,
    }
  }

  // ── trajectory ledger helpers ──────────────────────────────────────────────

  /** Elapsed ms since the given turn's turn_start (browser performance.now
   *  delta). null when the turn wasn't seen starting. */
  private elapsedSinceTurnStart(turnId: string | null): number | null {
    if (turnId === null) return null
    const start = this.state.turnStartPerf.get(turnId)
    if (start === undefined) return null
    return Math.round(performance.now() - start)
  }

  /** Apply a working-state event (TaskEventPayload) to the working projection
   *  + push a trajectory entry. The working projection is rebuilt from the
   *  event stream (mirrors the engine's derive_state, but lean — only the
   *  fields the UI renders). */
  private applyWorkingStateEvent(ev: import('../rpc/types').TaskEventPayload): void {
    const w = this.state.working
    const curTurn = this.state.currentTurnId
    switch (ev.kind) {
      case 'task':
        w.goal = ev.goal
        w.intent = ev.intent ?? null
        this.pushTrajectory(curTurn, 'working_state', `goal: ${ev.goal}`, { detail: { kind: 'working_state', event: ev } })
        break
      case 'step_added':
        w.steps = [...w.steps, ev.step]
        this.pushTrajectory(curTurn, 'working_state', `step added: ${ev.step.description}`, { detail: { kind: 'working_state', event: ev } })
        break
      case 'step_status_changed': {
        w.steps = w.steps.map((s) =>
          s.id === ev.step_id
            ? {
                ...s,
                status: ev.status,
                active_form: ev.active_form !== undefined ? ev.active_form : s.active_form,
              }
            : s,
        )
        this.pushTrajectory(curTurn, 'working_state', `step ${ev.step_id} → ${ev.status}`, { detail: { kind: 'working_state', event: ev } })
        break
      }
      case 'decision_made':
        w.decisions = [...w.decisions, ev.decision]
        this.pushTrajectory(curTurn, 'working_state', `decision: ${ev.decision.chosen}`, { detail: { kind: 'working_state', event: ev } })
        break
      case 'blocker_raised':
        w.blockers = [...w.blockers, ev.blocker]
        this.pushTrajectory(curTurn, 'working_state', `blocker: ${ev.blocker.description}`, { detail: { kind: 'working_state', event: ev } })
        break
      case 'blocker_resolved':
        w.blockers = w.blockers.map((b) =>
          b.id === ev.blocker_id ? { ...b, status: 'resolved', resolution: ev.resolution } : b,
        )
        this.pushTrajectory(curTurn, 'working_state', `blocker resolved: ${ev.blocker_id}`, { detail: { kind: 'working_state', event: ev } })
        break
      case 'note_added':
        w.notes = [...w.notes, ev.note]
        this.pushTrajectory(curTurn, 'working_state', `note: ${ev.note.content.slice(0, 60)}`, { detail: { kind: 'working_state', event: ev } })
        break
      case 'snapshot': {
        // A materialized checkpoint — replace the working projection wholesale.
        const s = ev.state
        this.state.working = {
          goal: s.goal ?? null,
          intent: s.intent ?? null,
          steps: s.steps ?? [],
          decisions: s.decisions ?? [],
          blockers: s.blockers ?? [],
          notes: s.notes ?? [],
        }
        this.pushTrajectory(curTurn, 'working_state', 'working-state snapshot', { detail: { kind: 'working_state', event: ev } })
        break
      }
      case 'task_status':
      case 'reflection_fired':
        this.pushTrajectory(curTurn, 'working_state', ev.kind === 'reflection_fired' ? 'reflection fired' : 'task status', { detail: { kind: 'working_state', event: ev } })
        break
      default:
        // Unknown TaskEventPayload kind — ignore (non_exhaustive contract).
        break
    }
    this.state.working = { ...this.state.working }
  }

  /** Semantic phase within an iteration, for `pos` ordering (issue #40
   *  follow-up): the trajectory must read context → infer → tool regardless of
   *  when each event actually landed on the bus (the streaming path emits
   *  `tool_calls` mid-stream, BEFORE `inference` fires at stream end). */
  private phaseForKind(kind: TrajectoryEntry['kind']): number {
    switch (kind) {
      case 'context_assembled':
        return 0
      case 'iteration_start':
        return 1
      case 'approval_request':
        return 2
      case 'tool_calls':
      case 'tool_result':
      case 'direct_answer':
      case 'delegate':
      case 'paradigm_switch':
        return 3
      // Delegation lifecycle: child-lane progress/complete must sort AFTER the
      // fork (phase 3) so fork/join edges always point forward on the timeline.
      case 'delegate_progress':
        return 4
      case 'delegate_complete':
        return 5
      default:
        return 1
    }
  }

  /** Logical sort key: `iteration * 1000 + phase` keeps iterations strictly
   *  ordered while letting context/infer/tool within an iteration order
   *  correctly. */
  private posFor(iteration: number, phase: number): number {
    return iteration * 1000 + phase
  }

  /** Push a trajectory entry (non-hot → caller flushes via emitNow). */
  private pushTrajectory(
    turnId: string | null,
    kind: TrajectoryEntry['kind'],
    title: string,
    extra?: Partial<TrajectoryEntry>,
  ): void {
    this.state.trajectory = [
      ...this.state.trajectory,
      {
        seq: nextTrajectorySeq(),
        turnId,
        at: Date.now(),
        ms: this.elapsedSinceTurnStart(turnId),
        kind,
        title,
        pos: this.posFor(this.currentIteration, this.phaseForKind(kind)),
        ...extra,
      },
    ]
  }

  /** Reset the session-scoped ledgers (new session / scenario start). Does
   *  NOT touch `metricsBaseline` — switch handlers manage that so a fresh
   *  start zeroes it while a session switch restores the cached baseline. */
  private resetLedgers(): void {
    this.state.trajectory = []
    this.state.working = { ...EMPTY_WORKING }
    this.state.subagents = []
    this.state.backgroundTasks = []
    this.state.usage = { usage: null, context: null }
    this.state.turnStartPerf.clear()
    this.state.turnEndPerf.clear()
    this.state.turnIterations.clear()
    this.state.firstTokenPerf.clear()
    this.state.turnUsage.clear()
    this.state.latestUsage = null
    this.iterBuffer = null
    this.ctxCache.clear()
    this.currentIterSeq = null
    this.currentIteration = 0
  }

  // ── Issue #40 trajectory helpers ───────────────────────────────────────────

  /** Wall-clock timestamp for a yield: the engine's persisted `ts` when present
   *  (a replayed historical event), else now (live arrival time). */
  private eventTs(y: EngineYield): number {
    const t = (y as { ts?: unknown }).ts
    return typeof t === 'number' ? t : Date.now()
  }

  /** Patch one ledger entry by seq (immutable replacement). */
  private patchTrajectory(seq: number, patch: Partial<TrajectoryEntry>): void {
    this.state.trajectory = this.state.trajectory.map((e) =>
      e.seq === seq ? { ...e, ...patch } : e,
    )
  }

  /** Flush the accumulated reasoning text into its iteration entry. */
  private finalizeIterBuffer(): void {
    const b = this.iterBuffer
    if (b === null) return
    this.iterBuffer = null
    const entry = this.state.trajectory.find((e) => e.seq === b.entrySeq)
    if (!entry) return
    const d = entry.detail
    if (d && d.kind === 'iteration') {
      this.patchTrajectory(b.entrySeq, {
        detail: { ...d, inference: b.inference, thinking: b.thinking },
      })
    }
  }

  /** Open a fresh iteration entry and point the reasoning buffer at it. */
  private startIterationBuffer(turnId: string, iteration: number, paradigm: string, at: number): void {
    this.finalizeIterBuffer()
    const seq = nextTrajectorySeq()
    this.currentIteration = iteration
    this.currentIterSeq = seq
    this.state.trajectory = [
      ...this.state.trajectory,
      {
        seq,
        turnId,
        at,
        ms: this.elapsedSinceTurnStart(turnId),
        kind: 'iteration_start',
        title: `iteration ${iteration} · ${paradigm}`,
        iter: iteration,
        paradigm,
        pos: this.posFor(iteration, this.phaseForKind('iteration_start')),
        detail: { kind: 'iteration', iteration, paradigm, inference: '', thinking: '' },
      },
    ]
    this.iterBuffer = { turnId, entrySeq: seq, inference: '', thinking: '' }
  }

  /** Backfill a matching tool node in the ledger with its result + duration. */
  private backfillToolResult(
    turnId: string,
    callId: string,
    toolName: string,
    output: { success: boolean; content: string; error?: string },
    at: number,
  ): void {
    let idx = -1
    for (let i = this.state.trajectory.length - 1; i >= 0; i--) {
      const e = this.state.trajectory[i]
      if (e.detail && e.detail.kind === 'tool' && e.detail.callId === callId && e.detail.result === undefined) {
        idx = i
        break
      }
    }
    if (idx >= 0) {
      const e = this.state.trajectory[idx]
      const d = e.detail as Extract<TrajectoryDetail, { kind: 'tool' }>
      this.patchTrajectory(e.seq, {
        detail: {
          ...d,
          result: output.success ? output.content : (output.error ?? output.content),
          error: output.error,
          ok: output.success,
          durationMs: at - e.at,
        },
      })
    } else {
      this.pushTrajectory(turnId, 'tool_result', `tool_result: ${toolName}`, {
        at,
        detail: {
          kind: 'tool',
          callId,
          name: toolName,
          args: undefined,
          result: output.success ? output.content : (output.error ?? output.content),
          error: output.error,
          ok: output.success,
        },
      })
    }
  }

  /** Resolve a context assembly snapshot against the cache; update the cache
   *  for sections that carry content. Returns the resolved sections. */
  private resolveContextSections(sections: ContextSection[]): ResolvedContextSection[] {
    const out: ResolvedContextSection[] = []
    for (const s of sections) {
      const key = contextKeyString(s.key)
      const changed = s.content !== undefined
      if (s.content !== undefined) this.ctxCache.set(key, s.content)
      const content = s.content !== undefined ? s.content : (this.ctxCache.get(key) ?? '')
      out.push({ key: s.key, label: s.label, tokens: s.tokens, content, changed })
    }
    return out
  }

  /** Find the `at` of the most recent tool node in `turnId` (for placing an
   *  approval just before the tool it gates). null when none yet. */
  private latestToolAt(turnId: string | null): number | null {
    for (let i = this.state.trajectory.length - 1; i >= 0; i--) {
      const e = this.state.trajectory[i]
      if (e.detail && e.detail.kind === 'tool' && e.turnId === turnId) return e.at
    }
    return null
  }

  /** Replay a historical session's trajectory. Events are fed through the
   *  trajectory-only path (no chat nodes — those come from session_loaded). */
  private replayEvents(events: EngineYield[]): void {
    if (events.length === 0) return
    for (const y of events) this.consumeReplay(y)
    this.emitNow()
  }

  /** Load + replay the persisted trajectory for a session (issue #40). */
  async loadSessionTrajectory(sessionId: string): Promise<void> {
    let result: { ok: boolean; events?: string[]; error?: string }
    try {
      result = await this.rpc.call<{ id: string }, { ok: boolean; events?: string[]; error?: string }>(
        'session/trajectory',
        { id: sessionId },
      )
    } catch {
      // method-not-found (older engine) or offline — live-only trajectory.
      return
    }
    if (!result.ok || !result.events || result.events.length === 0) return
    const events: EngineYield[] = []
    for (const line of result.events) {
      try {
        events.push(JSON.parse(line) as EngineYield)
      } catch {
        // skip a corrupt line (the store already guards against this)
      }
    }
    this.replayEvents(events)
  }

  /** Trajectory-only consumption path for replayed (historical) events — feeds
   *  the ledger + working projection but never touches chat nodes (which
   *  `session_loaded.messages` already built). Mirrors the live cases but is
   *  deliberately lean: stream/thinking/direct_answer are excluded by the tap
   *  whitelist, so they never arrive here. */
  private consumeReplay(y: EngineYield): void {
    const at = this.eventTs(y)
    switch (y.kind) {
      case 'turn_start':
        this.pushTrajectory(y.turn_id, 'turn_start', y.task.length > 0 ? y.task : 'turn start', {
          at,
          detail: { kind: 'turn', task: y.task },
        })
        break
      case 'iteration_start':
        this.startIterationBuffer(y.turn_id, y.iteration, y.paradigm, at)
        break
      case 'stream_chunk':
        if (this.iterBuffer) this.iterBuffer.inference += y.text
        break
      case 'thinking':
        if (this.iterBuffer) this.iterBuffer.thinking += y.text
        break
      case 'direct_answer':
        this.finalizeIterBuffer()
        break
      case 'tool_calls':
        this.finalizeIterBuffer()
        for (const c of y.calls) {
          // The engine emits the same call twice in streaming mode — once per
          // `ToolCallComplete` (mid-stream) and again from `AgentDecision::ToolCalls`
          // at stream end — and both are persisted. Dedupe so a historical replay
          // doesn't split one tool call into duplicate nodes (the live `consume`
          // path already guards this against the chat node ledger).
          if (
            this.state.trajectory.some(
              (e) => e.turnId === y.turn_id && e.detail?.kind === 'tool' && e.detail.callId === c.id,
            )
          ) {
            continue
          }
          this.pushTrajectory(y.turn_id, 'tool_calls', `tool: ${c.name}`, {
            at,
            detail: { kind: 'tool', callId: c.id, name: c.name, args: c.args },
          })
        }
        break
      case 'tool_result':
        this.backfillToolResult(y.turn_id, y.call_id, y.tool_name, y.output, at)
        break
      case 'plan_update':
        if (y.plan !== null) {
          const plan = y.plan as { steps?: unknown[]; revision?: number }
          this.pushTrajectory(y.turn_id, 'plan_revision', 'plan revised', {
            at,
            detail: {
              kind: 'plan',
              steps: plan.steps?.length ?? 0,
              revision: typeof plan.revision === 'number' ? plan.revision : undefined,
              plan: y.plan,
            },
          })
        }
        break
      case 'paradigm_switch':
        // Same no-op filter as the live path — a `from === to` switch is a
        // spurious activation (e.g. replayed `re_act → re_act`), not a node.
        if (y.from !== y.to) {
          this.pushTrajectory(y.turn_id, 'paradigm_switch', `paradigm: ${y.from} → ${y.to}`, {
            at,
            paradigm: y.to,
            detail: { kind: 'paradigm', from: y.from, to: y.to },
          })
        }
        break
      case 'delegate': {
        const kindLabel =
          typeof y.agent_kind === 'string' ? y.agent_kind : `Custom:${y.agent_kind.custom}`
        this.pushTrajectory(y.turn_id, 'delegate', `delegate → ${kindLabel}`, {
          at,
          detail: {
            kind: 'delegate',
            taskId: y.task_id,
            task: y.task,
            agentKind: y.agent_kind,
            dependsOn: y.depends_on ?? [],
          },
        })
        break
      }
      case 'delegate_progress':
        this.pushTrajectory(y.turn_id, 'delegate_progress', `bg progress: ${y.event.kind}`, {
          at,
          detail: { kind: 'delegate_progress', taskId: y.task_id, event: y.event },
        })
        break
      case 'delegate_complete': {
        const summary = {
          summary: y.summary.summary,
          keyFindings: y.summary.key_findings,
          budgetExceeded: y.summary.budget_exceeded,
          tokensUsed: y.summary.tokens_used,
          completed: y.summary.completed,
        }
        this.pushTrajectory(y.turn_id, 'delegate_complete', 'delegate complete', {
          at,
          detail: { kind: 'delegate_complete', taskId: y.task_id, summary },
        })
        break
      }
      case 'working_state':
        this.applyWorkingStateEvent(y.event)
        break
      case 'context_assembled': {
        const sections = this.resolveContextSections(y.sections)
        this.pushTrajectory(y.turn_id, 'context_assembled', `context assembled · iter ${y.iteration}`, {
          at,
          detail: {
            kind: 'context',
            iteration: y.iteration,
            sections,
            durationMs: y.duration_ms,
          },
        })
        break
      }
      case 'token_usage': {
        // Attribute to the current iteration entry (latest per iteration wins).
        const iterSeq = this.currentIterSeq
        if (iterSeq !== null) {
          const entry = this.state.trajectory.find((e) => e.seq === iterSeq)
          if (entry?.detail && entry.detail.kind === 'iteration') {
            this.patchTrajectory(iterSeq, { detail: { ...entry.detail, usage: y.usage } })
          }
        }
        break
      }
      case 'inference': {
        const iterSeq = this.currentIterSeq
        if (iterSeq !== null) {
          const entry = this.state.trajectory.find((e) => e.seq === iterSeq)
          if (entry?.detail && entry.detail.kind === 'iteration') {
            this.patchTrajectory(iterSeq, {
              detail: {
                ...entry.detail,
                inferenceDetail: y.snapshot,
                durationMs: y.snapshot.duration_ms,
              },
            })
          }
        }
        break
      }
      case 'tools_added':
        this.pushTrajectory(y.turn_id, 'tools_added', `+tools: ${y.names.join(', ')}`, {
          at,
          detail: { kind: 'tools_added', names: y.names },
        })
        break
      case 'interrupted':
        this.pushTrajectory(y.turn_id, 'interrupted', `interrupted: ${y.reason}`, {
          at,
          detail: { kind: 'interrupted', reason: y.reason, point: y.point },
        })
        break
      case 'reflection':
        this.pushTrajectory(y.turn_id, 'reflection', 'reflection', {
          at,
          detail: { kind: 'reflection', summary: y.summary },
        })
        break
      case 'error':
        this.pushTrajectory(this.state.currentTurnId, 'error', y.message, {
          at,
          detail: { kind: 'error', message: y.message, recoverable: y.recoverable },
        })
        break
      case 'turn_complete':
        this.finalizeIterBuffer()
        this.pushTrajectory(y.turn_id, 'turn_complete', 'turn complete', {
          at,
          detail: { kind: 'turn_complete' },
        })
        break
      case 'approval_request': {
        let approvalAt = at
        const toolAt = this.latestToolAt(this.state.currentTurnId)
        if (toolAt !== null) approvalAt = toolAt - 1
        this.pushTrajectory(this.state.currentTurnId, 'approval_request', 'approval request', {
          at: approvalAt,
          detail: { kind: 'approval', requestId: y.request_id, request: y.request },
        })
        break
      }
      default:
        break
    }
  }

  /** Build the per-turn timing array from the live perf maps (turns seen since
   *  the last switch — historical turns replayed on `session_loaded` aren't
   *  in these maps). Shared by `emit()` and `stashMetrics()`. */
  private currentTurnTimings(): {
    turnId: string
    startedAt: number | null
    endedAt: number | null
    iterations: number
  }[] {
    return Array.from(this.state.turnStartPerf.keys()).map((turnId) => ({
      turnId,
      startedAt: this.state.turnStartPerf.get(turnId) ?? null,
      endedAt: this.state.turnEndPerf.get(turnId) ?? null,
      iterations: this.state.turnIterations.get(turnId) ?? 0,
    }))
  }

  /** Snapshot the current session's merged metrics into the per-session cache,
   *  so a later switch back restores them (issue #35). Call BEFORE resetting
   *  ledgers / reassigning `sessionId` — the live perf maps must still hold
   *  the outgoing session's turns. No-op when `id` is null (group chat / no
   *  session bound). */
  private stashMetrics(id: string | null): void {
    if (id === null) return
    const live = this.computeMetrics(this.currentTurnTimings())
    this.metricsCache.set(id, this.mergeMetrics(this.state.metricsBaseline, live))
  }

  /** Restore (or clear) the metrics baseline for the session being switched
   *  to. Called AFTER `resetLedgers()` so the live perf maps are empty and the
   *  baseline is the sole source until live turns arrive. */
  private restoreBaseline(id: string | null): void {
    this.state.metricsBaseline =
      id !== null ? (this.metricsCache.get(id) ?? { ...EMPTY_METRICS }) : { ...EMPTY_METRICS }
  }

  /** Flush now (bypass the coalescer) — for non-hot mutations. */
  private emitNow(): void {
    this.coalescer.flushNow()
  }

  // ── user actions ────────────────────────────────────────────────────────────

  /** Optimistically append a user node before the engine confirms the turn.
   * `images` are the user's drag-dropped/pasted image content blocks, surfaced
   * as thumbnails in the user bubble (W4 attachments). */
  pushUserMessage(text: string, images?: ContentBlock[]): void {
    const attachments = images && images.length > 0 ? images : undefined
    this.state.nodes = [
      ...this.state.nodes,
      {
        id: nextNodeId(),
        role: 'user',
        kind: 'user',
        text,
        speaker: null,
        turnId: null,
        state: 'done',
        attachments,
      },
    ]
    this.emitNow()
  }

  /** Append a client-local system note (slash-command feedback — issue #39).
   * Purely presentational: never submitted to the engine, cleared with the
   * session, gone on reload — the web mirror of the TUI's system messages. */
  addSystemNote(text: string): void {
    this.state.nodes = [
      ...this.state.nodes,
      {
        id: nextNodeId(),
        role: 'system',
        kind: 'note',
        text,
        speaker: null,
        turnId: null,
        state: 'done',
      },
    ]
    this.emitNow()
  }

  /** Append an assistant placeholder for a turn about to start (optimistic). */
  private seedAssistant(turnId: string, speaker: string | null): ChatNode {
    const node: ChatNode = {
      id: nextNodeId(),
      role: 'assistant',
      kind: 'text',
      text: '',
      speaker,
      turnId,
      state: 'streaming',
    }
    this.state.nodes = [...this.state.nodes, node]
    this.state.currentSpeaker = speaker
    return node
  }

  private streamingNodeFor(turnId: string, speaker: string | null): ChatNode | null {
    // Find the last streaming text node for this turn whose speaker matches
    // (speaker routing: a different speaker within one turn seeds a new node).
    for (let i = this.state.nodes.length - 1; i >= 0; i -= 1) {
      const n = this.state.nodes[i]
      if (n.turnId !== turnId) continue
      if (n.kind !== 'text') continue
      if (n.state !== 'streaming') continue
      if (n.speaker !== speaker) return null // different speaker → new node
      return n
    }
    return null
  }

  private replaceNode(id: string, patch: Partial<ChatNode>): void {
    this.state.nodes = this.state.nodes.map((n) =>
      n.id === id ? { ...n, ...patch } : n,
    )
  }

  /** Mark all streaming text nodes for a turn done (finalize before tools). */
  private finalizeStreamingText(turnId: string): void {
    this.state.nodes = this.state.nodes.map((n) =>
      n.turnId === turnId && n.kind === 'text' && n.state === 'streaming'
        ? { ...n, state: 'done' as NodeState }
        : n,
    )
  }

  /** Mark any streaming thinking node for a turn done. A turn runs multiple
   *  iterations, each may emit its own `thinking` fragment; without this, the
   *  2nd iteration's thinking would APPEND to the 1st's still-streaming node
   *  and the whole turn's reasoning would collapse into one giant "stuck"
   *  block. Finalizing at each iteration boundary (iteration_start /
   *  tool_calls / direct_answer) makes each iteration's reasoning its own
   *  block — so the user sees distinct per-iteration thinking, not one
   *  ever-growing blob. */
  private finalizeStreamingThinking(turnId: string): void {
    this.state.nodes = this.state.nodes.map((n) =>
      n.turnId === turnId && n.kind === 'thinking' && n.state === 'streaming'
        ? { ...n, state: 'done' as NodeState }
        : n,
    )
  }

  // ── EngineYield consumer ───────────────────────────────────────────────────

  /** Apply a single EngineYield, mutating the working state + scheduling a
   * snapshot re-build (coalesced for hot variants, immediate otherwise).
   *
   * Public so tests can drive the store directly with a scripted yield
   * sequence (no real ws). Production wiring still goes through `attach()`
   * → `rpc.onEvent` → here. */
  consume(y: EngineYield): void {
    switch (y.kind) {
      case 'turn_start': {
        this.state.currentTurnId = y.turn_id
        this.state.turnActive = true
        // A new turn supersedes any stale operational error (e.g. a prior
        // action's RPC failure surfaced in the header banner) — clear it so
        // the banner doesn't linger across turns (issue #34).
        this.state.lastError = null
        this.state.turnStartPerf.set(y.turn_id, performance.now())
        this.state.turnEndPerf.delete(y.turn_id)
        this.state.turnIterations.set(y.turn_id, 0)
        // Do NOT eagerly seed an empty assistant text bubble here. A thinking
        // fragment (Anthropic/o1-style) arrives before the answer; with an
        // eager text node already on the list, the thinking node appended after
        // it would render BELOW the answer. Deferring the seed to the first
        // `stream_chunk`/`direct_answer` lets a leading `thinking` event own
        // the top slot — and the `TypingDots` row already signals "in flight"
        // so no progress affordance is lost. Group chat keeps deferring to
        // `speaker_turn`/`stream_chunk` (they carry the member id).
        this.pushTrajectory(y.turn_id, 'turn_start', y.task.length > 0 ? y.task : 'turn start', {
          at: this.eventTs(y),
          detail: { kind: 'turn', task: y.task },
        })
        this.emitNow()
        break
      }
      case 'speaker_turn': {
        // Round-level boundary in a group turn: finalize the previous
        // speaker's streaming text node for this turn (so its bubble flips to
        // done as the round progresses, not all-at-once at turn_complete) and
        // record the new speaker so the next stream_chunk seeds a fresh node.
        // Also finalize any still-streaming THINKING node for the turn — the
        // previous speaker's run is over, so a lingering streaming cursor on
        // its reasoning fragment would otherwise read as "still outputting"
        // while the next speaker already began (scenario UX: writing workshop
        // editor appearing before the writer's block visually settled).
        this.finalizeStreamingText(y.turn_id)
        this.state.nodes = this.state.nodes.map((n) =>
          n.turnId === y.turn_id && n.kind === 'thinking' && n.state === 'streaming'
            ? { ...n, state: 'done' as NodeState }
            : n,
        )
        this.state.currentSpeaker = y.speaker
        this.coalescer.request()
        break
      }
      case 'stream_chunk': {
        const node = this.streamingNodeFor(y.turn_id, y.speaker)
        if (node !== null) {
          this.replaceNode(node.id, { text: node.text + y.text })
        } else {
          // Speaker changed (group chat) or no streaming node — seed a new one.
          this.seedAssistant(y.turn_id, y.speaker)
          const seeded = this.streamingNodeFor(y.turn_id, y.speaker)
          if (seeded !== null) {
            this.replaceNode(seeded.id, { text: seeded.text + y.text })
          }
        }
        // First stream_chunk for this turn = first generated token; record
        // its arrival for the first-token-latency metric.
        if (!this.state.firstTokenPerf.has(y.turn_id)) {
          this.state.firstTokenPerf.set(y.turn_id, performance.now())
        }
        // Issue #40: accumulate per-iteration reasoning into the buffer.
        if (this.iterBuffer) this.iterBuffer.inference += y.text
        this.coalescer.request()
        break
      }
      case 'thinking': {
        // Maintain a single thinking node per turn (seed if none streaming).
        const existing = this.state.nodes.findIndex(
          (n) =>
            n.turnId === y.turn_id &&
            n.kind === 'thinking' &&
            n.state === 'streaming',
        )
        if (existing >= 0) {
          const n = this.state.nodes[existing]
          this.replaceNode(n.id, { text: n.text + y.text })
        } else {
          this.state.nodes = [
            ...this.state.nodes,
            {
              id: nextNodeId(),
              role: 'assistant',
              kind: 'thinking',
              text: y.text,
              speaker: y.speaker,
              turnId: y.turn_id,
              state: 'streaming',
            },
          ]
        }
        // Issue #40: accumulate per-iteration reasoning into the buffer.
        if (this.iterBuffer) this.iterBuffer.thinking += y.text
        this.coalescer.request()
        break
      }
      case 'direct_answer': {
        // The final iteration's thinking fragment closes here so the answer
        // renders as its own block, not appended to a streaming reasoning node.
        this.finalizeStreamingThinking(y.turn_id)
        const node = this.streamingNodeFor(y.turn_id, y.speaker)
        if (node !== null) {
          this.replaceNode(node.id, { text: y.text, state: 'done' })
        } else {
          this.seedAssistant(y.turn_id, y.speaker)
          const seeded = this.streamingNodeFor(y.turn_id, y.speaker)
          if (seeded !== null) {
            this.replaceNode(seeded.id, { text: y.text, state: 'done' })
          }
        }
        this.finalizeIterBuffer()
        this.emitNow()
        break
      }
      case 'tool_calls': {
        // Finalize any streaming text + thinking node for this turn so tool
        // cards interleave between finalized blocks (the next stream_chunk /
        // thinking seeds a fresh node after the cards).
        this.finalizeStreamingText(y.turn_id)
        this.finalizeStreamingThinking(y.turn_id)
        this.finalizeIterBuffer()
        for (const c of y.calls) {
          // Dedupe by call id within the turn: the engine's streaming path
          // (`on_tool_calls` per `ToolCallComplete`) and the decision path
          // (`AgentDecision::ToolCalls`) can both fire for the same call id in
          // one iteration. Without this guard each emission spawned a fresh
          // pending card, leaving duplicate "tool" blocks (one stuck pending
          // because tool_result only matches the first). Refresh args in place.
          const existingIdx = this.state.nodes.findIndex(
            (n) => n.kind === 'tool' && n.callId === c.id && n.turnId === y.turn_id,
          )
          if (existingIdx >= 0) {
            const n = this.state.nodes[existingIdx]
            this.replaceNode(n.id, {
              toolName: n.toolName ?? c.name,
              toolArgs: c.args,
              toolState: n.toolState ?? 'pending',
            })
            continue
          }
          this.state.nodes = [
            ...this.state.nodes,
            {
              id: nextNodeId(),
              role: 'assistant',
              kind: 'tool',
              text: '',
              speaker: y.speaker,
              turnId: y.turn_id,
              state: 'done',
              callId: c.id,
              toolName: c.name,
              toolArgs: c.args,
              toolState: 'pending',
            },
          ]
          this.pushTrajectory(y.turn_id, 'tool_calls', `tool: ${c.name}`, {
            at: this.eventTs(y),
            detail: { kind: 'tool', callId: c.id, name: c.name, args: c.args },
          })
        }
        this.emitNow()
        break
      }
      case 'tool_result': {
        // Locate the pending tool node for this call id and attach the
        // result. A tool_result without a matching tool_calls node (e.g.
        // resumed session) seeds a standalone node.
        const idx = this.state.nodes.findIndex(
          (n) =>
            n.kind === 'tool' &&
            n.callId === y.call_id &&
            n.turnId === y.turn_id,
        )
        const toolState: ToolState = y.output.success ? 'done' : 'error'
        if (idx >= 0) {
          const n = this.state.nodes[idx]
          this.replaceNode(n.id, {
            toolName: n.toolName ?? y.tool_name,
            toolOutput: y.output,
            toolState,
          })
        } else {
          this.state.nodes = [
            ...this.state.nodes,
            {
              id: nextNodeId(),
              role: 'assistant',
              kind: 'tool',
              text: '',
              speaker: y.speaker,
              turnId: y.turn_id,
              state: 'done',
              callId: y.call_id,
              toolName: y.tool_name,
              toolOutput: y.output,
              toolState,
            },
          ]
        }
        // Issue #40: backfill the trajectory tool node with result + duration.
        this.backfillToolResult(y.turn_id, y.call_id, y.tool_name, y.output, this.eventTs(y))
        this.emitNow()
        break
      }
      case 'paradigm_switch': {
        this.state.paradigm = y.to
        // A no-op switch (the engine forcing the already-active paradigm, e.g.
        // the initial `re_act → re_act` the directive pump emits when the
        // frontend selects re_act before any inference) is not a real
        // transition — suppress the spurious trajectory node.
        if (y.from !== y.to) {
          this.pushTrajectory(y.turn_id, 'paradigm_switch', `paradigm: ${y.from} → ${y.to}`, {
            at: this.eventTs(y),
            paradigm: y.to,
            detail: { kind: 'paradigm', from: y.from, to: y.to },
          })
        }
        this.emitNow()
        break
      }
      case 'plan_update': {
        // Seed-or-update a single plan node per turn (keyed `plan:<turn_id>`)
        // so the live plan renders in place as the planner revises it.
        const planId = `plan:${y.turn_id}`
        const existing = this.state.nodes.find((n) => n.id === planId)
        const plan: PlanState | null = y.plan
        if (plan === null) {
          // Plan cleared — drop the node if present.
          if (existing !== undefined) {
            this.state.nodes = this.state.nodes.filter((n) => n.id !== planId)
          }
        } else if (existing !== undefined) {
          this.replaceNode(existing.id, {
            planSteps: plan.steps,
            planRevision:
              typeof plan.revision === 'number' ? plan.revision : existing.planRevision,
          })
        } else {
          this.state.nodes = [
            ...this.state.nodes,
            {
              id: planId,
              role: 'assistant',
              kind: 'plan',
              text: '',
              speaker: null,
              turnId: y.turn_id,
              state: 'done',
              planSteps: plan.steps,
              planRevision: typeof plan.revision === 'number' ? plan.revision : 0,
            },
          ]
        }
        if (plan !== null) {
          this.pushTrajectory(y.turn_id, 'plan_revision', 'plan revised', {
            at: this.eventTs(y),
            detail: {
              kind: 'plan',
              steps: plan.steps.length,
              revision: typeof plan.revision === 'number' ? plan.revision : undefined,
              plan: y.plan,
            },
          })
        } else {
          this.pushTrajectory(y.turn_id, 'plan_revision', 'plan cleared', {
            at: this.eventTs(y),
            detail: { kind: 'plan', steps: 0, revision: undefined, plan: null },
          })
        }
        this.emitNow()
        break
      }      case 'approval_request': {
        // Parallel approval queue (issue #20): a second approval_request that
        // arrives before the first is resolved enqueues behind the head; the
        // store promotes the next on respond.
        approvalSeq += 1
        this.state.approvalQueue = [
          ...this.state.approvalQueue,
          { request_id: y.request_id, request: y.request, seq: approvalSeq },
        ]
        // Place the approval just BEFORE the tool it gates (issue #40 — an
        // approval gates execution, so it must sit to the left of the tool
        // node, not overlap it).
        let approvalAt = this.eventTs(y)
        const toolAt = this.latestToolAt(this.state.currentTurnId)
        if (toolAt !== null) approvalAt = toolAt - 1
        this.pushTrajectory(this.state.currentTurnId, 'approval_request', 'approval request', {
          at: approvalAt,
          detail: { kind: 'approval', requestId: y.request_id, request: y.request },
        })
        this.emitNow()
        // Auto-accept: silently allow tool execution (Proceed) without showing
        // the approval bar. Plan/network/elicitation still queue.
        if (this.state.autoApprove && Object.keys(y.request)[0] === 'ToolApproval') {
          void this.respondApproval(y.request_id, { Proceed: null })
        }
        break
      }
      case 'turn_complete': {
        // Mark any streaming nodes for this turn done.
        // W4 A3 — collect this turn's tool-result artifacts into a deliverable
        // strip on the final assistant text node of the turn (the last text
        // node carrying this turn_id). Reads `ToolOutput.artifacts` that file
        // tools (write_file/apply_patch) populated.
        const turnArtifacts: Artifact[] = []
        for (const n of this.state.nodes) {
          if (
            n.turnId === y.turn_id &&
            n.kind === 'tool' &&
            n.toolOutput?.artifacts &&
            n.toolOutput.artifacts.length > 0
          ) {
            turnArtifacts.push(...n.toolOutput.artifacts)
          }
        }
        this.state.nodes = this.state.nodes.map((n) => {
          if (n.turnId === y.turn_id && n.state === 'streaming') {
            return { ...n, state: 'done' as NodeState }
          }
          return n
        })
        if (turnArtifacts.length > 0) {
          // Attach to the last assistant text node of the turn (if any).
          let lastTextIdx = -1
          for (let i = this.state.nodes.length - 1; i >= 0; i--) {
            const n = this.state.nodes[i]
            if (n.turnId === y.turn_id && n.role === 'assistant' && n.kind === 'text') {
              lastTextIdx = i
              break
            }
          }
          if (lastTextIdx >= 0) {
            const n = this.state.nodes[lastTextIdx]
            this.replaceNode(n.id, { deliverables: turnArtifacts })
          }
        }
        this.state.turnActive = false
        this.state.currentSpeaker = null
        this.state.turnEndPerf.set(y.turn_id, performance.now())
        this.finalizeIterBuffer()
        this.pushTrajectory(y.turn_id, 'turn_complete', 'turn complete', {
          at: this.eventTs(y),
          detail: { kind: 'turn_complete' },
        })
        this.emitNow()
        break
      }
      case 'error': {
        // A turn-level error is surfaced as an in-conversation error node
        // (appended below) — do NOT also set `lastError` (the header banner),
        // or the same message shows twice (issue #34). `lastError` stays
        // reserved for non-conversational failures (RPC/action errors) that
        // have no chat node of their own.
        // Mark streaming nodes for the current turn as errored; always add an
        // error node so the message is visible.
        this.state.nodes = [
          ...this.state.nodes.map((n) =>
            this.state.currentTurnId !== null &&
            n.turnId === this.state.currentTurnId &&
            n.state === 'streaming'
              ? { ...n, state: 'error' as NodeState }
              : n,
          ),
          {
            id: nextNodeId(),
            role: 'system',
            kind: 'error',
            text: y.message,
            speaker: null,
            turnId: this.state.currentTurnId,
            state: y.recoverable ? 'streaming' : 'error',
          },
        ]
        if (!y.recoverable) this.state.turnActive = false
        this.pushTrajectory(this.state.currentTurnId, 'error', y.message, {
          at: this.eventTs(y),
          detail: { kind: 'error', message: y.message, recoverable: y.recoverable },
        })
        this.emitNow()
        break
      }
      case 'session_created': {
        // A fresh session = empty conversation. Clear any previously-loaded
        // history so the welcome/empty state shows (otherwise a session/create
        // after loading a historical session left the old messages on screen).
        this.stashMetrics(this.state.sessionId)
        this.state.currentScenario = null
        this.state.debriefActive = false
        this.state.currentSpeaker = null
        this.state.sessionId = y.id
        this.state.nodes = []
        this.state.lastError = null
        this.resetLedgers()
        this.restoreBaseline(y.id)
        this.emitNow()
        break
      }
      case 'session_loaded': {
        // Loading a saved single-agent session leaves any active scenario —
        // a stale scenario would tag the next single-agent turn's bubbles.
        this.stashMetrics(this.state.sessionId)
        this.state.currentScenario = null
        this.state.debriefActive = false
        this.state.currentSpeaker = null
        this.state.sessionId = y.id
        this.state.nodes = messagesToNodes(y.messages)
        this.state.lastError = null
        this.resetLedgers()
        this.restoreBaseline(y.id)
        // Reloaded messages replay without a turn_id (Message doesn't persist
        // it), so per-message feedback is not available on history — the UI
        // hides 👍/👎 on these nodes. New live turns in this loaded session
        // do carry a turn_id and are feedbackable.
        // Issue #40: rebuild this historical session's trajectory from the
        // persisted bus-event log (no-op when the engine has no store).
        void this.loadSessionTrajectory(y.id)
        this.emitNow()
        break
      }
      case 'session_cleared': {
        // `/clear` — fresh backend conversation for the live session; the
        // visible transcript empties. The backend conversation is wiped, so
        // its cached metrics are stale — drop them and zero the baseline so
        // the strip doesn't keep counting the cleared turns.
        this.state.sessionId = y.id
        this.state.nodes = []
        this.state.currentSpeaker = null
        this.state.lastError = null
        this.metricsCache.delete(y.id)
        this.resetLedgers()
        this.restoreBaseline(y.id)
        this.emitNow()
        break
      }
      case 'session_deleted': {
        // Only empty the view if the *active* session was the one deleted;
        // deleting a different (sidebar) session must not wipe the live view.
        if (this.state.sessionId === y.id) {
          this.state.sessionId = null
          this.state.nodes = []
          this.state.currentSpeaker = null
          this.state.lastError = null
          this.metricsCache.delete(y.id)
          this.resetLedgers()
          this.restoreBaseline(null)
        } else {
          this.metricsCache.delete(y.id)
        }
        this.emitNow()
        break
      }
      // ── W4: events the W1–W3 projection previously dropped into `default`.
      //     These feed the trajectory ledger + the capability panel (goal bar,
      //     subagent tree, context/usage overview). All pure consumers. ──
      case 'iteration_start': {
        // Close the previous iteration's thinking fragment so this
        // iteration's thinking seeds a fresh block (per-iteration reasoning,
        // not one merged blob for the whole turn).
        this.finalizeStreamingThinking(y.turn_id)
        const prev = this.state.turnIterations.get(y.turn_id) ?? 0
        this.state.turnIterations.set(y.turn_id, prev + 1)
        this.startIterationBuffer(y.turn_id, y.iteration, y.paradigm, this.eventTs(y))
        this.emitNow()
        break
      }
      case 'delegate': {
        const id = nextSubagentId(y.turn_id)
        const kindLabel = typeof y.agent_kind === 'string' ? y.agent_kind : `Custom:${y.agent_kind.custom}`
        this.state.subagents = [
          ...this.state.subagents,
          {
            id,
            turnId: y.turn_id,
            task: y.task,
            agentKind: y.agent_kind,
            status: 'active',
          },
        ]
        // Also track in the cross-turn background slice (matched by task_id)
        // so a background sub-agent launched here stays visible + updatable
        // across the turn boundary where it completes.
        if (y.task_id) {
          this.state.backgroundTasks = upsertBackgroundTask(this.state.backgroundTasks, {
            taskId: y.task_id,
            turnId: y.turn_id,
            task: y.task,
            agentKind: y.agent_kind,
            status: 'active',
          })
        }
        this.pushTrajectory(y.turn_id, 'delegate', `delegate → ${kindLabel}`, {
          at: this.eventTs(y),
          detail: {
            kind: 'delegate',
            taskId: y.task_id,
            task: y.task,
            agentKind: y.agent_kind,
            dependsOn: y.depends_on ?? [],
          },
        })
        this.emitNow()
        break
      }
      case 'delegate_progress': {
        // Mid-run progress from a (background) sub-agent — fan onto the
        // matching background-task card by task_id. iteration/tool/usage keep
        // the status strip live while the parent is also running.
        const ev = y.event
        this.state.backgroundTasks = this.state.backgroundTasks.map((t) => {
          if (t.taskId !== y.task_id) return t
          const next: BackgroundTaskNode = { ...t }
          if (ev.kind === 'iteration_start') {
            next.iteration = ev.iteration
            next.paradigm = typeof ev.paradigm === 'string' ? ev.paradigm : JSON.stringify(ev.paradigm)
          } else if (ev.kind === 'tool_result') {
            next.lastTool = ev.tool_name
            next.lastToolSnapshot = ev.snapshot
          } else if (ev.kind === 'token_usage') {
            next.usage = { prompt: ev.prompt, completion: ev.completion }
          } else if (ev.kind === 'cancelled') {
            // The engine emitted this the instant a cancel landed (the
            // registry's `cancel` fires `DelegateProgress { Cancelled }`).
            // Distinguish a user-cancelled task from a genuine failure so the
            // bar shows "cancelled" (grey) not "failed" (red).
            next.status = 'cancelled'
          }
          return next
        })
        this.pushTrajectory(y.turn_id, 'delegate_progress', `bg progress: ${ev.kind}`, {
          at: this.eventTs(y),
          detail: { kind: 'delegate_progress', taskId: y.task_id, event: ev },
        })
        this.emitNow()
        break
      }
      case 'delegate_complete': {
        // Mark the matching sub-agent done. Prefer task_id (carried by both
        // foreground + background delegate); fall back to the legacy
        // "most-recent still-active in this turn" heuristic when the server
        // omits task_id (older engine).
        const summary = {
          summary: y.summary.summary,
          keyFindings: y.summary.key_findings,
          budgetExceeded: y.summary.budget_exceeded,
          tokensUsed: y.summary.tokens_used,
          completed: y.summary.completed,
        }
        if (y.task_id) {
          // A cancel emits `DelegateProgress { Cancelled }` (→ 'cancelled') and
          // THEN this `DelegateComplete` (the sink's notify re-activates the
          // parent). Don't let the complete's `completed:false` downgrade an
          // already-cancelled card back to 'failed' — a user-cancelled task
          // stays 'cancelled' (grey), not 'failed' (red).
          this.state.backgroundTasks = this.state.backgroundTasks.map((t) =>
            t.taskId === y.task_id
              ? t.status === 'cancelled'
                ? { ...t, summary }
                : { ...t, status: y.summary.completed ? 'done' : 'failed', summary }
              : t,
          )
        }
        const idx = [...this.state.subagents]
          .reverse()
          .findIndex((s) => s.turnId === y.turn_id && s.status === 'active')
        if (idx >= 0) {
          const realIdx = this.state.subagents.length - 1 - idx
          const n = this.state.subagents[realIdx]
          this.state.subagents = this.state.subagents.map((s) =>
            s.id === n.id ? { ...s, status: 'done', summary } : s,
          )
        }
        this.pushTrajectory(y.turn_id, 'delegate_complete', 'delegate complete', {
          at: this.eventTs(y),
          detail: { kind: 'delegate_complete', taskId: y.task_id, summary },
        })
        this.emitNow()
        break
      }
      case 'working_state': {
        this.applyWorkingStateEvent(y.event)
        this.emitNow()
        break
      }
      case 'context_accounting': {
        // Context accounting is a capability-panel metric (context token
        // breakdown), not a timeline node — it carries no meaningful per-node
        // action, so it's folded into the usage snapshot only (issue #40).
        this.state.usage = { ...this.state.usage, context: y.accounting }
        this.emitNow()
        break
      }
      case 'token_usage': {
        this.state.usage = { ...this.state.usage, usage: y.usage }
        // Attribute the usage record to the active turn (TokenUsage carries no
        // turn_id). The latest record per turn wins; metrics aggregate the
        // per-turn latest at emit time.
        if (this.state.currentTurnId !== null) {
          this.state.turnUsage.set(this.state.currentTurnId, y.usage)
        }
        // Track the most recent usage record overall — the cache-hit % metric
        // reads the latest inference step's rate, not a cumulative average.
        this.state.latestUsage = y.usage
        // Issue #40: attribute the per-inference usage to the current
        // iteration node (latest record per iteration wins) instead of a
        // standalone session-scoped line. Resolve via `currentIterSeq` — the
        // `token_usage` event arrives AFTER the mid-stream `tool_calls`
        // finalizes `iterBuffer`, so the buffer can't locate the node.
        const iterSeq = this.currentIterSeq
        if (iterSeq !== null) {
          const entry = this.state.trajectory.find((e) => e.seq === iterSeq)
          if (entry?.detail && entry.detail.kind === 'iteration') {
            this.patchTrajectory(iterSeq, { detail: { ...entry.detail, usage: y.usage } })
          }
        }
        this.emitNow()
        break
      }
      case 'inference': {
        // Issue #40: attach the concrete API request/response + inference
        // latency to the current infer node. Ordering is handled by `pos`
        // (context < infer < tool), NOT by re-anchoring `at` — the streaming
        // path emits `tool_calls` mid-stream BEFORE `inference` fires, so a
        // wall-clock anchor would place infer AFTER tool.
        const iterSeq = this.currentIterSeq
        if (iterSeq !== null) {
          const entry = this.state.trajectory.find((e) => e.seq === iterSeq)
          if (entry?.detail && entry.detail.kind === 'iteration') {
            this.patchTrajectory(iterSeq, {
              detail: {
                ...entry.detail,
                inferenceDetail: y.snapshot,
                durationMs: y.snapshot.duration_ms,
              },
            })
          }
        }
        this.emitNow()
        break
      }
      case 'tools_added': {
        this.pushTrajectory(y.turn_id, 'tools_added', `+tools: ${y.names.join(', ')}`, {
          at: this.eventTs(y),
          detail: { kind: 'tools_added', names: y.names },
        })
        this.emitNow()
        break
      }
      case 'context_assembled': {
        // Issue #40: sectioned snapshot of the assembled context. Resolve
        // hash-deduped sections against the cache; store a context node. The
        // context sits to the LEFT of its infer node by construction: the
        // infer node is re-anchored to the inference-completion time when the
        // `inference` event arrives, so context (assembled first) < infer.
        const sections = this.resolveContextSections(y.sections)
        this.pushTrajectory(y.turn_id, 'context_assembled', `context assembled · iter ${y.iteration}`, {
          at: this.eventTs(y),
          detail: {
            kind: 'context',
            iteration: y.iteration,
            sections,
            durationMs: y.duration_ms,
          },
        })
        this.emitNow()
        break
      }
      case 'interrupted': {
        this.pushTrajectory(y.turn_id, 'interrupted', `interrupted: ${y.reason}`, {
          at: this.eventTs(y),
          detail: { kind: 'interrupted', reason: y.reason, point: y.point },
        })
        this.emitNow()
        break
      }
      case 'reflection': {
        this.pushTrajectory(y.turn_id, 'reflection', 'reflection', {
          at: this.eventTs(y),
          detail: { kind: 'reflection', summary: y.summary },
        })
        this.emitNow()
        break
      }
      default:
        // Unknown kind — ignore (the bus contract: new variants are
        // transparent to old frontends).
        break
    }
  }

  // ── RPC helpers (call site for the conversation domain) ─────────────────────

  /** Run a single-agent turn. `images` (W4 attachments) are appended as
   * `image` content blocks after the text — the engine's `turn/run` parses
   * `content: Vec<ContentBlock>` and forwards to `Directive::UserMessage`,
   * so image blocks flow to the provider inline. */
  async runTurn(text: string, images?: ContentBlock[]): Promise<void> {
    this.pushUserMessage(text, images)
    const content: ContentBlock[] = [{ type: 'text', text }, ...(images ?? [])]
    try {
      await this.rpc.call<TurnRunParams, { turn_id: string }>('turn/run', {
        content,
      })
    } catch (e) {
      // The turn/run ack failed before TurnStart — surface it. Stream chunks
      // (if any arrived) are already in the projection.
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
    }
  }

  async loadSession(id: string): Promise<void> {
    try {
      await this.rpc.call<{ id: string }, unknown>('session/load', { id })
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
    }
  }

  // ── W4 actions: per-message feedback ─────────────────────────────────────

  /** Record a 👍/👎/note against one assistant text node. Optimistic: the
   * node's `feedback` is set immediately and cleared/rolled-back on failure.
   *
   * The feedback target is the node's runtime `turn_id`. Feedback is therefore
   * only available on the **current live conversation** — reloaded/historical
   * messages replay without a turn_id (`Message` doesn't persist it), so the UI
   * hides the 👍/👎 buttons on them (see ChatView's `feedbackDone` gate). This
   * is by design: feedback is a reaction to a fresh model output, not a retro
   * tag on stored history. */
  async submitFeedback(
    nodeId: string,
    kind: FeedbackKind,
    text?: string,
  ): Promise<void> {
    const node = this.state.nodes.find((n) => n.id === nodeId)
    if (node === undefined) return
    const sessionId = this.state.sessionId
    const turnId = node.turnId
    if (sessionId === null || turnId === null) {
      // No live session / no runtime turn_id (a reloaded message) — the UI
      // gates this too, but guard so a stray call is a silent no-op, not an
      // engine error.
      return
    }
    const optimistic: FeedbackEntry = {
      id: `pending-${nodeId}`,
      session_id: sessionId,
      turn_id: turnId,
      message_role: 'assistant',
      kind,
      text,
      created_at_ms: Date.now(),
    }
    this.replaceNode(nodeId, { feedback: optimistic, feedbackPending: true })
    this.emitNow()
    try {
      await this.rpc.call<
        { session_id: string; turn_id: string; message_role: string; kind: string; text?: string },
        { ok: boolean }
      >('feedback/submit', {
        session_id: sessionId,
        turn_id: turnId,
        message_role: 'assistant',
        kind,
        text,
      })
      this.replaceNode(nodeId, { feedbackPending: false })
    } catch (e) {
      // Roll back the optimistic marker — the server didn't persist it.
      this.replaceNode(nodeId, { feedback: undefined, feedbackPending: false })
      this.state.lastError = e instanceof Error ? e.message : String(e)
    }
    this.emitNow()
  }

  // ── B5 actions: durable host allow/deny list ─────────────────────────────

  /** Admit `host` persistently ("always") via `host/allow`. Best-effort: a
   * network failure (app-server offline) is swallowed so the caller can still
   * fall back to a session-only `Proceed` — the host simply won't persist
   * across restarts. The engine `NetworkProxy` consults the same durable
   * `~/.oneai/oneai.db` table on its next CONNECT, so the admitted host no
   * longer re-prompts. */
  async admitHost(host: string): Promise<void> {
    try {
      await this.rpc.call<{ host: string }, { ok: boolean }>('host/allow', { host })
    } catch (e) {
      // Swallow — the caller (ApprovalPanel "Always") still proceeds.
      this.state.lastError = e instanceof Error ? e.message : String(e)
    }
  }

  /** Deny `host` persistently via `host/deny` — future tunnel attempts are
   * blocked without re-prompting. Best-effort like `admitHost`. */
  async denyHost(host: string): Promise<void> {
    try {
      await this.rpc.call<{ host: string }, { ok: boolean }>('host/deny', { host })
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
    }
  }

  /** Both lists in one round-trip (the `host/list` result shape). Returns
   * `{allowed:[],denied:[]}` on failure (the Settings panel shows empty). */
  async listHosts(): Promise<HostListResult> {
    try {
      return await this.rpc.call<Record<string, never>, HostListResult>('host/list', {} as Record<string, never>)
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      return { allowed: [], denied: [] }
    }
  }

  /** Revoke an admission (delete from the allowlist). */
  async removeHost(host: string): Promise<void> {
    try {
      await this.rpc.call<{ host: string }, { ok: boolean }>('host/remove', { host })
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
    }
  }

  /** Revoke a denial (delete from the denylist). */
  async removeDeniedHost(host: string): Promise<void> {
    try {
      await this.rpc.call<{ host: string }, { ok: boolean }>('host/remove-denied', { host })
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
    }
  }

  // ── Background sub-agent task control (Phase 2A gap-1) ───────────────────

  /** Cancel one in-flight background sub-agent by `task_id`. Optimistically
   * flips the card to "cancelled" so the bar reacts instantly; the engine's
   * `DelegateProgress { Cancelled }` confirms it (and is what flips it for a
   * cross-turn task the UI never polled). On RPC failure roll back to the
   * prior status + surface the error — the engine is the authority. */
  async cancelBackgroundTask(taskId: string): Promise<boolean> {
    const prev = this.state.backgroundTasks.find((t) => t.taskId === taskId)?.status
    if (prev === undefined) return false
    this.state.backgroundTasks = this.state.backgroundTasks.map((t) =>
      t.taskId === taskId ? { ...t, status: 'cancelled' } : t,
    )
    this.emitNow()
    try {
      const res = await this.rpc.call<{ task_id: string }, BackgroundTaskOpResult>(
        'background/cancel',
        { task_id: taskId },
      )
      // The server may report the task wasn't found (already finished, or a
      // stale id) — roll back to the prior status so the bar reflects reality.
      if (!res.ok) {
        this.state.backgroundTasks = this.state.backgroundTasks.map((t) =>
          t.taskId === taskId ? { ...t, status: prev } : t,
        )
        this.state.lastError = res.error ?? 'background/cancel failed'
        this.emitNow()
        return false
      }
      return true
    } catch (e) {
      this.state.backgroundTasks = this.state.backgroundTasks.map((t) =>
        t.taskId === taskId ? { ...t, status: prev } : t,
      )
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
      return false
    }
  }

  /** Cancel all in-flight background sub-agents via `background/cancel_all`.
   * Optimistically flips every active card to "cancelled". */
  async cancelAllBackground(): Promise<boolean> {
    const prevs = new Map(
      this.state.backgroundTasks
        .filter((t) => t.status === 'active')
        .map((t) => [t.taskId, t.status] as const),
    )
    if (prevs.size === 0) return false
    this.state.backgroundTasks = this.state.backgroundTasks.map((t) =>
      t.status === 'active' ? { ...t, status: 'cancelled' } : t,
    )
    this.emitNow()
    try {
      const res = await this.rpc.call<Record<string, never>, BackgroundTaskOpResult>(
        'background/cancel_all',
        {} as Record<string, never>,
      )
      if (!res.ok) {
        this.state.backgroundTasks = this.state.backgroundTasks.map((t) =>
          prevs.has(t.taskId) ? { ...t, status: 'active' } : t,
        )
        this.state.lastError = res.error ?? 'background/cancel_all failed'
        this.emitNow()
        return false
      }
      return true
    } catch (e) {
      this.state.backgroundTasks = this.state.backgroundTasks.map((t) =>
        prevs.has(t.taskId) ? { ...t, status: 'active' } : t,
      )
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
      return false
    }
  }

  // ── W2 actions: approval / paradigm / plan-mode / cancel / details ────────

  /** Respond to a queued approval and promote the next (issue #20 queue). */
  async respondApproval(
    requestId: string,
    response: InteractionResponse,
  ): Promise<void> {
    // Optimistically drop the item from the queue so the next is promoted
    // immediately. Each approval carries a unique request_id; a Revise
    // re-plan emits a fresh approval_request (new id) rather than reusing.
    this.state.approvalQueue = this.state.approvalQueue.filter(
      (a) => a.request_id !== requestId,
    )
    this.emitNow()
    try {
      await this.rpc.call<ApprovalRespondParams, { ok: boolean }>(
        'approval/respond',
        { request_id: requestId, response },
      )
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
    }
  }

  /** Cancel the in-flight turn (Stop button). */
  async cancelTurn(): Promise<void> {
    try {
      await this.rpc.call<{ reason?: unknown }, { ok: boolean }>('turn/cancel', {})
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
    }
  }

  /** Switch the active paradigm (plan/reflect/explore/re_act). */
  async switchParadigm(to: BusParadigmKind): Promise<void> {
    this.state.paradigm = to
    this.emitNow()
    try {
      await this.rpc.call<ParadigmSwitchParams, { ok: boolean }>(
        'paradigm/switch',
        { to },
      )
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
    }
  }

  /** Toggle plan mode on/off (config/update{plan_mode}). */
  async setPlanMode(on: boolean): Promise<void> {
    try {
      await this.rpc.call<ConfigUpdateParams, { ok: boolean }>('config/update', {
        plan_mode: on,
      })
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
    }
  }

  /** Set auto-accept mode (silently Proceed on tool approvals). Purely
   *  frontend — no engine config; the projection short-circuits the approval
   *  bar for ToolApproval requests while this is on. */
  setAutoApprove(on: boolean): void {
    this.state.autoApprove = on
    this.emitNow()
  }

  /** Select a tool node for the details rail (opens the rail via App). */
  selectTool(nodeId: string | null): void {
    this.state.selectedToolNodeId = nodeId
    this.emitNow()
  }

  clearSelection(): void {
    this.state.selectedToolNodeId = null
    this.emitNow()
  }

  /** Compact the conversation (keep_recent_turns). */
  async compact(keepRecentTurns: number): Promise<void> {
    try {
      await this.rpc.call<{ keep_recent_turns: number }, { ok: boolean }>(
        'conversation/compact',
        { keep_recent_turns: keepRecentTurns },
      )
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
    }
  }

  /** Clear the current session. */
  async clearSession(): Promise<void> {
    try {
      await this.rpc.call<unknown, unknown>('session/clear', {})
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.emitNow()
    }
  }

  /** Generate a project-instruction file (`/init` — TUI parity). Blocking-ack
   * RPC: resolves with the engine's `InitResult.message` once the probe +
   * (optional) LLM synthesis finishes. Unlike the swallow-and-banner actions
   * above, this THROWS on failure so the slash handler can surface the error
   * as an in-conversation note (the TUI prints `/init` failures inline too). */
  async projectInit(
    format?: string,
    force = false,
    noLlm = false,
  ): Promise<string> {
    const res = await this.rpc.call<
      { format?: string; force?: boolean; no_llm?: boolean },
      { message?: string }
    >('project/init', { format, force, no_llm: noLlm })
    return res?.message ?? ''
  }

  // ── W3 actions: scenario group chat ───────────────────────────────────────

  /** Leave scenario mode (return to single-agent). Called on New chat /
   *  loading a saved session so a stale scenario doesn't tag the next
   *  single-agent turn's bubbles. */
  exitScenario(): void {
    this.state.currentScenario = null
    this.state.debriefActive = false
    this.state.currentSpeaker = null
    this.emitNow()
  }

  /**
   * Start a multi-agent scenario. Compiles the rich `BusScenario` + collected
   * topic values into the engine launch payload (baking the visible topic
   * background into each member's system prompt), submits `group/start`, then
   * either runs the opener turn (`group/open`) or kicks the first round with
   * a user message built from the topic values (`group/run`). Streaming +
   * the round-end `turn_complete` arrive as `event` notifications, routed by
   * the `speaker` field on each fragment. Mirrors macOS `newConversation`.
   */
  async startScenario(
    scenario: BusScenario,
    values: Record<string, string>,
    locale: BusLocale,
  ): Promise<void> {
    this.stashMetrics(this.state.sessionId)
    this.state.currentScenario = scenario
    this.state.debriefActive = false
    this.state.currentSpeaker = null
    this.state.currentTurnId = null
    this.state.nodes = []
    this.state.lastError = null
    this.state.sessionId = null // group-chat conversation id is engine-side
    this.state.turnActive = true
    this.resetLedgers()
    this.restoreBaseline(null)
    this.emitNow()
    const spec = compileGroupScenario(scenario, values, locale)
    try {
      await this.rpc.call<GroupStartParams, { ok: boolean }>('group/start', {
        scenario: spec,
      })
      if (scenario.opener_agent_id !== undefined && scenario.opener_agent_id !== null) {
        // Opener speaks first (it knows the topic from its system prompt).
        await this.rpc.call<unknown, { ok: boolean }>('group/open', {})
      } else {
        // No opener — kick off the first round with a user message built
        // from the collected topic values (e.g. writing workshop).
        const firstMsg = firstUserMessage(scenario, values)
        if (firstMsg.length > 0) {
          await this.groupRun(firstMsg, firstMsg)
        } else {
          this.state.turnActive = false
          this.emitNow()
        }
      }
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.state.turnActive = false
      this.emitNow()
    }
  }

  /** Append the user's message and run the round's speakers (`group/run`).
   *  `display` is the bubble text shown locally (defaults to the input — for
   *  the debrief it's a friendly marker, while the engine receives the
   *  summary prompt). */
  async groupRun(userInput: string, display?: string): Promise<void> {
    this.pushUserMessage(display ?? userInput)
    // Optimistically mark the round in flight so Stop/TypingDots show even if
    // the engine doesn't emit a fresh turn_start for the group round (group
    // turn_complete fires per round; turn_start is single-agent-eager).
    this.state.turnActive = true
    this.emitNow()
    try {
      await this.rpc.call<GroupRunParams, { ok: boolean }>('group/run', {
        user_input: userInput,
      })
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.state.turnActive = false
      this.emitNow()
    }
  }

  /** Trigger the scenario's debrief phase: narrow the turn policy to the
   *  debrief member (`group/set_order`), then send the summary prompt
   *  (`group/run`) for a single-member summary. Subsequent user messages route
   *  only to that member. Mirrors macOS `endScenarioDebrief`. */
  async debrief(markerLabel: string): Promise<void> {
    const sc = this.state.currentScenario
    if (sc === null || sc.debrief === undefined || this.state.debriefActive) return
    if (this.state.turnActive) return
    this.state.debriefActive = true
    this.emitNow()
    try {
      await this.rpc.call<GroupSetOrderParams, { ok: boolean }>(
        'group/set_order',
        { order: [sc.debrief.debrief_member_id] },
      )
      await this.groupRun(sc.debrief.summary_prompt, markerLabel)
    } catch (e) {
      this.state.lastError = e instanceof Error ? e.message : String(e)
      this.state.debriefActive = false
      this.emitNow()
    }
  }

  /** Send a user message in whichever mode is active: group (`group/run`)
   *  when a scenario is running, single-agent (`turn/run`) otherwise. The
   *  App routes all composer sends through this so the mode switch is
   *  transparent to the UI. `images` (W4 attachments) only flow in
   *  single-agent mode — group chat's `GroupUserMessage` takes plain text, so
   *  the UI disables the attachment rail when a scenario is active. */
  async sendMessage(text: string, images?: ContentBlock[]): Promise<void> {
    if (this.state.currentScenario !== null) {
      await this.groupRun(text)
    } else {
      await this.runTurn(text, images)
    }
  }
}

// ─── oneai-core::Message → ChatNode ─────────────────────────────────────────
// `Message = { role: "user"|"assistant"|"system"|"tool", content: ContentBlock[] }`.
// Only text/thinking blocks are rendered at W1; image/file/tool blocks are
// flattened to a placeholder until the full tool-rendering domain lands (W2).
function messagesToNodes(messages: unknown[]): ChatNode[] {
  const out: ChatNode[] = []
  for (const m of messages) {
    const msg = m as { role?: string; content?: ContentBlock[] } | null
    if (msg === null || typeof msg !== 'object') continue
    // Don't replay the system prompt or tool-result messages as chat bubbles —
    // the system prompt is engine context, not conversation, and tool results
    // get their own rendering in W2.
    if (msg.role === 'system' || msg.role === 'tool') continue
    const role = roleOf(msg.role)
    // W4: collect image blocks for a user node's attachment thumbnails.
    const attachments: ContentBlock[] = (msg.content ?? []).filter(
      (b) => b.type === 'image' || b.type === 'file',
    )
    for (const block of msg.content ?? []) {
      if (block.type === 'text') {
        out.push({
          id: nextNodeId(),
          role,
          kind: role === 'assistant' ? 'text' : role === 'user' ? 'user' : 'text',
          text: block.text,
          speaker: null,
          turnId: null,
          state: 'done',
          attachments: role === 'user' && attachments.length > 0 ? attachments : undefined,
        })
      } else if (block.type === 'thinking') {
        out.push({
          id: nextNodeId(),
          role: 'assistant',
          kind: 'thinking',
          text: block.text,
          speaker: null,
          turnId: null,
          state: 'done',
        })
      }
    }
  }
  return out
}

function roleOf(r: string | undefined): NodeRole {
  if (r === 'user') return 'user'
  if (r === 'assistant') return 'assistant'
  return 'system'
}

// ─── React hook ──────────────────────────────────────────────────────────────

export function useProjection(store: ProjectionStore): ProjectionSnapshot {
  return useSyncExternalStore(store.subscribe, store.getSnapshot, () => EMPTY)
}

// ─── Session list store (sidebar) — synchronous CRUD, no streaming ──────────

export class SessionListStore {
  private rpc: OneAiRpcClient
  private sessions: SessionInfo[] = []
  private listeners = new Set<() => void>()

  constructor(rpc: OneAiRpcClient) {
    this.rpc = rpc
  }

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn)
    return () => this.listeners.delete(fn)
  }
  getSnapshot = (): SessionInfo[] => this.sessions

  async refresh(): Promise<void> {
    try {
      const res = await this.rpc.call<unknown, { sessions: SessionInfo[] }>(
        'session/list',
        {},
      )
      this.sessions = res.sessions
      for (const l of this.listeners) l()
    } catch {
      /* offline — keep the last list */
    }
  }
}

export function useSessionList(store: SessionListStore): SessionInfo[] {
  return useSyncExternalStore(
    store.subscribe,
    store.getSnapshot,
    () => [],
  )
}
