// GoalBar — the composer-anchored dock that surfaces the live working-state
// goal + step progress. Pure consumer of the projection's `working` slice
// (rebuilt from the `working_state` EngineYield stream — goal/task events,
// step_added / step_status_changed). Hidden when no goal has been declared
// for the session. Mirrors the macOS goal-indicator dock.

import type { ReactNode } from 'react'
import type { WorkingProjection } from '../store/trajectory'
import { stepProgress } from '../store/trajectory'
import { useLocale } from '../i18n'
import styles from './GoalBar.module.css'

interface GoalBarProps {
  working: WorkingProjection
}

export function GoalBar({ working }: GoalBarProps): ReactNode | null {
  const { t } = useLocale()
  if (working.goal === null) return null
  const prog = stepProgress(working.steps)
  const openBlockers = working.blockers.filter((b) => b.status === 'open').length

  return (
    <div className={styles.root}>
      <div className={styles.head}>
        <span className={styles.icon}>◎</span>
        <span className={styles.goal} title={working.goal}>
          {working.goal}
        </span>
        {working.steps.length > 0 && (
          <span className={styles.progress}>
            {prog.done}/{prog.total}
          </span>
        )}
        {openBlockers > 0 && (
          <span className={styles.blockers} title={t('goal.blockers')}>
            ⚠ {openBlockers}
          </span>
        )}
      </div>
      {working.steps.length > 0 && (
        <div className={styles.steps}>
          {working.steps.map((s) => (
            <div
              key={s.id}
              className={`${styles.step} ${styles[`step_${s.status.toLowerCase()}`]}`}
            >
              <span className={styles.stepDot} aria-hidden />
              <span className={styles.stepText} title={s.description}>
                {s.active_form ?? s.description}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
