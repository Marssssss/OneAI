// DomainPackModal — the standalone domain-pack viewer, opened by the
// `/domainpack` slash command. Lifted out of the settings modal per the W4
// reorg. Read-only list of builtin packs + the active marker. Switching the
// active pack requires restarting the app-server with `--domain` (the
// architecture has no live hot-swap path — `App.domain_pack` is an immutable
// `Arc`), so it's surfaced honestly here, not faked.

import { useEffect } from 'react'
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

  useEffect(() => {
    void store.refresh()
  }, [store])

  const dp = snap.domainPacks

  return (
    <Modal title={t('settings.domainPacks')} width={640} onClose={onClose}>
      <div className={styles.stack}>
        {dp === null ? (
          <div className={styles.empty}>{t('settings.offline')}</div>
        ) : (
          <>
            {dp.available.map((p) => (
              <div
                key={p.name}
                className={`${styles.card} ${dp.active === p.name ? styles.cardActive : ''}`}
              >
                <div className={styles.packHead}>
                  <code className={styles.code}>{p.name}</code>
                  {dp.active === p.name && (
                    <span className={styles.activeBadge}>{t('settings.active')}</span>
                  )}
                </div>
                {p.description !== undefined && (
                  <div className={styles.packDesc}>{p.description}</div>
                )}
              </div>
            ))}
            <div className={styles.restartHint}>{t('settings.restartHintDomain')}</div>
          </>
        )}
      </div>
    </Modal>
  )
}
