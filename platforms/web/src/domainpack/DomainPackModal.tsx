// DomainPackModal — the standalone domain-pack viewer + live switcher, opened
// by the `/domainpack` slash command. Lifted out of the settings modal per the
// W4 reorg. Lists builtin packs + the active marker; clicking a pack hot-swaps
// the active DomainPack at runtime (backed by `domainpack/switch`), which takes
// effect on the next turn — no app-server restart.

import { useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import { Modal } from '../scenario/Modal'
import { useLocale } from '../i18n'
import type { SettingsStore } from '../settings/settingsStore'
import { useSettings } from '../settings/settingsStore'
import styles from '../settings/SettingsRoot.module.css'

interface DomainPackModalProps {
  store: SettingsStore
  onClose: () => void
}

export function DomainPackModal({ store, onClose }: DomainPackModalProps): ReactNode {
  const { t } = useLocale()
  const snap = useSettings(store)
  const [switching, setSwitching] = useState<string | null>(null)

  useEffect(() => {
    void store.refresh()
  }, [store])

  const dp = snap.domainPacks

  const switchTo = async (name: string): Promise<void> => {
    if (switching !== null || dp?.active === name) return
    setSwitching(name)
    try {
      await store.domainpackSwitch(name)
    } finally {
      setSwitching(null)
    }
  }

  return (
    <Modal title={t('settings.domainPacks')} width={640} onClose={onClose}>
      <div className={styles.stack}>
        {dp === null ? (
          <div className={styles.empty}>{t('settings.offline')}</div>
        ) : (
          <>
            {dp.available.map((p) => {
              const isActive = dp.active === p.name
              return (
                <div
                  key={p.name}
                  role="button"
                  tabIndex={0}
                  className={`${styles.card} ${isActive ? styles.cardActive : ''}`}
                  onClick={() => void switchTo(p.name)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault()
                      void switchTo(p.name)
                    }
                  }}
                >
                  <div className={styles.packHead}>
                    <code className={styles.code}>{p.name}</code>
                    {isActive && (
                      <span className={styles.activeBadge}>{t('settings.active')}</span>
                    )}
                    {switching === p.name && (
                      <span className={styles.activeBadge}>{t('settings.switching') ?? '…'}</span>
                    )}
                  </div>
                  {p.description !== undefined && (
                    <div className={styles.packDesc}>{p.description}</div>
                  )}
                </div>
              )
            })}
            <div className={styles.restartHint}>{t('settings.restartHintDomain')}</div>
          </>
        )}
      </div>
    </Modal>
  )
}
