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

  it('orders multi-turn sessions as disjoint turn spans (issue #45)', () => {
    // The store stamps `pos = turnOrdinal * 1_000_000 + iteration * 1000 + phase`,
    // so turn 2's iteration 1 must NOT collide with turn 1's iteration 1. Here
    // we reproduce that shape: two turns, each with two iterations, both turns'
    // iteration counters restarting at 1.
    const entries: TrajectoryEntry[] = [
      entry(1, 1000, 'turn_start', { kind: 'turn', task: 'first' }, 't1'),
      entry(2, 1100, 'iteration_start', { kind: 'iteration', iteration: 1, paradigm: 're_act', inference: '', thinking: '' }, 't1'),
      entry(3, 1200, 'iteration_start', { kind: 'iteration', iteration: 2, paradigm: 're_act', inference: '', thinking: '' }, 't1'),
      entry(4, 1300, 'turn_complete', { kind: 'turn_complete' }, 't1'),
      entry(5, 1400, 'turn_start', { kind: 'turn', task: 'second' }, 't2'),
      entry(6, 1500, 'iteration_start', { kind: 'iteration', iteration: 1, paradigm: 're_act', inference: '', thinking: '' }, 't2'),
      entry(7, 1600, 'iteration_start', { kind: 'iteration', iteration: 2, paradigm: 're_act', inference: '', thinking: '' }, 't2'),
      entry(8, 1700, 'turn_complete', { kind: 'turn_complete' }, 't2'),
    ]
    // Stamp the store's turn-aware pos (turn 1 → ordinal 1, turn 2 → ordinal 2).
    entries[1].pos = 1_000_000 + 1 * 1000 + 1
    entries[2].pos = 1_000_000 + 2 * 1000 + 1
    entries[5].pos = 2_000_000 + 1 * 1000 + 1
    entries[6].pos = 2_000_000 + 2 * 1000 + 1

    const m = buildTimeline(entries)
    expect(m.turns).toHaveLength(2)
    // Turn 1's span ends before turn 2's span begins — no overlap, so the
    // turn boundary markers separate the two rounds instead of crossing.
    expect(m.turns[0].endIndex!).toBeLessThan(m.turns[1].startIndex)
    // Nodes are strictly turn-major: all of turn 1, then all of turn 2.
    expect(m.nodes.map((n) => n.entry.turnId)).toEqual(['t1', 't1', 't2', 't2'])
  })

  it('turns tool duration into a node span', () => {
    const entries = [
      entry(1, 1000, 'tool_calls', { kind: 'tool', callId: 'c1', name: 'shell', args: {}, durationMs: 250 }),
    ]
    const m = buildTimeline(entries)
    expect(m.nodes[0].durationMs).toBe(250)
  })
})
