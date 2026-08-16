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

import { useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import type { BusParadigmKind, ContentBlock } from '../rpc/types'
import { useLocale } from '../i18n'
import styles from './Composer.module.css'

export type SlashCommand =
  | 'plan'
  | 'clear'
  | 'compact'
  | 'scenario'
  | 'newScenario'
  | 'editScenario'
  | 'trajectory'
  | 'settings'
  | 'skills'
  | 'domainpack'

/** A staged image attachment: the wire block (sent in turn/run `content`) +
 * a `data:` URL preview for the thumbnail. */
interface StagedImage {
  block: ContentBlock
  preview: string
  name: string
}

interface ComposerProps {
  placeholder: string
  sendLabel: string
  stopLabel: string
  turnActive: boolean
  paradigm: BusParadigmKind
  planMode: boolean
  /** §W4 attachments — disabled in group-chat mode (its bus directive is
   * plain-text only). When false the attach button + drop zone hide. */
  attachmentsEnabled: boolean
  onSend: (text: string, images?: ContentBlock[]) => void
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

/** Read one image File into a `ContentBlock::image` (base64 data, no data-URI
 * prefix — the engine's `#[serde(with = "base64_bytes")]` expects raw base64)
 * + a `data:` URL for the thumbnail preview. Resolves null for non-images. */
function readImageFile(file: File): Promise<StagedImage | null> {
  return new Promise((resolve) => {
    if (!file.type.startsWith('image/')) {
      resolve(null)
      return
    }
    const reader = new FileReader()
    reader.onload = () => {
      const dataUrl = typeof reader.result === 'string' ? reader.result : ''
      const comma = dataUrl.indexOf(',')
      const b64 = comma >= 0 ? dataUrl.slice(comma + 1) : ''
      if (b64.length === 0) {
        resolve(null)
        return
      }
      resolve({
        block: { type: 'image', mime_type: file.type, data: b64 },
        preview: dataUrl,
        name: file.name,
      })
    }
    reader.onerror = () => resolve(null)
    reader.readAsDataURL(file)
  })
}

export function Composer({
  placeholder,
  sendLabel,
  stopLabel,
  turnActive,
  paradigm,
  planMode,
  attachmentsEnabled,
  onSend,
  onStop,
  onTogglePlan,
  onSlash,
}: ComposerProps): ReactNode {
  const { t } = useLocale()
  const [text, setText] = useState('')
  const [images, setImages] = useState<StagedImage[]>([])
  const [dragOver, setDragOver] = useState(false)
  const fileInputRef = useRef<HTMLInputElement>(null)

  const commands = useMemo(
    () => [
      { cmd: 'plan' as const, label: '/plan', desc: t('command.plan') },
      { cmd: 'clear' as const, label: '/clear', desc: t('command.clear') },
      { cmd: 'compact' as const, label: '/compact', desc: t('command.compact') },
      { cmd: 'scenario' as const, label: '/scenario', desc: t('command.scenario') },
      { cmd: 'newScenario' as const, label: '/new-scenario', desc: t('command.newScenario') },
      { cmd: 'editScenario' as const, label: '/edit-scenario', desc: t('command.editScenario') },
      { cmd: 'trajectory' as const, label: '/trajectory', desc: t('command.trajectory') },
      { cmd: 'settings' as const, label: '/settings', desc: t('command.settings') },
      { cmd: 'skills' as const, label: '/skills', desc: t('command.skills') },
      { cmd: 'domainpack' as const, label: '/domainpack', desc: t('command.domainpack') },
    ],
    [t],
  )

  const isSlash = text.startsWith('/')
  const filtered = isSlash
    ? commands.filter((c) => c.label.startsWith(text))
    : []

  const runSlash = (raw: string): boolean => {
    const match = commands.find((c) => c.label === raw.trim())
    if (match === undefined) return false
    onSlash(match.cmd)
    setText('')
    return true
  }

  /** Stage a batch of image files (drag/drop/paste/file-picker all funnel here). */
  const stageFiles = async (files: FileList | File[]) => {
    const arr = Array.from(files)
    const staged = await Promise.all(arr.map(readImageFile))
    const kept = staged.filter((s): s is StagedImage => s !== null)
    if (kept.length > 0) setImages((prev) => [...prev, ...kept])
  }

  const submit = () => {
    const trimmed = text.trim()
    const hasText = trimmed.length > 0
    const hasImages = images.length > 0
    if (!hasText && !hasImages) return
    if (turnActive) return
    if (hasText && trimmed.startsWith('/') && runSlash(trimmed)) return
    onSend(trimmed, hasImages ? images.map((i) => i.block) : undefined)
    setText('')
    setImages([])
  }

  return (
    <div
      className={`${styles.wrap} ${dragOver ? styles.wrapDragOver : ''}`}
      onDragOver={(e) => {
        if (!attachmentsEnabled) return
        e.preventDefault()
        setDragOver(true)
      }}
      onDragLeave={() => setDragOver(false)}
      onDrop={(e) => {
        if (!attachmentsEnabled) return
        e.preventDefault()
        setDragOver(false)
        if (e.dataTransfer.files.length > 0) void stageFiles(e.dataTransfer.files)
      }}
    >
      {images.length > 0 && (
        <div className={styles.attachmentRail}>
          {images.map((img, i) => (
            <div className={styles.attachmentChip} key={`${img.name}-${i}`}>
              <img className={styles.attachmentThumb} src={img.preview} alt={img.name} />
              <button
                className={styles.attachmentRemove}
                onClick={() => setImages((prev) => prev.filter((_, j) => j !== i))}
                title="remove"
                aria-label="remove attachment"
              >
                ✕
              </button>
            </div>
          ))}
        </div>
      )}

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
        {attachmentsEnabled && (
          <button
            className={styles.chip}
            onClick={() => fileInputRef.current?.click()}
            title="attach image"
            aria-label="attach image"
          >
            <span className={styles.chipLabel}>📎</span>
          </button>
        )}
        <input
          ref={fileInputRef}
          type="file"
          accept="image/*"
          multiple
          className={styles.fileInput}
          onChange={(e) => {
            if (e.target.files !== null && e.target.files.length > 0) {
              void stageFiles(e.target.files)
            }
            // Reset so picking the same file twice re-fires onChange.
            e.target.value = ''
          }}
        />
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
          onPaste={(e) => {
            if (!attachmentsEnabled) return
            const files = Array.from(e.clipboardData.items)
              .map((it) => it.getAsFile())
              .filter((f): f is File => f !== null && f.type.startsWith('image/'))
            if (files.length > 0) {
              e.preventDefault()
              void stageFiles(files)
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
            disabled={text.trim().length === 0 && images.length === 0}
            title={sendLabel}
          >
            ↑
          </button>
        )}
      </div>
    </div>
  )
}
