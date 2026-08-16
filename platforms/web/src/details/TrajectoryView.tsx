// TrajectoryView — the trajectory tab of the details rail.
//
// A pure-consumer rendering of the projection's `trajectory` ledger (a
// turn-aware event timeline assembled from the EngineYield kinds W1–W3
// dropped) plus a timing overview bar (per-iteration elapsed ms derived from
// the projection's turn-start performance.now() marks, and the latest token
// usage / context-accounting snapshots). No D3 / StateGraph bridge — the
// design doc's "复用 Studio graph-render" was descoped to the lighter
// "tab + event ledger + timing" tier the user picked.

import type { ReactNode } from 'react'
import type { TrajectoryEntry } from '../store/trajectory'
import type { UsageSnapshot } from '../store/trajectory'
import type { SubagentNode } from '../store/trajectory'
import type { ProjectionSnapshot } from '../store/projection'
import { useLocale } from '../i18n'
import { SubagentTree } from './SubagentTree'
import styles from './TrajectoryView.module.css'

interface TrajectoryViewProps {
  trajectory: TrajectoryEntry[]
  usage: UsageSnapshot
  turnTimings: ProjectionSnapshot['turnTimings']
  subagents: SubagentNode[]
}

interface TurnGroup {
  turnId: string
  entries: TrajectoryEntry[]
  iterations: number
  durationMs: number | null
}

/** Group the ledger by turn (descending — most recent turn first). Session-
 *  scoped entries (turnId === null, e.g. token_usage) fold into the most
 *  recent turn group, or a leading "session" group when none yet. */
function groupByTurn(
  trajectory: TrajectoryEntry[],
  timings: TrajectoryViewProps['turnTimings'],
): TurnGroup[] {
  const byTurn = new Map<string, TrajectoryEntry[]>()
  let sessionEntries: TrajectoryEntry[] = []
  for (const e of trajectory) {
    if (e.turnId === null) {
      sessionEntries.push(e)
      continue
    }
    const arr = byTurn.get(e.turnId)
    if (arr !== undefined) arr.push(e)
    else byTurn.set(e.turnId, [e])
  }
  const groups: TurnGroup[] = []
  for (const [turnId, entries] of byTurn) {
    const t = timings.find((x) => x.turnId === turnId)
    let duration: number | null = null
    if (t?.startedAt != null && t?.endedAt != null) {
      duration = Math.round(t.endedAt - t.startedAt)
    } else if (t?.startedAt != null) {
      duration = Math.round(performance.now() - t.startedAt)
    }
    groups.push({
      turnId,
      entries,
      iterations: t?.iterations ?? entries.filter((e) => e.kind === 'iteration_start').length,
      durationMs: duration,
    })
  }
  // Most recent turn first (by seq of last entry).
  groups.sort((a, b) => {
    const al = a.entries[a.entries.length - 1]?.seq ?? 0
    const bl = b.entries[b.entries.length - 1]?.seq ?? 0
    return bl - al
  })
  if (sessionEntries.length > 0 && groups.length > 0) {
    groups[0].entries = [...groups[0].entries, ...sessionEntries].sort((a, b) => a.seq - b.seq)
  }
  return groups
}

