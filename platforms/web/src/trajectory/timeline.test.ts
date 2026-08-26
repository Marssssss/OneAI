import { describe, expect, it } from 'vitest'
import { buildTimeline } from './timeline'
import type { TrajectoryEntry } from '../store/trajectory'

function entry(seq: number, at: number, kind: TrajectoryEntry['kind'], detail?: TrajectoryEntry['detail'], turnId = 't1'): TrajectoryEntry {
  return { seq, turnId, at, ms: null, kind, title: kind, detail }
}

describe('buildTimeline (issue #40)', () => {
  it('maps a flat sequence onto the main lane with uniform indices', () => {
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
    expect(m.nodes[0].index).toBe(0)
    expect(m.turns).toHaveLength(1)
    // The turn's first + last node both resolve to index 0 (the single node).
    expect(m.turns[0].startIndex).toBe(0)
    expect(m.turns[0].endIndex).toBe(0)
    expect(m.count).toBe(1)
    expect(m.edges).toHaveLength(0)
  })

  it('orders nodes by semantic position (context < infer < tool), not wall-clock', () => {
    // `pos` is `iteration * 1000 + phase`; context=0, infer=1, tool=3.
    // Deliberately arrive tool first to prove the sort is pos-driven.
    const entries = [
      entry(1, 1300, 'tool_calls', { kind: 'tool', callId: 'c1', name: 'shell', args: {} }, 't1'),
      entry(2, 1200, 'iteration_start', { kind: 'iteration', iteration: 1, paradigm: 're_act', inference: '', thinking: '' }, 't1'),
      entry(3, 1100, 'context_assembled', { kind: 'context', iteration: 1, sections: [] }, 't1'),
    ]
    // Stamp pos directly (the store would compute this).
    entries[0].pos = 1003
    entries[1].pos = 1001
    entries[2].pos = 1000
    const m = buildTimeline(entries)
    expect(m.nodes.map((n) => n.kind)).toEqual([
      'context_assembled',
      'iteration_start',
      'tool_calls',
    ])
    expect(m.nodes.map((n) => n.index)).toEqual([0, 1, 2])
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
