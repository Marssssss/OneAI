// ScenarioPicker — modal listing the scenario library (local presets +
// server customs). Picking one either opens the topic intake (when the
// scenario has `topic_fields`) or starts it directly. Mirrors macOS's
// "new conversation from scenario" entry. Presets and customs render the
// same; only the editor treats presets as read-only.

import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { BusScenario } from '../rpc/types'
import type { ScenarioEntry } from './scenarioStore'
import { Modal } from './Modal'
import mBtn from './Modal.module.css'
import styles from './ScenarioPicker.module.css'

interface ScenarioPickerProps {
  entries: ScenarioEntry[]
  onPick: (scenario: BusScenario) => void
  onEdit: (scenario: BusScenario) => void
  onNew: () => void
  onClose: () => void
}

export function ScenarioPicker({
  entries,
  onPick,
  onEdit,
  onNew,
  onClose,
}: ScenarioPickerProps): ReactNode {
  const { t } = useLocale()
  return (
    <Modal title={t('scenario.pick')} onClose={onClose} width={560}>
      <div className={styles.list}>
        {entries.length === 0 && (
          <div className={styles.empty}>{t('scenario.empty')}</div>
        )}
        {entries.map((e) => (
          <div className={styles.row} key={e.scenario.id}>
            <button
              className={styles.pickBtn}
              onClick={() => onPick(e.scenario)}
            >
              <span className={styles.icon} aria-hidden>
                {e.scenario.icon ?? '◆'}
              </span>
              <span className={styles.meta}>
                <span className={styles.name}>{e.scenario.name}</span>
                <span className={styles.desc}>
                  {memberLine(e.scenario)}
                </span>
              </span>
              {e.isPreset && <span className={styles.badge}>{t('scenario.preset')}</span>}
            </button>
            <button
              className={styles.editBtn}
              onClick={() => onEdit(e.scenario)}
              title={e.isPreset ? t('scenario.view') : t('scenario.edit')}
              aria-label={t('scenario.edit')}
            >
              {e.isPreset ? '👁️' : '✏️'}
            </button>
          </div>
        ))}
      </div>
      <div className={styles.actions}>
        <button className={`${mBtn.btn} ${mBtn.btnPrimary}`} onClick={onNew}>
          {t('scenario.new')}
        </button>
      </div>
    </Modal>
  )
}

function memberLine(scenario: BusScenario): string {
  const names = scenario.members.map((m) => m.name).join(' · ')
  const policy = scenario.turn_policy
  return `${names} · ${policy}`
}
