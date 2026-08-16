// ModelSelect — the composer's provider/model switcher. A popover listing the
// configured providers (from `provider/list`); picking one calls
// `provider/set_active`, which live-switches the pool's `active_index` (takes
// effect on the next turn — no `App.provider` swap, no restart). Active
// provider marked. Mirrors the deepseek-harness model-selection menu.

import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { SettingsStore } from '../settings/settingsStore'
import { useSettings } from '../settings/settingsStore'
import styles from './ModelSelect.module.css'

interface ModelSelectProps {
  store: SettingsStore
}

export function ModelSelect({ store }: ModelSelectProps): ReactNode {
  const { t } = useLocale()
  const snap = useSettings(store)
  const [open, setOpen] = useState(false)
  const wrapRef = useRef<HTMLDivElement>(null)

  // Refresh the provider list when the popover opens (catch live add/delete).
  useEffect(() => {
    if (open) void store.refresh()
  }, [open, store])

  // Close on outside click.
  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (wrapRef.current !== null && !wrapRef.current.contains(e.target as Node)) {
        setOpen(false)
      }
    }
    window.addEventListener('mousedown', onDown)
    return () => window.removeEventListener('mousedown', onDown)
  }, [open])

  const active = snap.providers.find((p) => p.active) ?? snap.providers[0] ?? null

  const pick = async (name: string) => {
    setOpen(false)
    await store.providerSetActive(name)
  }

  return (
    <div className={styles.wrap} ref={wrapRef}>
      <button
        className={styles.trigger}
        onClick={() => setOpen((v) => !v)}
        disabled={snap.providers.length === 0}
        title={t('modelSelect.title')}
      >
        <span className={styles.glyph}>⚙</span>
        <span className={styles.label}>
          {active !== null
            ? `${active.kind}${active.model !== '' ? ' · ' + active.model : ''}`
            : t('modelSelect.none')}
        </span>
        <span className={styles.caret}>{open ? '▴' : '▾'}</span>
      </button>
      {open && (
        <div className={styles.popover}>
          {snap.providers.length === 0 ? (
            <div className={styles.empty}>{t('modelSelect.none')}</div>
          ) : (
            snap.providers.map((p, i) => (
              <button
                key={i}
                className={`${styles.item} ${p.active ? styles.itemActive : ''}`}
                onClick={() => void pick(p.kind)}
              >
                <span className={styles.itemKind}>{p.kind}</span>
                {p.model !== '' && <span className={styles.itemModel}>{p.model}</span>}
                {p.active && <span className={styles.itemActiveDot}>●</span>}
              </button>
            ))
          )}
        </div>
      )}
    </div>
  )
}
