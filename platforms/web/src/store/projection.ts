import { useSyncExternalStore } from 'react'
import type { OneAiRpcClient } from '../rpc/client'
import type {
  ApprovalRespondParams,
  Artifact,
  BusParadigmKind,
  BusScenario,
  BusScenarioMember,
  BusLocale,
  ConfigUpdateParams,
  ContentBlock,
  EngineYield,
  FeedbackEntry,
  FeedbackKind,
  GroupRunParams,
  GroupSetOrderParams,
  GroupStartParams,
  InteractionRequest,
  InteractionResponse,
  ParadigmSwitchParams,
  PlanState,
  PlanStep,
  SessionInfo,
  ToolOutput,
  TurnRunParams,
} from '../rpc/types'
import { StreamCoalescer } from '../stream/coalescer'
import { compileGroupScenario, firstUserMessage } from '../scenario/compile'
import type {
  SubagentNode,
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
  /** Latest usage snapshot (token usage + context accounting). */
  usage: UsageSnapshot
  /** Performance.now() marks per turn, for timing-overview rendering. */
  turnTimings: { turnId: string; startedAt: number | null; endedAt: number | null; iterations: number }[]
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
  usage: UsageSnapshot
  /** per-turn performance.now() start marks (for elapsed-ms timing). */
  turnStartPerf: Map<string, number>
  turnEndPerf: Map<string, number>
  turnIterations: Map<string, number>
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
  usage: { usage: null, context: null },
  turnTimings: [],
}

