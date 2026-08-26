// Timeline model builder (issue #40) — a pure function that folds the flat
// trajectory ledger into swim-lanes + nodes + fork/join edges the SVG canvas
// renders. No React, no DOM — trivially unit-testable.

import type { TrajectoryDetail, TrajectoryEntry } from '../store/trajectory'

export interface TimelineNode {
  seq: number
  lane: number
  kind: TrajectoryEntry['kind']
  title: string
  /** Epoch ms — the x position along the timeline. */
  at: number
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

export interface TimelineModel {
  lanes: TimelineLane[]
  nodes: TimelineNode[]
  edges: TimelineEdge[]
  timeRange: [number, number]
}

const MAIN_LANE = 0

/**
 * Build the timeline. Lanes: `0` = main agent, `1..` = one per delegated
 * task id (in first-seen order). Child-lane nodes are the `delegate_progress`
 * entries (forwarded sub-agent iteration/tool/usage events); `delegate` /
 * `delegate_complete` stay on the main lane as the fork/join anchors.
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

  // First pass: assign lanes + collect task ids.
  const nodes: TimelineNode[] = entries.map((e) => {
    let lane = MAIN_LANE
    let durationMs = 0
    const d = e.detail
    if (d) {
      if (d.kind === 'delegate_progress') lane = laneForTask(d.taskId)
      if (d.kind === 'tool' && typeof d.durationMs === 'number') durationMs = d.durationMs
    }
    return { seq: e.seq, lane, kind: e.kind, title: e.title, at: e.at, durationMs, entry: e }
  })

  // Second pass: fork/join/depends edges.
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

  // Time range.
  const times = nodes.map((n) => n.at)
  const timeRange: [number, number] =
    times.length > 0
      ? [Math.min(...times), Math.max(...times)]
      : [0, 1]

  return { lanes, nodes, edges, timeRange }
}
