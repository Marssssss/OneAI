import { describe, expect, it } from 'vitest'
import { buildTimeline } from './timeline'
import type { TrajectoryEntry } from '../store/trajectory'

function entry(seq: number, at: number, kind: TrajectoryEntry['kind'], detail?: TrajectoryEntry['detail'], turnId = 't1'): TrajectoryEntry {
  return { seq, turnId, at, ms: null, kind, title: kind, detail }
}

describe('buildTimeline (issue #40)', () => {
  it('maps a flat sequence onto the main lane with a time range', () => {
    const entries = [
      entry(1, 1000, 'turn_start', { kind: 'turn', task: 'go' }),
      entry(2, 1100, 'iteration_start', { kind: 'iteration', iteration: 1, paradigm: 're_act', inference: '', thinking: '' }),
      entry(3, 1300, 'turn_complete', { kind: 'turn_complete' }),
    ]
    const m = buildTimeline(entries)
    expect(m.lanes).toHaveLength(1)
    // turn_start/turn_complete are folded into `turns`, not nodes.
    expect(m.nodes).toHaveLength(1)
    expect(m.nodes[0].kind).toBe('iteration_start')
    expect(m.nodes[0].lane).toBe(0)
    expect(m.turns).toHaveLength(1)
    expect(m.turns[0].startAt).toBe(1000)
    expect(m.turns[0].endAt).toBe(1300)
    expect(m.timeRange).toEqual([1100, 1100])
    expect(m.edges).toHaveLength(0)
  })

  it('assigns a child lane + fork/join edges for a delegation', () => {
    const entries = [
      entry(1, 1000, 'turn_start', { kind: 'turn', task: 'go' }),
      entry(2, 1100, 'delegate', { kind: 'delegate', taskId: 'd1', task: 'explore', agentKind: 'code', dependsOn: [] }),
      entry(3, 1150, 'delegate_progress', { kind: 'delegate_progress', taskId: 'd1', event: { kind: 'iteration_start', iteration: 1, paradigm: 're_act' } }),
      entry(4, 1200, 'delegate_complete', { kind: 'delegate_complete', taskId: 'd1', summary: { summary: 'done', keyFindings: [], budgetExceeded: false, tokensUsed: 10, completed: true } }),
      entry(5, 1250, 'turn_complete', { kind: 'turn_complete' }),
    ]
    const m = buildTimeline(entries)
    expect(m.lanes).toHaveLength(2)
    expect(m.lanes[1].id).toBe('task:d1')
    // child progress node lands on lane 1
    const progress = m.nodes.find((n) => n.entry.detail?.kind === 'delegate_progress')
    expect(progress?.lane).toBe(1)
    // fork edge: delegate → first child node; join: last child → complete
    const forks = m.edges.filter((e) => e.kind === 'fork')
    const joins = m.edges.filter((e) => e.kind === 'join')
    expect(forks).toHaveLength(1)
    expect(joins).toHaveLength(1)
  })

  it('draws a depends edge between serial delegations', () => {
    const entries = [
      entry(1, 1000, 'delegate', { kind: 'delegate', taskId: 'a', task: 'first', agentKind: 'code', dependsOn: [] }),
      entry(2, 1100, 'delegate', { kind: 'delegate', taskId: 'b', task: 'second', agentKind: 'code', dependsOn: ['a'] }),
    ]
    const m = buildTimeline(entries)
    const depends = m.edges.filter((e) => e.kind === 'depends')
    expect(depends).toHaveLength(1)
    expect(depends[0].fromSeq).toBe(1)
    expect(depends[0].toSeq).toBe(2)
  })

  it('turns tool duration into a node span', () => {
    const entries = [
      entry(1, 1000, 'tool_calls', { kind: 'tool', callId: 'c1', name: 'shell', args: {}, durationMs: 250 }),
    ]
    const m = buildTimeline(entries)
    expect(m.nodes[0].durationMs).toBe(250)
  })
})
