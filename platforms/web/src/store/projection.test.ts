import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ProjectionStore } from './projection'
import type { EngineYield } from '../rpc/types'
import type { OneAiRpcClient } from '../rpc/client'

// Drives ProjectionStore.consume with scripted EngineYield sequences and
// asserts the projected ChatNode tree + approval queue + trajectory. No real
// ws — a fake rpc is cast in (consume never touches it; attach() isn't called).

function fakeRpc(): OneAiRpcClient {
  return {
    onEvent: () => () => {},
    onStatus: () => () => {},
    getStatus: () => 'closed',
    call: () => Promise.resolve({} as never),
  } as unknown as OneAiRpcClient
}

describe('ProjectionStore.consume', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('defers the assistant node seed to the first stream_chunk (so a leading thinking fragment owns the top slot), finalizes on turn_complete', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)

    y({ kind: 'turn_start', turn_id: 't1', task: 'hello' })
    let snap = store.getSnapshot()
    expect(snap.turnActive).toBe(true)
    // No eager empty bubble — the seed is deferred to the first chunk (TypingDots
    // signals "in flight"), so a leading thinking fragment renders above text.
    expect(snap.nodes).toHaveLength(0)

    // Two stream chunks — coalesced (deferred), so flush explicitly by time.
    // The first chunk seeds the streaming text node.
    y({ kind: 'stream_chunk', turn_id: 't1', text: 'Hel', speaker: null })
    y({ kind: 'stream_chunk', turn_id: 't1', text: 'lo', speaker: null })
    vi.advanceTimersByTime(60); vi.advanceTimersToNextFrame() // fire the 50ms timer + its queued rAF drain

    snap = store.getSnapshot()
    expect(snap.nodes).toHaveLength(1)
    expect(snap.nodes[0].kind).toBe('text')
    expect(snap.nodes[0].state).toBe('streaming')
    expect(snap.nodes[0].text).toBe('Hello')

    y({ kind: 'turn_complete', turn_id: 't1', summary: null })
    snap = store.getSnapshot()
    expect(snap.turnActive).toBe(false)
    expect(snap.nodes[0].state).toBe('done')
    expect(snap.nodes[0].text).toBe('Hello')
  })

  it('creates a pending tool node on tool_calls, fills it on tool_result', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)

    y({ kind: 'turn_start', turn_id: 't2', task: '' })
    y({
      kind: 'tool_calls',
      turn_id: 't2',
      calls: [{ id: 'call_a', name: 'write_file', args: { path: '/x.txt' } }],
      speaker: null,
    })
    let snap = store.getSnapshot()
    const toolNode = snap.nodes.find((n) => n.kind === 'tool')!
    expect(toolNode).toBeDefined()
    expect(toolNode.toolState).toBe('pending')
    expect(toolNode.toolName).toBe('write_file')

    y({
      kind: 'tool_result',
      turn_id: 't2',
      call_id: 'call_a',
      tool_name: 'write_file',
      speaker: null,
      output: {
        success: true,
        content: 'wrote 10 bytes',
        artifacts: [{ path: '/x.txt', mime_type: 'text/plain', description: 'p', size_bytes: 10 }],
      },
    })
    snap = store.getSnapshot()
    const done = snap.nodes.find((n) => n.kind === 'tool')!
    expect(done.toolState).toBe('done')
    expect(done.toolOutput?.success).toBe(true)
    expect(done.toolOutput?.artifacts).toHaveLength(1)
  })

  it('dedupes tool_calls re-emitted with the same call id (streaming + decision paths both fire)', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)

    y({ kind: 'turn_start', turn_id: 't2', task: '' })
    y({
      kind: 'tool_calls',
      turn_id: 't2',
      calls: [{ id: 'call_a', name: 'bash', args: { command: 'echo hi' } }],
      speaker: null,
    })
    // The decision path re-emits the same call id (what the engine does in one
    // streaming iteration) — must NOT spawn a second pending card.
    y({
      kind: 'tool_calls',
      turn_id: 't2',
      calls: [{ id: 'call_a', name: 'bash', args: { command: 'echo hi' } }],
      speaker: null,
    })
    const snap = store.getSnapshot()
    const toolNodes = snap.nodes.filter((n) => n.kind === 'tool')
    expect(toolNodes).toHaveLength(1)
    expect(toolNodes[0].callId).toBe('call_a')
  })

  it('aggregates the turn tool artifacts as deliverables on the final assistant text node', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)

    y({ kind: 'turn_start', turn_id: 't3', task: '' })
    y({
      kind: 'tool_calls',
      turn_id: 't3',
      calls: [{ id: 'c1', name: 'write_file', args: {} }],
      speaker: null,
    })
    y({
      kind: 'tool_result',
      turn_id: 't3',
      call_id: 'c1',
      tool_name: 'write_file',
      speaker: null,
      output: {
        success: true,
        content: 'ok',
        artifacts: [{ path: '/a.txt', mime_type: 'text/plain', description: 'p' }],
      },
    })
    y({ kind: 'stream_chunk', turn_id: 't3', text: 'done', speaker: null })
    vi.advanceTimersByTime(60)
    y({ kind: 'turn_complete', turn_id: 't3', summary: null })

    const snap = store.getSnapshot()
    // Deliverables attach to the LAST assistant text node of the turn.
    let textNode = snap.nodes[snap.nodes.length - 1]
    for (let i = snap.nodes.length - 1; i >= 0; i--) {
      if (snap.nodes[i].kind === 'text') {
        textNode = snap.nodes[i]
        break
      }
    }
    expect(textNode.deliverables).toBeDefined()
    expect(textNode.deliverables![0].path).toBe('/a.txt')
  })

  it('enqueues approval_request and exposes it as currentApproval', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)

    y({
      kind: 'approval_request',
      request_id: 'req1',
      request: {
        ToolApproval: {
          approval: {
            tool_name: 'shell',
            args: { cmd: 'rm -rf /' },
            risk_level: 'high',
            justification: 'dangerous',
          },
        },
      },
    })
    const snap = store.getSnapshot()
    expect(snap.currentApproval).not.toBeNull()
    expect(snap.currentApproval!.request_id).toBe('req1')
    expect(snap.approvalQueueDepth).toBe(0)
  })

  it('flips paradigm on paradigm_switch and records trajectory entries', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)

    expect(store.getSnapshot().paradigm).toBe('re_act')
    y({ kind: 'turn_start', turn_id: 't4', task: '' })
    y({ kind: 'paradigm_switch', turn_id: 't4', from: 're_act', to: 'plan' })
    const snap = store.getSnapshot()
    expect(snap.paradigm).toBe('plan')
    expect(snap.trajectory.length).toBeGreaterThanOrEqual(1)
    expect(snap.trajectory.some((e) => e.kind === 'turn_start')).toBe(true)
  })

  it('ignores unknown yield kinds without throwing', () => {
    const store = new ProjectionStore(fakeRpc())
    expect(() =>
      store.consume({ kind: 'init_result' as EngineYield['kind'] } as EngineYield),
    ).not.toThrow()
  })
})
