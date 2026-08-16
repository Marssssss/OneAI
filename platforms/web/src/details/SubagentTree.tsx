// SubagentTree — the sub-agent directory. Pure consumer of the projection's
// `subagents` slice (assembled from `delegate` / `delegate_complete`
// EngineYields). Rendered as a section in the trajectory tab. Shows each
// delegation's kind + task + status (active spinner / done) and, on
// completion, the summary + key findings.

import type { ReactNode } from 'react'
import type { SubagentNode } from '../store/trajectory'
import { useLocale } from '../i18n'
import styles from './SubagentTree.module.css'

interface SubagentTreeProps {
  subagents: SubagentNode[]
}

export function SubagentTree({ subagents }: SubagentTreeProps): ReactNode | null {
  const { t } = useLocale()
  if (subagents.length === 0) return null

  return (
    <section className={styles.root}>
      <div className={styles.label}>{t('subagent.title')}</div>
      <ul className={styles.list}>
        {subagents.map((s) => (
          <li key={s.id} className={styles.item}>
            <span className={`${styles.dot} ${styles[`dot_${s.status}`]}`} aria-hidden />
            <div className={styles.body}>
              <div className={styles.head}>
                <span className={styles.kind}>{kindLabel(s.agentKind)}</span>
                <span className={styles.status}>
                  {s.status === 'active' ? t('subagent.active') : t('subagent.done')}
                </span>
              </div>
              <div className={styles.task} title={s.task}>
                {s.task}
              </div>
              {s.summary !== undefined && (
                <div className={styles.summary}>
                  {s.summary.summary.length > 0 && (
                    <div className={styles.summaryText}>{s.summary.summary}</div>
                  )}
                  {s.summary.keyFindings.length > 0 && (
                    <ul className={styles.findings}>
                      {s.summary.keyFindings.map((f, i) => (
                        <li key={i}>{f}</li>
                      ))}
                    </ul>
                  )}
                  <div className={styles.tokens}>
                    {s.summary.tokensUsed.toLocaleString()} {t('subagent.tokens')}
                    {s.summary.budgetExceeded && ` · ${t('subagent.budget')}`}
                  </div>
                </div>
              )}
            </div>
          </li>
        ))}
      </ul>
    </section>
  )
}

function kindLabel(kind: SubagentNode['agentKind']): string {
  if (typeof kind === 'string') return kind
  return `Custom:${kind.Custom}`
}
