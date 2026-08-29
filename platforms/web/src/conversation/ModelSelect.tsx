// ModelSelect — the composer's model switcher. It lists only the models served
// by the ACTIVE provider (the one chosen in Settings); picking a model calls
// `provider/set_model` for that provider (persisted + hot-swapped, next turn).
// Provider selection itself lives in Settings — this control does NOT switch
// providers (issue #41 follow-up).

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
  const [models, setModels] = useState<string[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const active = snap.providers.find((p) => p.active) ?? snap.providers[0] ?? null

  // Fetch the active provider's model list whenever the popover opens.
  useEffect(() => {
    if (!open || active === null) return
    let cancelled = false
    setLoading(true)
    setError(null)
    void store.providerModels({ name: active.name }).then((res) => {
      if (cancelled) return
      setLoading(false)
      if (res.ok) {
        setModels(res.models)
      } else {
        setModels([])
        setError(res.error ?? 'provider/models failed')
      }
    })
    return () => {
      cancelled = true
    }
  }, [open, active, store])

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

  const pick = async (model: string) => {
    if (active === null) return
    setOpen(false)
    await store.providerSetModel(active.name, model)
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
            ? `${active.name}${active.model !== '' ? ' · ' + active.model : ''}`
            : t('modelSelect.none')}
        </span>
        <span className={styles.caret}>{open ? '▴' : '▾'}</span>
      </button>
      {open && (
        <div className={styles.popover}>
          {active === null ? (
            <div className={styles.empty}>{t('modelSelect.none')}</div>
          ) : (
            <>
              <div className={styles.head}>{active.name}</div>
              {loading ? (
                <div className={styles.empty}>{t('modelSelect.loading')}</div>
              ) : error !== null ? (
                <div className={styles.empty}>{error}</div>
              ) : models.length === 0 ? (
                <div className={styles.empty}>{t('modelSelect.noModels')}</div>
              ) : (
                models.map((m) => {
                  const isCurrent = m === active.model
                  return (
                    <button
                      key={m}
                      className={`${styles.item} ${isCurrent ? styles.itemActive : ''}`}
                      onClick={() => void pick(m)}
                    >
                      <span className={styles.itemModel}>{m}</span>
                      {isCurrent && <span className={styles.itemActiveDot}>●</span>}
                    </button>
                  )
                })
              )}
            </>
          )}
        </div>
      )}
    </div>
  )
}
