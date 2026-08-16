// SkillsModal — the standalone skills manager, opened by the `/skills` slash
// command. Lifted out of the settings modal per the W4 reorg. Same skill
// lifecycle surface (pin/unpin/archive/restore) via the shared `settingsStore`
// — which is both the settings modal's source AND the skills modal's source,
// so the two stay consistent.

import { useEffect } from 'react'
import type { ReactNode } from 'react'
import { Modal } from '../scenario/Modal'
import { useLocale } from '../i18n'
import type { SettingsStore } from '../settings/settingsStore'
import { useSettings } from '../settings/settingsStore'
import styles from '../settings/SettingsRoot.module.css'

interface SkillsModalProps {
  store: SettingsStore
  onClose: () => void
}

export function SkillsModal({ store, onClose }: SkillsModalProps): ReactNode {
  const { t } = useLocale()
  const snap = useSettings(store)

  useEffect(() => {
    void store.refresh()
  }, [store])

  const skills = [...snap.skills].sort((a, b) => a.name.localeCompare(b.name))

  return (
    <Modal title={t('settings.skills')} width={640} onClose={onClose}>
      <div className={styles.stack}>
        {snap.lastError !== null && (
          <div className={styles.errorBanner}>{snap.lastError}</div>
        )}
        {skills.length === 0 && <div className={styles.empty}>{t('settings.noSkills')}</div>}
        {skills.map((s) => (
          <div key={s.name} className={styles.skillRow}>
            <div className={styles.skillMain}>
              <code className={styles.code}>{s.name}</code>
              {s.description !== undefined && (
                <span className={styles.skillDesc}>{s.description}</span>
              )}
            </div>
            <div className={styles.skillMeta}>
              <span className={`${styles.stateBadge} ${styles[`state_${s.state}`] ?? ''}`}>
                {t(`skill.state.${s.state}`)}
              </span>
              {s.pinned && <span className={styles.pinBadge}>📌</span>}
              <span className={styles.useCount}>{s.use_count}×</span>
            </div>
            <div className={styles.skillActions}>
              <button
                className={styles.miniBtn}
                onClick={() => void store.skillOp(s.pinned ? 'skill/unpin' : 'skill/pin', s.name)}
                disabled={s.state === 'archived'}
              >
                {s.pinned ? t('settings.unpin') : t('settings.pin')}
              </button>
              {s.state === 'archived' ? (
                <button
                  className={styles.miniBtn}
                  onClick={() => void store.skillOp('skill/restore', s.name)}
                >
                  {t('settings.restore')}
                </button>
              ) : (
                <button
                  className={`${styles.miniBtn} ${styles.danger}`}
                  onClick={() => void store.skillOp('skill/archive', s.name)}
                >
                  {t('settings.archive')}
                </button>
              )}
            </div>
          </div>
        ))}
      </div>
    </Modal>
  )
}
