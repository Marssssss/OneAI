import { useSyncExternalStore } from 'react'
import type { OneAiRpcClient } from '../rpc/client'
import type {
  ApprovalRespondParams,
  BusParadigmKind,
  ConfigUpdateParams,
  ContentBlock,
  EngineYield,
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
    }
    for (const l of this.listeners) l()
  }

  /** Flush now (bypass the coalescer) — for non-hot mutations. */
  private emitNow(): void {
    this.coalescer.flushNow()
  }

  // ── user actions ────────────────────────────────────────────────────────────

  /** Optimistically append a user node before the engine confirms the turn. */
  pushUserMessage(text: string): void {
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
        // Seed the assistant node eagerly so the user sees an empty bubble
        // the moment the turn starts (before any stream_chunk lands).
        this.seedAssistant(y.turn_id, this.state.currentSpeaker)
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
        this.emitNow()
        break
      }
      case 'approval_request': {
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
        this.state.nodes = this.state.nodes.map((n) =>
          n.turnId === y.turn_id && n.state === 'streaming'
            ? { ...n, state: 'done' as NodeState }
            : n,
        )
        this.state.turnActive = false
        this.state.currentSpeaker = null
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
        this.emitNow()
        break
      }
      case 'session_loaded': {
        this.state.sessionId = y.id
        this.state.nodes = messagesToNodes(y.messages)
        this.emitNow()
        break
      }
      case 'session_cleared':
      case 'session_deleted': {
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

  async runTurn(text: string): Promise<void> {
    this.pushUserMessage(text)
    const content: ContentBlock[] = [{ type: 'text', text }]
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
