// MetricsBar — the composer strip showing session aggregates: turns/steps,
// first-token latency + throughput, cache hit %, in/out tokens. Pure consumer
// of the projection's `metrics` snapshot — no engine data of its own.
//
// Renders inline in the composer chips row (between attach and model). Hides
// itself on a fresh session (no turns yet) so the empty-state composer stays
// clean.

import { memo } from 'react'
import type { ReactNode } from 'react'
import type { SessionMetrics } from '../store/projection'
import styles from './MetricsBar.module.css'

interface MetricsBarProps {
  metrics: SessionMetrics
}

export const MetricsBar = memo(function MetricsBar({ metrics }: MetricsBarProps): ReactNode {
  // Hide on a fresh session — nothing to show yet.
  if (metrics.turns === 0) return null

  const firstTokenS = metrics.firstTokenMs !== null ? metrics.firstTokenMs / 1000 : null
  const seconds = metrics.totalDurationMs / 1000
  const tokPerS = seconds > 0 ? metrics.totalCompletion / seconds : null
  // Cache hit is the LATEST inference step's rate (projection computes it from
  // the most recent token_usage record), not a session cumulative — token
  // counts below stay cumulative (total spend).
  const cacheHit = metrics.cacheHitPct
  // Current context size = the latest inference step's total input footprint
  // (prompt_tokens). Tells the user how full the context window is, so they
  // can decide whether to /compact or start a fresh session (issue #35).
  const contextTokens = metrics.contextTokens

  return (
    <div className={styles.strip} title={undefined}>
      <span className={styles.group}>
        <span className={styles.value}>{metrics.turns}</span>
        <span className={styles.unit}>轮</span>
        <span className={styles.sep}>·</span>
        <span className={styles.value}>{metrics.steps}</span>
        <span className={styles.unit}>步</span>
      </span>
      {(firstTokenS !== null || tokPerS !== null) && (
        <span className={styles.group}>
          {firstTokenS !== null && (
            <>
              首 token 平均 <span className={styles.value}>{firstTokenS.toFixed(1)}s</span>
            </>
          )}
          {tokPerS !== null && (
            <>
              <span className={styles.sep}>·</span>
              <span className={styles.value}>{Math.round(tokPerS)}</span>
              <span className={styles.unit}> tok/s</span>
            </>
          )}
        </span>
      )}
      {cacheHit !== null && (
        <span className={styles.group}>
          缓存命中 <span className={styles.value}>{Math.round(cacheHit)}%</span>
        </span>
      )}
      {contextTokens !== null && (
        <span className={styles.group}>
          上下文 <span className={styles.value}>{fmtK(contextTokens)}</span>
        </span>
      )}
      <span className={styles.group}>
        输入 <span className={styles.value}>{fmtK(metrics.totalPrompt)}</span>
        <span className={styles.sep}>·</span>
        输出 <span className={styles.value}>{fmtK(metrics.totalCompletion)}</span>
      </span>
    </div>
  )
})

/** Format a token count with a K suffix when ≥ 1000 (one decimal). */
function fmtK(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}K`
  return String(n)
}
