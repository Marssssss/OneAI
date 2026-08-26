// Scripted EngineYield sequences the mock ws server replays. Pure data so
// the e2e flows are deterministic and Rust-free — the real app-server isn't
// spun up in the JS test path.
import type { EngineYield } from '../../src/rpc/types'

const TURN = 'turn-e2e'

/** A single-agent turn: turn_start → N text deltas → turn_complete. */
export function singleAgentReply(text: string, turnId = TURN): EngineYield[] {
  const chunks = text.match(/.{1,5}/g) ?? [text]
  const out: EngineYield[] = [{ kind: 'turn_start', turn_id: turnId, task: '' }]
  for (const c of chunks) {
    out.push({ kind: 'stream_chunk', turn_id: turnId, text: c, speaker: null })
  }
  out.push({ kind: 'turn_complete', turn_id: turnId, summary: null })
  return out
}

/** A group-chat round: speaker_turn per member → their text deltas → ... */
export function groupRound(
  speakers: { id: string; text: string }[],
  turnId = TURN,
): EngineYield[] {
  const out: EngineYield[] = [{ kind: 'turn_start', turn_id: turnId, task: '' }]
  for (const s of speakers) {
    out.push({ kind: 'speaker_turn', turn_id: turnId, speaker: s.id })
    const chunks = s.text.match(/.{1,6}/g) ?? [s.text]
    for (const c of chunks) {
      out.push({ kind: 'stream_chunk', turn_id: turnId, text: c, speaker: s.id })
    }
  }
  out.push({ kind: 'turn_complete', turn_id: turnId, summary: null })
  return out
}

/**
 * A turn that pauses for approval: turn_start → tool_calls(shell) → then the
 * server waits for `approval/respond` before pushing tool_result → a short
 * assistant summary → turn_complete. The pre-approval and post-approval
 * halves are split so the mock can await the user's respond.
 */
export function approvalPre(turnId = TURN): EngineYield[] {
  return [
    { kind: 'turn_start', turn_id: turnId, task: '' },
    {
      kind: 'tool_calls',
      turn_id: turnId,
      calls: [{ id: 'call_shell', name: 'shell', args: { cmd: 'ls' } }],
      speaker: null,
    },
    {
      kind: 'approval_request',
      request_id: 'req-e2e',
      request: {
        ToolApproval: {
          approval: {
            tool_name: 'shell',
            args: { cmd: 'ls' },
            risk_level: 'standard',
            justification: 'list files',
          },
        },
      },
    },
  ]
}

export function approvalPost(turnId = TURN): EngineYield[] {
  return [
    {
      kind: 'tool_result',
      turn_id: turnId,
      call_id: 'call_shell',
      tool_name: 'shell',
      speaker: null,
      output: { success: true, content: 'file1\nfile2' },
    },
    { kind: 'stream_chunk', turn_id: turnId, text: 'Done.', speaker: null },
    { kind: 'turn_complete', turn_id: turnId, summary: null },
  ]
}

/** Issue #40 — a trajectory-rich turn: iteration boundaries, context
 *  assembly, a tool round-trip, and a delegation (fork → child progress →
 *  join). Drives the TrajectoryExplorer e2e. */
export function trajectoryReply(turnId = TURN): EngineYield[] {
  return [
    { kind: 'turn_start', turn_id: turnId, task: 'plan and execute a task' },
    { kind: 'iteration_start', turn_id: turnId, iteration: 1, paradigm: 're_act' },
    {
      kind: 'context_assembled',
      turn_id: turnId,
      iteration: 1,
      sections: [
        { key: { type: 'base_prompt' }, label: 'system prompt', tokens: 120, content_hash: 1, content: 'You are OneAI.' },
        { key: { type: 'tools' }, label: 'tool definitions', tokens: 80, content_hash: 2, content: 'shell, read_file' },
        { key: { type: 'latest_user' }, label: 'latest user message', tokens: 6, content_hash: 3, content: 'plan and execute a task' },
      ],
    },
    {
      kind: 'tool_calls',
      turn_id: turnId,
      calls: [{ id: 'call_1', name: 'shell', args: { command: 'ls' } }],
      speaker: null,
    },
    {
      kind: 'tool_result',
      turn_id: turnId,
      call_id: 'call_1',
      tool_name: 'shell',
      output: { success: true, content: 'file1\nfile2' },
      speaker: null,
    },
    { kind: 'iteration_start', turn_id: turnId, iteration: 2, paradigm: 're_act' },
    {
      kind: 'delegate',
      turn_id: turnId,
      task_id: 'd1',
      task: 'explore the codebase',
      agent_kind: 'code',
      speaker: null,
      depends_on: [],
    },
    { kind: 'delegate_progress', turn_id: turnId, task_id: 'd1', agent_kind: 'code', event: { kind: 'iteration_start', iteration: 1, paradigm: 're_act' } },
    { kind: 'delegate_progress', turn_id: turnId, task_id: 'd1', agent_kind: 'code', event: { kind: 'tool_result', tool_name: 'grep', snapshot: 'hit' } },
    {
      kind: 'delegate_complete',
      turn_id: turnId,
      task_id: 'd1',
      summary: { completed: true, summary: 'found the entry point', key_findings: ['main.rs'], budget_exceeded: false, agent_kind: 'code', tokens_used: 120 },
      speaker: null,
    },
    { kind: 'turn_complete', turn_id: turnId, summary: null },
  ]
}
