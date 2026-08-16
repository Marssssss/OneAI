// TopicIntake — collects the scenario's `topic_fields` values before launch.
// Mirrors macOS `TopicIntakeView`. `visible_to` controls which members see a
// field's value in their system prompt; the compile step bakes that
// background per-member, so the intake form just collects raw values and is
// blind to visibility (the marker `◈` flags fields only some members see).
// Submitting compiles + starts the scenario; cancel returns to the picker.

import { useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { BusScenario } from '../rpc/types'
import { Modal } from './Modal'
import styles from './TopicIntake.module.css'

interface TopicIntakeProps {
  scenario: BusScenario
  onSubmit: (values: Record<string, string>) => void
  onClose: () => void
}

export function TopicIntake({
  scenario,
  onSubmit,
  onClose,
}: TopicIntakeProps): ReactNode {
  const { t } = useLocale()
  const fields = scenario.topic_fields ?? []
  const [values, setValues] = useState<Record<string, string>>(() => {
    const init: Record<string, string> = {}
    for (const f of fields) init[f.id] = ''
    return init
  })

  const anyFilled = useMemo(
    () => Object.values(values).some((v) => v.trim().length > 0),
    [values],
  )

  const submit = () => {
    onSubmit(values)
  }

  return (
    <Modal
      title={`${t('scenario.intake')} · ${scenario.name}`}
      onClose={onClose}
      width={520}
      footer={
        <>
          <button className={styles.secondary} onClick={onClose}>
            {t('scenario.cancel')}
          </button>
          <button
            className={styles.primary}
            onClick={submit}
            disabled={!anyFilled && fields.length > 0}
          >
            {t('scenario.start')}
          </button>
        </>
      }
    >
      {fields.length === 0 ? (
        <div className={styles.hint}>{t('scenario.noTopic')}</div>
      ) : (
        <div className={styles.form}>
          {fields.map((f) => {
            const partial = f.visible_to !== undefined && f.visible_to.length > 0
            return (
              <label className={styles.field} key={f.id}>
                <span className={styles.label}>
                  {f.label}
                  {partial && (
                    <span className={styles.partial} title={t('scenario.partial')}>
                      ◈
                    </span>
                  )}
                </span>
                <input
                  className={styles.input}
                  type="text"
                  placeholder={f.placeholder ?? ''}
                  value={values[f.id]}
                  onChange={(e) =>
                    setValues({ ...values, [f.id]: e.target.value })
                  }
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault()
                      if (anyFilled || fields.length === 0) submit()
                    }
                  }}
                />
              </label>
            )
          })}
          <div className={styles.hint}>{t('scenario.intakeHint')}</div>
        </div>
      )}
    </Modal>
  )
}
