// Composer — the message input + send/stop button + plan-mode chip + slash
// command palette.
//
//  - Enter sends, Shift+Enter for a newline.
//  - While a turn is in flight the button becomes Stop → turn/cancel.
//  - Plan-mode chip reflects the App-owned `planMode` flag (toggled via
//    config/update) and the live `paradigm` from paradigm_switch yields.
//  - Slash commands: typing `/` surfaces a candidate popup; Enter on a known
//    `/command` dispatches it instead of sending a message. W2 ships the three
//    engine-backed commands; /model / /permission need W4 RPCs.

import { useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import type { BusParadigmKind } from '../rpc/types'
import { useLocale } from '../i18n'
import styles from './Composer.module.css'

export type SlashCommand = 'plan' | 'clear' | 'compact'

interface ComposerProps {
  placeholder: string
  sendLabel: string
  stopLabel: string
  turnActive: boolean
  paradigm: BusParadigmKind
  planMode: boolean
  onSend: (text: string) => void
  onStop: () => void
  onTogglePlan: () => void
  onSlash: (cmd: SlashCommand) => void
}

const PARADIGM_GLYPH: Record<BusParadigmKind, string> = {
  plan: ' ◎',
  re_act: '',
  reflect: ' ↻',
  explore: ' ⌕',
}

export function Composer({
  placeholder,
  sendLabel,
  stopLabel,
  turnActive,
  paradigm,
  planMode,
  onSend,
  onStop,
  onTogglePlan,
  onSlash,
}: ComposerProps): ReactNode {
  const { t } = useLocale()
  const [text, setText] = useState('')

  const commands = useMemo(
    () => [
      { cmd: 'plan' as const, label: '/plan', desc: t('command.plan') },
      { cmd: 'clear' as const, label: '/clear', desc: t('command.clear') },
      { cmd: 'compact' as const, label: '/compact', desc: t('command.compact') },
    ],
    [t],
  )

  const isSlash = text.startsWith('/')
  const filtered = isSlash
    ? commands.filter((c) => c.label.startsWith(text))
    : []

  const runSlash = (raw: string): boolean => {
    const name = raw.slice(1).trim()
    const match = commands.find((c) => c.cmd === name)
    if (match === undefined) return false
    onSlash(match.cmd)
    setText('')
    return true
  }

  const submit = () => {
    const trimmed = text.trim()
    if (trimmed.length === 0) return
    if (turnActive) return
    if (trimmed.startsWith('/') && runSlash(trimmed)) return
    onSend(trimmed)
    setText('')
  }

  return (
    <div className={styles.wrap}>
      <div className={styles.chips}>
        <button
          className={`${styles.chip} ${planMode ? styles.chipOn : ''}`}
          onClick={onTogglePlan}
          title={t('command.plan')}
        >
          <span className={styles.chipLabel}>{t('plan.mode')}</span>
          <span className={styles.chipState}>{planMode ? t('plan.on') : t('plan.off')}</span>
          {paradigm !== 're_act' && (
            <span className={styles.chipParadigm}>{paradigm}{PARADIGM_GLYPH[paradigm]}</span>
          )}
        </button>
      </div>

      <div className={styles.inputRow}>
        {isSlash && filtered.length > 0 && (
          <div className={styles.slashPopup}>
            {filtered.map((c) => (
              <button
                key={c.cmd}
                className={styles.slashItem}
                onClick={() => {
                  onSlash(c.cmd)
                  setText('')
                }}
              >
                <span className={styles.slashLabel}>{c.label}</span>
                <span className={styles.slashDesc}>{c.desc}</span>
              </button>
            ))}
          </div>
        )}
        <textarea
          className={styles.input}
          placeholder={placeholder}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              submit()
            }
          }}
          rows={1}
        />
        {turnActive ? (
          <button className={styles.stopBtn} onClick={onStop} title={stopLabel}>
            ◼
          </button>
        ) : (
          <button
            className={styles.button}
            onClick={submit}
            disabled={text.trim().length === 0}
            title={sendLabel}
          >
            ↑
          </button>
        )}
      </div>
    </div>
  )
}