export function TrajectoryView({
  trajectory,
  usage,
  turnTimings,
  subagents,
}: TrajectoryViewProps): ReactNode {
  const { t } = useLocale()
  const groups = groupByTurn(trajectory, turnTimings)

  if (trajectory.length === 0 && subagents.length === 0) {
    return <div className={styles.empty}>{t('trajectory.empty')}</div>
  }

  const totalTokens = usage.usage !== null
      ? usage.usage.prompt_tokens + usage.usage.completion_tokens
      : null

  return (
    <div className={styles.root}>
      <SubagentTree subagents={subagents} />
      <section className={styles.overview}>
        <div className={styles.overviewRow}>
          <span className={styles.overviewLabel}>{t('trajectory.turns')}</span>
          <span className={styles.overviewValue}>{groups.length}</span>
        </div>
        {totalTokens !== null && (
          <div className={styles.overviewRow}>
            <span className={styles.overviewLabel}>{t('trajectory.tokens')}</span>
            <span className={styles.overviewValue}>{totalTokens.toLocaleString()}</span>
          </div>
        )}
        {usage.usage !== null && (
          <div className={styles.usageDetail}>
            <span>
              {t('trajectory.prompt')}: {usage.usage.prompt_tokens.toLocaleString()}
            </span>
            <span>
              {t('trajectory.completion')}: {usage.usage.completion_tokens.toLocaleString()}
            </span>
            {usage.usage.cache_read_tokens > 0 && (
              <span>
                {t('trajectory.cacheRead')}: {usage.usage.cache_read_tokens.toLocaleString()}
              </span>
            )}
          </div>
        )}
      </section>

      {groups.map((g) => (
        <section key={g.turnId} className={styles.group}>
          <header className={styles.groupHeader}>
            <span className={styles.groupTitle}>turn {shortId(g.turnId)}</span>
            <span className={styles.groupMeta}>
              {g.iterations} {t('trajectory.iterations')}
              {g.durationMs !== null ? ` · ${g.durationMs}ms` : ''}
            </span>
          </header>
          {g.durationMs !== null && (
            <TimingBar entries={g.entries} totalMs={g.durationMs} />
          )}
          <ol className={styles.timeline}>
            {g.entries.map((e) => (
              <li key={e.seq} className={styles.entry}>
                <span className={styles.entryIcon} aria-hidden>
                  {iconFor(e.kind)}
                </span>
                <span className={styles.entryTitle}>{e.title}</span>
                <span className={styles.entryMs}>
                  {e.iter !== undefined ? `#${e.iter}` : e.ms !== null ? `${e.ms}ms` : ''}
                </span>
              </li>
            ))}
          </ol>
        </section>
      ))}
    </div>
  )
}

function shortId(id: string): string {
  return id.length > 8 ? id.slice(0, 8) : id
}

function iconFor(kind: TrajectoryEntry['kind']): string {
  switch (kind) {
    case 'turn_start':
      return '▶'
    case 'turn_complete':
      return '■'
    case 'iteration_start':
      return '↻'
    case 'tool_calls':
    case 'tool_result':
      return '🔧'
    case 'delegate':
      return '↳'
    case 'delegate_complete':
      return '✓'
    case 'paradigm_switch':
      return '⇄'
    case 'plan_revision':
      return '📋'
    case 'working_state':
      return '◈'
    case 'token_usage':
      return '∑'
    case 'context_accounting':
      return '☰'
    case 'tools_added':
      return '＋'
    default:
      return '·'
  }
}

/** A horizontal bar visualizing per-iteration timing within a turn. Each
 *  `iteration_start` entry becomes a segment whose width is proportional to
 *  the ms gap to the next iteration boundary (or turn end). */
function TimingBar({ entries, totalMs }: { entries: TrajectoryEntry[]; totalMs: number }) {
  const iters = entries.filter((e) => e.kind === 'iteration_start')
  if (iters.length === 0 || totalMs <= 0) return null
  const segments = iters.map((it, i) => {
    const nextMs = i + 1 < iters.length ? iters[i + 1].ms ?? totalMs : totalMs
    const dur = Math.max(0, (nextMs - (it.ms ?? 0)))
    return { seq: it.seq, dur }
  })
  return (
    <div className={styles.timingBar} title={`turn duration ${totalMs}ms`}>
      {segments.map((s) => (
        <div
          key={s.seq}
          className={styles.timingSeg}
          style={{ width: `${Math.max(2, (s.dur / totalMs) * 100)}%` }}
          title={`${s.dur}ms`}
        />
      ))}
    </div>
  )
}