let nodeSeq = 0
function nextNodeId(): string {
  nodeSeq += 1
  return `n${nodeSeq}`
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
    usage: { usage: null, context: null },
    turnStartPerf: new Map(),
    turnEndPerf: new Map(),
    turnIterations: new Map(),
  }
  private snapshot: ProjectionSnapshot = EMPTY
  private listeners = new Set<() => void>()
  private coalescer: StreamCoalescer

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
    const turnTimings = Array.from(this.state.turnStartPerf.keys()).map((turnId) => ({
      turnId,
      startedAt: this.state.turnStartPerf.get(turnId) ?? null,
      endedAt: this.state.turnEndPerf.get(turnId) ?? null,
      iterations: this.state.turnIterations.get(turnId) ?? 0,
    }))
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
      usage: this.state.usage,
      turnTimings,
    }
    for (const l of this.listeners) l()
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
        this.pushTrajectory(curTurn, 'working_state', `goal: ${ev.goal}`, { detail: ev })
        break
      case 'step_added':
        w.steps = [...w.steps, ev.step]
        this.pushTrajectory(curTurn, 'working_state', `step added: ${ev.step.description}`, { detail: ev })
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
        this.pushTrajectory(curTurn, 'working_state', `step ${ev.step_id} → ${ev.status}`, { detail: ev })
        break
      }
      case 'decision_made':
        w.decisions = [...w.decisions, ev.decision]
        this.pushTrajectory(curTurn, 'working_state', `decision: ${ev.decision.chosen}`, { detail: ev })
        break
      case 'blocker_raised':
        w.blockers = [...w.blockers, ev.blocker]
        this.pushTrajectory(curTurn, 'working_state', `blocker: ${ev.blocker.description}`, { detail: ev })
        break
      case 'blocker_resolved':
        w.blockers = w.blockers.map((b) =>
          b.id === ev.blocker_id ? { ...b, status: 'resolved', resolution: ev.resolution } : b,
        )
        this.pushTrajectory(curTurn, 'working_state', `blocker resolved: ${ev.blocker_id}`, { detail: ev })
        break
      case 'note_added':
        w.notes = [...w.notes, ev.note]
        this.pushTrajectory(curTurn, 'working_state', `note: ${ev.note.content.slice(0, 60)}`, { detail: ev })
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
        this.pushTrajectory(curTurn, 'working_state', 'working-state snapshot', { detail: ev })
        break
      }
      case 'task_status':
      case 'reflection_fired':
        this.pushTrajectory(curTurn, 'working_state', ev.kind === 'reflection_fired' ? 'reflection fired' : 'task status', { detail: ev })
        break
      default:
        // Unknown TaskEventPayload kind — ignore (non_exhaustive contract).
        break
    }
    this.state.working = { ...this.state.working }
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
        ...extra,
      },
    ]
  }

  /** Reset the session-scoped ledgers (new session / scenario start). */
  private resetLedgers(): void {
    this.state.trajectory = []
    this.state.working = { ...EMPTY_WORKING }
    this.state.subagents = []
    this.state.usage = { usage: null, context: null }
    this.state.turnStartPerf.clear()
    this.state.turnEndPerf.clear()
    this.state.turnIterations.clear()
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

  // ── EngineYield consumer ───────────────────────────────────────────────────

  private consume(y: EngineYield): void {
    switch (y.kind) {
      case 'turn_start': {
        this.state.currentTurnId = y.turn_id
        this.state.turnActive = true
        this.state.turnStartPerf.set(y.turn_id, performance.now())
        this.state.turnEndPerf.delete(y.turn_id)
        this.state.turnIterations.set(y.turn_id, 0)
        // Single-agent: seed an empty bubble eagerly so the user sees progress
        // the moment the turn starts. Group chat: defer to `speaker_turn` /
        // `stream_chunk` (which carry the member id) to avoid a stale empty
        // placeholder node before the first speaker is known.
        if (this.state.currentScenario === null) {
          this.seedAssistant(y.turn_id, this.state.currentSpeaker)
        }
        this.pushTrajectory(y.turn_id, 'turn_start', y.task.length > 0 ? y.task : 'turn start')
        this.emitNow()
        break
      }
      case 'speaker_turn': {
        // Round-level boundary in a group turn: finalize the previous
        // speaker's streaming text node for this turn (so its bubble flips to
        // done as the round progresses, not all-at-once at turn_complete) and
        // record the new speaker so the next stream_chunk seeds a fresh node.
        this.finalizeStreamingText(y.turn_id)
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
        this.coalescer.request()
        break
      }
      case 'direct_answer': {
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
        this.emitNow()
        break
      }
      case 'tool_calls': {
        // Finalize any streaming text node for this turn so tool cards
        // interleave between finalized text blocks (the next stream_chunk
        // seeds a fresh text node after the cards).
        this.finalizeStreamingText(y.turn_id)
        for (const c of y.calls) {
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
            detail: { callId: c.id, args: c.args },
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
        this.emitNow()
        break
      }
      case 'paradigm_switch': {
        this.state.paradigm = y.to
        this.pushTrajectory(y.turn_id, 'paradigm_switch', `paradigm: ${y.from} → ${y.to}`, {
          paradigm: y.to,
          detail: { from: y.from, to: y.to },
        })
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
            detail: { steps: plan.steps.length, revision: plan.revision },
          })
        } else {
          this.pushTrajectory(y.turn_id, 'plan_revision', 'plan cleared')
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
        this.emitNow()
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
        this.pushTrajectory(y.turn_id, 'turn_complete', 'turn complete')
        this.emitNow()
        break
      }
      case 'error': {
        this.state.lastError = y.message
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
        this.emitNow()
        break
      }
      case 'session_created': {
        this.state.sessionId = y.id
        this.resetLedgers()
        this.emitNow()
        break
      }
      case 'session_loaded': {
        // Loading a saved single-agent session leaves any active scenario —
        // a stale scenario would tag the next single-agent turn's bubbles.
        this.state.currentScenario = null
        this.state.debriefActive = false
        this.state.currentSpeaker = null
        this.state.sessionId = y.id
        this.state.nodes = messagesToNodes(y.messages)
        this.resetLedgers()
        // W4 B4 — backfill per-message 👍/👎 markers for this session so a
        // reloaded conversation restores its reaction state. Fire-and-forget:
        // handleYield is sync; the backfill lands as its own emit when ready.
        void this.loadFeedback(y.id)
        this.emitNow()
        break
      }
      case 'session_cleared':
      case 'session_deleted': {
        this.emitNow()
        break
      }
      // ── W4: events the W1–W3 projection previously dropped into `default`.
      //     These feed the trajectory ledger + the capability panel (goal bar,
      //     subagent tree, context/usage overview). All pure consumers. ──
      case 'iteration_start': {
        const prev = this.state.turnIterations.get(y.turn_id) ?? 0
        this.state.turnIterations.set(y.turn_id, prev + 1)
        this.pushTrajectory(y.turn_id, 'iteration_start', `iteration ${y.iteration} · ${y.paradigm}`, {
          iter: y.iteration,
          paradigm: y.paradigm,
          detail: { iteration: y.iteration, paradigm: y.paradigm },
        })
        this.emitNow()
        break
      }
      case 'delegate': {
        const id = nextSubagentId(y.turn_id)
        const kindLabel = typeof y.agent_kind === 'string' ? y.agent_kind : `Custom:${y.agent_kind.Custom}`
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
        this.pushTrajectory(y.turn_id, 'delegate', `delegate → ${kindLabel}`, {
          detail: { subagentId: id, task: y.task },
        })
        this.emitNow()
        break
      }
      case 'delegate_complete': {
        // Mark the last active subagent for this turn done + attach summary.
        // (delegate carries no call id; the matching is by recency within the
        // turn — the most recent still-active delegate completes first.)
        const idx = [...this.state.subagents]
          .reverse()
          .findIndex((s) => s.turnId === y.turn_id && s.status === 'active')
        if (idx >= 0) {
          const realIdx = this.state.subagents.length - 1 - idx
          const n = this.state.subagents[realIdx]
          this.state.subagents = this.state.subagents.map((s) =>
            s.id === n.id
              ? {
                  ...s,
                  status: 'done',
                  summary: {
                    summary: y.summary.summary,
                    keyFindings: y.summary.key_findings,
                    budgetExceeded: y.summary.budget_exceeded,
                    tokensUsed: y.summary.tokens_used,
                    completed: y.summary.completed,
                  },
                }
              : s,
          )
        }
        this.pushTrajectory(y.turn_id, 'delegate_complete', 'delegate complete', {
          detail: { summary: y.summary.summary, keyFindings: y.summary.key_findings },
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
        this.state.usage = { ...this.state.usage, context: y.accounting }
        this.pushTrajectory(y.turn_id, 'context_accounting', 'context accounting', {
          detail: y.accounting,
        })
        this.emitNow()
        break
      }
      case 'token_usage': {
        this.state.usage = { ...this.state.usage, usage: y.usage }
        const total = y.usage.prompt_tokens + y.usage.completion_tokens
        this.pushTrajectory(null, 'token_usage', `usage: ${total} tokens`, {
          detail: y.usage,
        })
        this.emitNow()
        break
      }
      case 'tools_added': {
        this.pushTrajectory(y.turn_id, 'tools_added', `+tools: ${y.names.join(', ')}`, {
          detail: { names: y.names },
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
   * `nodeId` resolves to the node's `turnId` (the feedback target). */
  async submitFeedback(
    nodeId: string,
    kind: FeedbackKind,
    text?: string,
  ): Promise<void> {
    const node = this.state.nodes.find((n) => n.id === nodeId)
    if (node === null || node === undefined) return
    const sessionId = this.state.sessionId
    const turnId = node.turnId
    if (sessionId === null || turnId === null) {
      this.state.lastError = 'feedback needs an active session + a finalized turn'
      this.emitNow()
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

  /** Backfill per-message feedback markers for a session — called after a
   * session loads so reloaded assistant bubbles show their 👍/👎 state. */
  async loadFeedback(sessionId: string): Promise<void> {
    try {
      const res = await this.rpc.call<{ session_id: string }, { feedback: FeedbackEntry[] }>(
        'feedback/list',
        { session_id: sessionId },
      )
      const byTurn = new Map<string, FeedbackEntry>()
      for (const e of res.feedback) {
        // Last-write-wins on turn_id — the most recent reaction wins.
        byTurn.set(e.turn_id, e)
      }
      if (byTurn.size === 0) return
      let changed = false
      this.state.nodes = this.state.nodes.map((n) => {
        if (n.role === 'assistant' && n.kind === 'text' && n.turnId !== null) {
          const fb = byTurn.get(n.turnId)
          if (fb !== undefined) {
            changed = true
            return { ...n, feedback: fb }
          }
        }
        return n
      })
      if (changed) this.emitNow()
    } catch {
      // Best-effort: a failing feedback/list must not break session load.
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
    this.state.currentScenario = scenario
    this.state.debriefActive = false
    this.state.currentSpeaker = null
    this.state.currentTurnId = null
    this.state.nodes = []
    this.state.lastError = null
    this.state.sessionId = null // group-chat conversation id is engine-side
    this.state.turnActive = true
    this.resetLedgers()
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
