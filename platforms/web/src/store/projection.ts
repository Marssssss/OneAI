import { useSyncExternalStore } from 'react'
import type { OneAiRpcClient } from '../rpc/client'
import type {
  ContentBlock,
  EngineYield,
  SessionInfo,
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
export type NodeKind = 'user' | 'text' | 'thinking' | 'error'
export type NodeState = 'streaming' | 'done' | 'error'

export interface ChatNode {
  /** Stable per-node key — React list reconciliation uses this. */
  id: string
  role: NodeRole
  kind: NodeKind
  text: string
  speaker: string | null
  turnId: string | null
  state: NodeState
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
}

interface WorkingState {
  sessionId: string | null
  nodes: ChatNode[]
  turnActive: boolean
  lastError: string | null
  currentTurnId: string | null
  currentSpeaker: string | null
}

const EMPTY: ProjectionSnapshot = {
  version: 0,
  sessionId: null,
  nodes: [],
  turnActive: false,
  lastError: null,
}

let nodeSeq = 0
function nextNodeId(): string {
  nodeSeq += 1
  return `n${nodeSeq}`
}

export class ProjectionStore {
  private rpc: OneAiRpcClient
  private state: WorkingState = {
    sessionId: null,
    nodes: [],
    turnActive: false,
    lastError: null,
    currentTurnId: null,
    currentSpeaker: null,
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
    this.snapshot = {
      version: this.snapshot.version + 1,
      sessionId: this.state.sessionId,
      // Shallow-copy the nodes array so React sees a new reference; the node
      // objects themselves are replaced (not mutated) on each edit, so a
      // shallow copy is enough for memoized row components to diff.
      nodes: [...this.state.nodes],
      turnActive: this.state.turnActive,
      lastError: this.state.lastError,
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
