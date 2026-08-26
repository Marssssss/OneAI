// Trajectory node color mapping (issue #40) — one hue per node kind, resolved
// through the `--oneai-traj-*` design tokens (light/dark via the theme
// attribute). Grouped kinds share a hue so the legend stays compact.

import type { TrajectoryEntry } from '../store/trajectory'

export type TrajectoryKind = TrajectoryEntry['kind']

/** Node kind → CSS variable token. Grouped kinds collapse to one legend row. */
const KIND_TO_TOKEN: Record<string, string> = {
  turn_start: 'var(--oneai-traj-turn)',
  turn_complete: 'var(--oneai-traj-turn)',
  iteration_start: 'var(--oneai-traj-iteration)',
  tool_calls: 'var(--oneai-traj-tool)',
  tool_result: 'var(--oneai-traj-tool)',
  delegate: 'var(--oneai-traj-delegate)',
  delegate_progress: 'var(--oneai-traj-delegate)',
  delegate_complete: 'var(--oneai-traj-delegate)',
  plan_revision: 'var(--oneai-traj-plan)',
  paradigm_switch: 'var(--oneai-traj-paradigm)',
  context_assembled: 'var(--oneai-traj-context)',
  context_accounting: 'var(--oneai-traj-context)',
  working_state: 'var(--oneai-traj-working)',
  token_usage: 'var(--oneai-traj-usage)',
  tools_added: 'var(--oneai-traj-tools)',
  interrupted: 'var(--oneai-traj-interrupt)',
  reflection: 'var(--oneai-traj-reflection)',
  error: 'var(--oneai-traj-error)',
  approval_request: 'var(--oneai-traj-approval)',
  round_boundary: 'var(--oneai-traj-turn)',
}

export function colorFor(kind: TrajectoryKind): string {
  return KIND_TO_TOKEN[kind] ?? 'var(--oneai-text-tertiary)'
}

/** Legend entries — one per distinct hue, with the kinds it represents. */
export interface LegendEntry {
  color: string
  label: string
  kinds: TrajectoryKind[]
}

export const LEGEND: LegendEntry[] = [
  { color: 'var(--oneai-traj-turn)', label: 'turn', kinds: ['turn_start', 'turn_complete'] },
  { color: 'var(--oneai-traj-iteration)', label: 'iteration', kinds: ['iteration_start'] },
  { color: 'var(--oneai-traj-tool)', label: 'tool', kinds: ['tool_calls', 'tool_result'] },
  { color: 'var(--oneai-traj-delegate)', label: 'delegate', kinds: ['delegate', 'delegate_progress', 'delegate_complete'] },
  { color: 'var(--oneai-traj-plan)', label: 'plan', kinds: ['plan_revision'] },
  { color: 'var(--oneai-traj-paradigm)', label: 'paradigm', kinds: ['paradigm_switch'] },
  { color: 'var(--oneai-traj-context)', label: 'context', kinds: ['context_assembled', 'context_accounting'] },
  { color: 'var(--oneai-traj-working)', label: 'working', kinds: ['working_state'] },
  { color: 'var(--oneai-traj-usage)', label: 'usage', kinds: ['token_usage'] },
  { color: 'var(--oneai-traj-tools)', label: 'tools', kinds: ['tools_added'] },
  { color: 'var(--oneai-traj-interrupt)', label: 'interrupt', kinds: ['interrupted'] },
  { color: 'var(--oneai-traj-reflection)', label: 'reflection', kinds: ['reflection'] },
  { color: 'var(--oneai-traj-error)', label: 'error', kinds: ['error'] },
  { color: 'var(--oneai-traj-approval)', label: 'approval', kinds: ['approval_request'] },
]
