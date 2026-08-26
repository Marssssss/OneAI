// Timeline model builder (issue #40) — a pure function that folds the flat
// trajectory ledger into swim-lanes + nodes + fork/join edges + turn boundaries
// the SVG canvas renders. No React, no DOM — trivially unit-testable.

import type { TrajectoryDetail, TrajectoryEntry } from '../store/trajectory'

export interface TimelineNode {
  seq: number
  lane: number
  kind: TrajectoryEntry['kind']
  title: string
  /** Logical position (0-based sorted rank) — the x coordinate source. Nodes
   *  are ordered semantically (context < infer < tool per iteration) then
   *  spaced uniformly, so long wall-clock gaps don't make the lane sparse. */
  index: number
  /** Span (ms) for nodes with a duration (tool calls); 0 for instants. */
  durationMs: number
  entry: TrajectoryEntry
}

export interface TimelineLane {
  id: string
  label: string
  depth: number
}

export type TimelineEdgeKind = 'fork' | 'join' | 'depends'
export interface TimelineEdge {
  fromSeq: number
  toSeq: number
  kind: TimelineEdgeKind
}

/** A turn's start/end span, drawn as a lane marker (not a node). */
export interface TurnBoundary {
  turnId: string
  /** The turn's task (from `turn_start`), used as a marker label. */
  label: string
  /** Sorted index of the turn's first node. */
  startIndex: number
  /** Sorted index of the turn's last node; null while the turn is still in
   *  flight (no `turn_complete` seen). */
  endIndex: number | null
}

export interface TimelineModel {
  lanes: TimelineLane[]
  nodes: TimelineNode[]
  edges: TimelineEdge[]
  turns: TurnBoundary[]
  /** Total node count (drives fit-to-width; 0 when no nodes). */
  count: number
}

const MAIN_LANE = 0

/**
 * Build the timeline. Lanes: `0` = main agent, `1..` = one per delegated
 * task id (in first-seen order). Child-lane nodes are the `delegate_progress`
 * entries (forwarded sub-agent iteration/tool/usage events); `delegate` /
 * `delegate_complete` stay on the main lane as the fork/join anchors.
 *
 * Turn boundaries (`turn_start` / `turn_complete`) are NOT nodes — they are
 * folded into `turns` for a lane marker (issue #40 follow-up).
 */
export function buildTimeline(entries: TrajectoryEntry[]): TimelineModel {
  const lanes: TimelineLane[] = [{ id: 'main', label: 'main', depth: 0 }]
  const taskLane = new Map<string, number>()

  const laneForTask = (taskId: string): number => {
    const existing = taskLane.get(taskId)
    if (existing !== undefined) return existing
    const idx = lanes.length
    taskLane.set(taskId, idx)
    lanes.push({ id: `task:${taskId}`, label: taskId, depth: 1 })
    return idx
  }

  // First pass: split turn-boundary entries from node entries, collect turns.
  const nodeEntries: TrajectoryEntry[] = []
  const turnStarts = new Map<string, { turnId: string; label: string }>()
  const turnEnds = new Set<string>()

  for (const e of entries) {
    if (e.kind === 'turn_start') {
      const d = e.detail as Extract<TrajectoryDetail, { kind: 'turn' }> | undefined
      const id = e.turnId ?? ''
      turnStarts.set(id, { turnId: id, label: d?.task ?? '' })
      continue
    }
    if (e.kind === 'turn_complete') {
      turnEnds.add(e.turnId ?? '')
      continue
    }
    nodeEntries.push(e)
  }

  // Second pass: order nodes by semantic position (`pos`, falling back to
  // arrival `seq` for older replayed logs), then assign a uniform logical
  // index + lane + duration. Wall-clock `at` is deliberately NOT the axis —
  // it would scatter nodes with long gaps and invert context/infer/tool.
  const ordered = [...nodeEntries].sort(
    (a, b) => (a.pos ?? a.seq) - (b.pos ?? b.seq) || a.seq - b.seq,
  )
  const nodes: TimelineNode[] = ordered.map((e, index) => {
    let lane = MAIN_LANE
    let durationMs = 0
    const d = e.detail
    if (d) {
      if (d.kind === 'delegate_progress') lane = laneForTask(d.taskId)
      if (d.kind === 'tool' && typeof d.durationMs === 'number') durationMs = d.durationMs
    }
    return { seq: e.seq, lane, kind: e.kind, title: e.title, index, durationMs, entry: e }
  })

  // Third pass: fork/join/depends edges.
  const edges: TimelineEdge[] = []
  const delegateNodeByTask = new Map<string, TimelineNode>()
  const childNodesByTask = new Map<string, TimelineNode[]>()
  const completeNodeByTask = new Map<string, TimelineNode>()

  for (const n of nodes) {
    const d = n.entry.detail
    if (!d) continue
    if (d.kind === 'delegate') delegateNodeByTask.set(d.taskId, n)
    else if (d.kind === 'delegate_progress') {
      const arr = childNodesByTask.get(d.taskId) ?? []
      arr.push(n)
      childNodesByTask.set(d.taskId, arr)
    } else if (d.kind === 'delegate_complete') completeNodeByTask.set(d.taskId, n)
  }

  for (const [taskId, fork] of delegateNodeByTask) {
    const children = childNodesByTask.get(taskId) ?? []
    const join = completeNodeByTask.get(taskId)
    const target = children[0] ?? join
    if (target) edges.push({ fromSeq: fork.seq, toSeq: target.seq, kind: 'fork' })
    if (join && children.length > 0) {
      edges.push({ fromSeq: children[children.length - 1].seq, toSeq: join.seq, kind: 'join' })
    }
    // depends_on: a serial edge from the dependency's fork to this fork.
    const fd = fork.entry.detail as Extract<TrajectoryDetail, { kind: 'delegate' }>
    for (const dep of fd.dependsOn) {
      const depFork = delegateNodeByTask.get(dep)
      if (depFork) edges.push({ fromSeq: depFork.seq, toSeq: fork.seq, kind: 'depends' })
    }
  }

  // Turn boundaries map to the first/last node index of each turn.
  const firstIndexByTurn = new Map<string, number>()
  const lastIndexByTurn = new Map<string, number>()
  for (const n of nodes) {
    const id = n.entry.turnId ?? ''
    if (!firstIndexByTurn.has(id)) firstIndexByTurn.set(id, n.index)
    lastIndexByTurn.set(id, n.index)
  }
  const turns: TurnBoundary[] = Array.from(turnStarts.values()).map((s) => ({
    turnId: s.turnId,
    label: s.label,
    startIndex: firstIndexByTurn.get(s.turnId) ?? 0,
    endIndex: turnEnds.has(s.turnId) ? (lastIndexByTurn.get(s.turnId) ?? null) : null,
  }))

  return { lanes, nodes, edges, turns, count: nodes.length }
}
