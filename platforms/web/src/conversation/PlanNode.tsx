// PlanNode — the live plan checklist. The projection seeds one `plan` node per
// turn (keyed `plan:<turn_id>`) and updates it in place on each `PlanUpdate`
// yield, so the planner's revisions render as the steps' statuses change.

import { memo } from 'react'
import type { ReactNode } from 'react'
import type { PlanStep, PlanStepStatus } from '../rpc/types'
import { useLocale } from '../i18n'
import styles from './PlanNode.module.css'

interface PlanNodeProps {
  steps: PlanStep[]
  revision?: number
}

const STATUS_GLYPH: Record<PlanStepStatus, string> = {
  pending: '○',
  in_progress: '◐',
  completed: '●',
  failed: '✕',
}

export const PlanNode = memo(function PlanNode({
  steps,
  revision,
}: PlanNodeProps): ReactNode {
  const { t } = useLocale()
  if (steps.length === 0) return null
  const done = steps.filter((s) => s.status === 'completed').length
  return (
    <div className={styles.card}>
      <div className={styles.header}>
        <span className={styles.title}>{t('plan.steps')}</span>
        <span className={styles.meta}>
          {done}/{steps.length}
          {typeof revision === 'number' ? ` · r${revision}` : ''}
        </span>
      </div>
      <ol className={styles.list}>
        {steps.map((s) => (
          <li key={s.id} className={`${styles.item} ${styles[`s_${s.status ?? 'pending'}`]}`}>
            <span className={styles.glyph}>{STATUS_GLYPH[s.status ?? 'pending']}</span>
            <span className={styles.desc}>
              {s.status === 'in_progress' && s.active_form ? s.active_form : s.description}
            </span>
          </li>
        ))}
      </ol>
    </div>
  )
})
