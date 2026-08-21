// EffortChip — the composer's thinking-effort picker. A popover (mirrors
// ModelSelect) listing the 5 tiers (off/low/medium/high/max); picking one calls
// `settingsStore.setThinkingEffort`, which persists the tier via `thinking/set`
// and live-patches the snapshot so the chip reacts instantly.
//
// Lives in the composer's chips row (above the textarea), moved here from the
// Settings modal per issue #36 — effort is a per-turn knob the user reaches
// for while composing, so it belongs beside the mode chip + model switcher,
// not buried in a settings sheet. The Settings modal no longer renders it.

import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { SettingsStore } from '../settings/settingsStore'
import { useSettings } from '../settings/settingsStore'
import type { ThinkingEffort } from '../rpc/types'
import styles from './EffortChip.module.css'

interface EffortChipProps {
  store: SettingsStore
}

const TIERS: ThinkingEffort[] = ['off', 'low', 'medium', 'high', 'max']

export function EffortChip({ store }: EffortChipProps): ReactNode {
  const { t } = useLocale()
  const snap = useSettings(store)
  const [open, setOpen] = useState(false)
  const wrapRef = useRef<HTMLDivElement>(null)

  // The persisted thinking-effort tier; defaults to "medium" when no store is
  // wired (the snapshot omits the field) — matches the Rust default.
  const effort: ThinkingEffort =
    (snap.config?.thinking_effort as ThinkingEffort | undefined) ?? 'medium'

  const tierLabel = (e: ThinkingEffort): string =>
    (
      {
        off: t('settings.thinkingOff'),
        low: t('settings.thinkingLow'),
        medium: t('settings.thinkingMedium'),
        high: t('settings.thinkingHigh'),
        max: t('settings.thinkingMax'),
      } as Record<ThinkingEffort, string>
    )[e]

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

  const pick = async (e: ThinkingEffort) => {
    setOpen(false)
    await store.setThinkingEffort(e)
  }

  return (
    <div className={styles.wrap} ref={wrapRef}>
      <button
        className={styles.trigger}
        onClick={() => setOpen((v) => !v)}
        title={t('settings.thinkingEffort')}
        aria-label={t('settings.thinkingEffort')}
      >
        <span className={styles.glyph} aria-hidden>🧠</span>
        <span className={styles.label}>{tierLabel(effort)}</span>
        <span className={styles.caret}>{open ? '▴' : '▾'}</span>
      </button>
      {open && (
        <div className={styles.popover} role="listbox" aria-label={t('settings.thinkingEffort')}>
          {TIERS.map((e) => (
            <button
              key={e}
              className={`${styles.item} ${e === effort ? styles.itemActive : ''}`}
              onClick={() => void pick(e)}
              role="option"
              aria-selected={e === effort}
            >
              <span className={styles.itemLabel}>{tierLabel(e)}</span>
              {e === effort && <span className={styles.itemActiveDot}>●</span>}
            </button>
          ))}
          <div className={styles.hint}>{t('settings.thinkingEffortHint')}</div>
        </div>
      )}
    </div>
  )
}
