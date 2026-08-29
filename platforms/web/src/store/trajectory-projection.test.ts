// Issue #40 trajectory data-layer: per-iteration reasoning buffer, context
// assembly cache, tool result backfill, delegate DAG edges, and the
// trajectory-only replay path (session/trajectory).
import { describe, expect, it, vi, afterEach, beforeEach } from 'vitest'
import { ProjectionStore } from './projection'
import type { EngineYield } from '../rpc/types'
import type { OneAiRpcClient } from '../rpc/client'
import type { TrajectoryDetail } from './trajectory'

function fakeRpc(overrides?: Partial<OneAiRpcClient>): OneAiRpcClient {
  return {
    onEvent: () => () => {},
    onStatus: () => () => {},
    getStatus: () => 'closed',
    call: () => Promise.resolve({} as never),
    ...overrides,
  } as unknown as OneAiRpcClient
}

function detail(store: ProjectionStore, kind: string): TrajectoryDetail | undefined {
  return store.getSnapshot().trajectory.find((e) => e.detail?.kind === kind)?.detail
}

describe('trajectory data layer (issue #40)', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('accumulates per-iteration reasoning and flushes it into the iteration node', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)
    y({ kind: 'turn_start', turn_id: 't1', task: 'go' })
    y({ kind: 'iteration_start', turn_id: 't1', iteration: 1, paradigm: 're_act' })
    y({ kind: 'thinking', turn_id: 't1', text: 'reasoning ', speaker: null })
    y({ kind: 'stream_chunk', turn_id: 't1', text: 'answer', speaker: null })
    y({ kind: 'iteration_start', turn_id: 't1', iteration: 2, paradigm: 're_act' })

    const it1 = detail(store, 'iteration') as Extract<TrajectoryDetail, { kind: 'iteration' }>
    expect(it1.kind).toBe('iteration')
    expect(it1.inference).toBe('answer')
    expect(it1.thinking).toBe('reasoning ')
  })

  it('keeps `pos` strictly increasing across turns so rounds never interleave (issue #45)', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)
    // Turn 1: two iterations.
    y({ kind: 'turn_start', turn_id: 't1', task: 'first' })
    y({ kind: 'iteration_start', turn_id: 't1', iteration: 1, paradigm: 're_act' })
    y({ kind: 'iteration_start', turn_id: 't1', iteration: 2, paradigm: 're_act' })
    y({ kind: 'turn_complete', turn_id: 't1', summary: null })
    // Turn 2: iteration counter RESETS to 1 (the engine's per-turn counter).
    y({ kind: 'turn_start', turn_id: 't2', task: 'second' })
    y({ kind: 'iteration_start', turn_id: 't2', iteration: 1, paradigm: 're_act' })
    y({ kind: 'iteration_start', turn_id: 't2', iteration: 2, paradigm: 're_act' })

    const nodes = store
      .getSnapshot()
      .trajectory.filter((e) => e.kind === 'iteration_start')
    const t1 = nodes.filter((e) => e.turnId === 't1')
    const t2 = nodes.filter((e) => e.turnId === 't2')
    expect(t1).toHaveLength(2)
    expect(t2).toHaveLength(2)
    // Every turn-2 node sorts strictly after every turn-1 node (no overlap).
    const maxT1 = Math.max(...t1.map((e) => e.pos!))
    const minT2 = Math.min(...t2.map((e) => e.pos!))
    expect(minT2).toBeGreaterThan(maxT1)
  })

  it('resolves context assembly sections against the cache across iterations', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)
    y({ kind: 'turn_start', turn_id: 't1', task: 'go' })
    y({ kind: 'iteration_start', turn_id: 't1', iteration: 1, paradigm: 're_act' })
    y({
      kind: 'context_assembled',
      turn_id: 't1',
      iteration: 1,
      sections: [
        { key: { type: 'base_prompt' }, label: 'system prompt', tokens: 10, content_hash: 1, content: 'you are OneAI' },
        { key: { type: 'tools' }, label: 'tool definitions', tokens: 40, content_hash: 2, content: '[...]' },
      ],
    })
    // Iteration 2: base prompt unchanged (deduped), tools changed.
    y({ kind: 'iteration_start', turn_id: 't1', iteration: 2, paradigm: 're_act' })
    y({
      kind: 'context_assembled',
      turn_id: 't1',
      iteration: 2,
      sections: [
        { key: { type: 'base_prompt' }, label: 'system prompt', tokens: 10, content_hash: 1 },
        { key: { type: 'tools' }, label: 'tool definitions', tokens: 45, content_hash: 3, content: '[new]' },
      ],
    })

    const ctx = [...store.getSnapshot().trajectory]
      .reverse()
      .find((e) => e.detail?.kind === 'context')?.detail as Extract<TrajectoryDetail, { kind: 'context' }>
    expect(ctx.sections).toHaveLength(2)
    expect(ctx.sections[0].content).toBe('you are OneAI') // resolved from cache
    expect(ctx.sections[1].content).toBe('[new]') // fresh content
  })

  it('backfills a tool node with result + duration', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)
    y({ kind: 'turn_start', turn_id: 't1', task: 'go' })
    y({ kind: 'iteration_start', turn_id: 't1', iteration: 1, paradigm: 're_act' })
    y({
      kind: 'tool_calls',
      turn_id: 't1',
      calls: [{ id: 'c1', name: 'shell', args: { command: 'ls' } }],
      speaker: null,
    })
    vi.advanceTimersByTime(100)
    y({
      kind: 'tool_result',
      turn_id: 't1',
      call_id: 'c1',
      tool_name: 'shell',
      output: { success: true, content: 'ok' },
      speaker: null,
    })

    const tool = detail(store, 'tool') as Extract<TrajectoryDetail, { kind: 'tool' }>
    expect(tool.callId).toBe('c1')
    expect(tool.result).toBe('ok')
    expect(tool.ok).toBe(true)
    expect(tool.durationMs).toBeGreaterThanOrEqual(100)
  })

  it('carries delegate depends_on through to the ledger', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)
    y({ kind: 'turn_start', turn_id: 't1', task: 'go' })
    y({
      kind: 'delegate',
      turn_id: 't1',
      task_id: 'd2',
      task: 'step two',
      agent_kind: 'code',
      speaker: null,
      depends_on: ['d1'],
    })

    const d = detail(store, 'delegate') as Extract<TrajectoryDetail, { kind: 'delegate' }>
    expect(d.taskId).toBe('d2')
    expect(d.dependsOn).toEqual(['d1'])
  })

  it('replays a historical trajectory without touching chat nodes', async () => {
    const events = [
      JSON.stringify({ kind: 'turn_start', turn_id: 't1', task: 'hello', ts: 1000 }),
      JSON.stringify({ kind: 'iteration_start', turn_id: 't1', iteration: 1, paradigm: 're_act', ts: 1001 }),
      JSON.stringify({ kind: 'tool_calls', turn_id: 't1', calls: [{ id: 'c1', name: 'shell', args: {} }], ts: 1002 }),
      JSON.stringify({ kind: 'turn_complete', turn_id: 't1', ts: 1003 }),
    ]
    const rpc = fakeRpc({
      call: () => Promise.resolve({ ok: true, events } as never),
    })
    const store = new ProjectionStore(rpc)
    // session_loaded populates chat nodes from messages (empty here), then
    // kicks off the replay.
    store.consume({ kind: 'session_loaded', id: 's1', messages: [] })
    await vi.runAllTimersAsync()
    await Promise.resolve()

    const snap = store.getSnapshot()
    // Chat nodes stay empty (the trajectory is a separate surface).
    expect(snap.nodes).toHaveLength(0)
    // The ledger carries the replayed events, preserving engine timestamps.
    expect(snap.trajectory).toHaveLength(4)
    expect(snap.trajectory[0].at).toBe(1000)
    expect(snap.trajectory.map((e) => e.kind)).toEqual([
      'turn_start',
      'iteration_start',
      'tool_calls',
      'turn_complete',
    ])
  })

  it('positions context before infer, approval before tool, and attaches inference detail (streaming order)', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)
    y({ kind: 'turn_start', turn_id: 't1', task: 'go' })
    y({ kind: 'iteration_start', turn_id: 't1', iteration: 1, paradigm: 're_act' })
    vi.advanceTimersByTime(5)
    y({
      kind: 'context_assembled',
      turn_id: 't1',
      iteration: 1,
      sections: [{ key: { type: 'base_prompt' }, label: 'system prompt', tokens: 10, content_hash: 1, content: 'sys' }],
      duration_ms: 7,
    })
    vi.advanceTimersByTime(5)
    // Streaming path: the tool call is detected mid-stream, BEFORE `inference`
    // fires at stream end — so `tool_calls` finalizes the text buffer first.
    y({ kind: 'tool_calls', turn_id: 't1', calls: [{ id: 'c1', name: 'shell', args: {} }], speaker: null })
    vi.advanceTimersByTime(5)
    y({
      kind: 'inference',
      turn_id: 't1',
      snapshot: {
        iteration: 1,
        model: 'gpt-4o',
        temperature: 0.3,
        max_tokens: 4096,
        top_p: null,
        thinking_budget: null,
        tool_names: ['shell'],
        message_count: 1,
        request_messages: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
        request_body: { model: 'gpt-4o', messages: [{ role: 'user' }], tools: [{ name: 'shell' }] },
        response: {
          message: { role: 'assistant', content: [{ type: 'text', text: 'ok' }] },
          usage: { prompt_tokens: 10, completion_tokens: 1, total_tokens: 11, cache_read_tokens: 2, cache_creation_tokens: 0 },
          model: 'gpt-4o',
        },
        response_body: { model: 'gpt-4o', message: { role: 'assistant' } },
        duration_ms: 99,
      },
    })
    vi.advanceTimersByTime(5)
    y({
      kind: 'approval_request',
      request_id: 'r1',
      request: { ToolApproval: { approval: { tool_name: 'shell', args: {}, risk_level: 'low', justification: '' } } },
    })

    const snap = store.getSnapshot()
    const infer = snap.trajectory.find((e) => e.detail?.kind === 'iteration')!
    const context = snap.trajectory.find((e) => e.detail?.kind === 'context')!
    const tool = snap.trajectory.find((e) => e.kind === 'tool_calls')!
    const approval = snap.trajectory.find((e) => e.detail?.kind === 'approval')!

    // Semantic ordering via `pos` (context 1000 < infer 1001 < approval 1002
    // < tool 1003), independent of wall-clock arrival.
    expect(context.pos!).toBeLessThan(infer.pos!)
    expect(infer.pos!).toBeLessThan(tool.pos!)
    expect(approval.pos!).toBeLessThan(tool.pos!)

    const id = infer.detail as Extract<TrajectoryDetail, { kind: 'iteration' }>
    // The inference detail attaches even though `tool_calls` (mid-stream)
    // finalized the text buffer before `inference` arrived.
    expect(id.inferenceDetail).toBeDefined()
    expect(id.durationMs).toBe(99)
    expect(id.inferenceDetail!.response.usage.prompt_tokens).toBe(10)

    const cd = context.detail as Extract<TrajectoryDetail, { kind: 'context' }>
    expect(cd.durationMs).toBe(7)
  })

  it('ignores interrupted/reflection kinds that older frontends drop, but records them here', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)
    y({ kind: 'turn_start', turn_id: 't1', task: 'go' })
    y({ kind: 'interrupted', turn_id: 't1', reason: 'rate limited', point: 'pre_infer' })
    y({ kind: 'reflection', turn_id: 't1', summary: 'reconsider' })
    const snap = store.getSnapshot()
    expect(snap.trajectory.map((e) => e.kind)).toContain('interrupted')
    expect(snap.trajectory.map((e) => e.kind)).toContain('reflection')
  })

  it('dedupes duplicate tool_calls in replay (streaming + decision paths both persist)', async () => {
    const events = [
      JSON.stringify({ kind: 'turn_start', turn_id: 't1', task: 'hello', ts: 1000 }),
      JSON.stringify({ kind: 'iteration_start', turn_id: 't1', iteration: 1, paradigm: 're_act', ts: 1001 }),
      // Same call id persisted twice (mid-stream ToolCallComplete + decision
      // path AgentDecision::ToolCalls), then one result.
      JSON.stringify({ kind: 'tool_calls', turn_id: 't1', calls: [{ id: 'c1', name: 'shell', args: { cmd: 'ls' } }], ts: 1002 }),
      JSON.stringify({ kind: 'tool_calls', turn_id: 't1', calls: [{ id: 'c1', name: 'shell', args: { cmd: 'ls' } }], ts: 1003 }),
      JSON.stringify({ kind: 'tool_result', turn_id: 't1', call_id: 'c1', tool_name: 'shell', output: { success: true, content: 'ok' }, ts: 1004 }),
      JSON.stringify({ kind: 'turn_complete', turn_id: 't1', ts: 1005 }),
    ]
    const rpc = fakeRpc({ call: () => Promise.resolve({ ok: true, events } as never) })
    const store = new ProjectionStore(rpc)
    store.consume({ kind: 'session_loaded', id: 's1', messages: [] })
    await vi.runAllTimersAsync()
    await Promise.resolve()

    const tools = store.getSnapshot().trajectory.filter((e) => e.kind === 'tool_calls')
    expect(tools).toHaveLength(1)
    expect((tools[0].detail as Extract<TrajectoryDetail, { kind: 'tool' }>).result).toBe('ok')
  })

  it('suppresses no-op paradigm_switch nodes (re_act → re_act)', () => {
    const store = new ProjectionStore(fakeRpc())
    const y = (o: EngineYield) => store.consume(o)
    y({ kind: 'turn_start', turn_id: 't1', task: 'go' })
    y({ kind: 'paradigm_switch', turn_id: 't1', from: 're_act', to: 're_act' })
    y({ kind: 'paradigm_switch', turn_id: 't1', from: 're_act', to: 'plan' })

    const paradigms = store.getSnapshot().trajectory.filter((e) => e.kind === 'paradigm_switch')
    expect(paradigms).toHaveLength(1)
    const d = paradigms[0].detail as Extract<TrajectoryDetail, { kind: 'paradigm' }>
    expect(d.from).toBe('re_act')
    expect(d.to).toBe('plan')
  })
})
