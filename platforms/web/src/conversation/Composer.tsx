// Composer — the message input + send/stop button + mode-cycler chip +
// slash command palette + provider/model switcher + session metrics strip.
//
//  - Enter sends, Shift+Enter for a newline.
//  - While a turn is in flight the button becomes Stop → turn/cancel.
//  - Mode chip cycles Normal → Auto → Plan (mirrors the TUI InteractionMode);
//    hover the chip for a tooltip describing the current mode. The live
//    `paradigm` from paradigm_switch yields surfaces a tag when the model
//    auto-switched to reflect/explore.
//  - The provider/model switcher lives at the row's right edge (moved here
//    from the header so it sits beside the mode + attach controls).
//  - The metrics strip (turns/steps/first-token/tok-s/cache/in-out) sits
//    between the attach button and the model switcher.
//  - Slash commands (issue #39 — TUI-aligned scope): typing `/` surfaces a
//    grouped candidate popup (↑↓ select · Tab fill · Enter run); a command
//    with subcommands (`/session`, `/init`) opens a second level once fully
//    typed + a space. Enter on a known `/command` dispatches it instead of
//    sending a message; an unknown `/…` surfaces a note (never sent raw).

import { useRef, useLayoutEffect, useState } from 'react'
import type { ReactNode } from 'react'
import type { BusParadigmKind, ContentBlock } from '../rpc/types'
import type { SessionMetrics } from '../store/projection'
import type { SettingsStore } from '../settings/settingsStore'
import { useLocale } from '../i18n'
import { ModelSelect } from './ModelSelect'
import { EffortChip } from './EffortChip'
import { MetricsBar } from './MetricsBar'
import { WorkspaceDropdown } from '../workspace/WorkspaceDropdown'
import { Tooltip } from '../components/Tooltip'
import {
  getSuggestions,
  parseSlash,
  type CommandGroup,
  type SlashInvocation,
} from './slashCommands'
import styles from './Composer.module.css'

/** The 3-state interaction mode — mirrors the TUI's `InteractionMode`
 * (Normal → AutoAccept → Plan → Normal). The composer cycles on click. */
export type InteractionMode = 'normal' | 'auto' | 'plan'

export const MODE_ORDER: InteractionMode[] = ['normal', 'auto', 'plan']

export function nextMode(m: InteractionMode): InteractionMode {
  const i = MODE_ORDER.indexOf(m)
  return MODE_ORDER[(i + 1) % MODE_ORDER.length]
}

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
  mode: InteractionMode
  metrics: SessionMetrics
  settingsStore: SettingsStore
  /** §W4 attachments — disabled in group-chat mode (its bus directive is
   * plain-text only). When false the attach button + drop zone hide. */
  attachmentsEnabled: boolean
  /** The workspace (working-directory) chip is hidden entirely while a
   *  scenario is active — scenarios don't bind a host working directory, so
   *  the picker has nothing to offer and showing it implies a switch that
   *  can't take effect mid-scenario. */
  workspaceEnabled: boolean
  /** The workspace (working-directory) chip label, or null when no workspace
   *  is selected. Sits left of the mode chip. Clicking routes through
   *  `onWorkspaceClick` — the caller decides dropdown-vs-blocked (the latter
   *  when a conversation is mid-flight). */
  workspaceLabel: string | null
  onSend: (text: string, images?: ContentBlock[]) => void
  onStop: () => void
  onCycleMode: () => void
  /** Dispatch a parsed, known slash command (issue #39 registry). */
  onSlash: (invocation: SlashInvocation) => void
  /** A `/…` line whose first token matches no registered command — App
   *  surfaces an "unknown command" note (TUI parity: never sent to the model). */
  onUnknownSlash: (label: string) => void
  onWorkspaceClick: () => void
  /** Workspace dropdown (popover) state — rendered inside the chips row when
   *  open, anchored under the chip. The dropdown self-closes via onClose
   *  before firing onSelect. */
  workspaceDropdownOpen: boolean
  onCloseWorkspaceDropdown: () => void
  onSelectWorkspace: (path: string) => void
  /** Open the native OS folder picker (App owns the RPC). */
  onAddWorkspace: () => void
}

const PARADIGM_GLYPH: Record<BusParadigmKind, string> = {
  plan: ' ◎',
  re_act: '',
  reflect: ' ↻',
  explore: ' ⌕',
}

const MODE_META: Record<InteractionMode, { glyph: string; labelKey: string; tipKey: string }> = {
  normal: { glyph: '', labelKey: 'mode.normal', tipKey: 'mode.normal.tip' },
  auto: { glyph: '⚡', labelKey: 'mode.auto', tipKey: 'mode.auto.tip' },
  plan: { glyph: '📋', labelKey: 'mode.plan', tipKey: 'mode.plan.tip' },
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
  mode,
  metrics,
  settingsStore,
  attachmentsEnabled,
  workspaceEnabled,
  workspaceLabel,
  onSend,
  onStop,
  onCycleMode,
  onSlash,
  onUnknownSlash,
  onWorkspaceClick,
  workspaceDropdownOpen,
  onCloseWorkspaceDropdown,
  onSelectWorkspace,
  onAddWorkspace,
}: ComposerProps): ReactNode {
  const { t } = useLocale()
  const [text, setText] = useState('')
  const [images, setImages] = useState<StagedImage[]>([])
  const [dragOver, setDragOver] = useState(false)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const taRef = useRef<HTMLTextAreaElement>(null)

  /** Auto-grow the textarea with its content, capping at MAX_ROWS visible lines
   *  (then it scrolls internally). Re-runs on every text/attachment mutation
   *  and on mount. Reads computed line-height + vertical padding from the
   *  live element so it stays correct if the theme/font-size changes — no
   *  hardcoded px. */
  const MAX_ROWS = 5
  useLayoutEffect(() => {
    const ta = taRef.current
    if (ta === null) return
    ta.style.height = 'auto'
    const cs = getComputedStyle(ta)
    const lineH = parseFloat(cs.lineHeight) || ta.clientHeight || 22.5
    const pad =
      (parseFloat(cs.paddingTop) || 0) + (parseFloat(cs.paddingBottom) || 0)
    const maxH = lineH * MAX_ROWS + pad
    const next = Math.min(ta.scrollHeight, maxH)
    ta.style.height = `${next}px`
  }, [text, images])

  // ── Slash command palette (issue #39) ──────────────────────────────────────
  // The registry + two-level filtering live in `slashCommands.ts` (pure, unit
  // tested). `sel` is the keyboard-selected row; it clamps when the list
  // shrinks mid-typing and resets to the top whenever the input changes.
  const [selIndex, setSelIndex] = useState(0)
  const isSlash = text.startsWith('/')
  const suggestions = isSlash ? getSuggestions(text) : []
  const sel = suggestions.length > 0 ? Math.min(selIndex, suggestions.length - 1) : 0

  /** Accept one suggestion. `run` = Enter/click: execute immediately when the
   *  entry is a complete command, otherwise fill the input (a folder command
   *  opens its second level; a takesArg subcommand makes room for its arg).
   *  `fill` = Tab: never executes, only completes the input line. */
  const acceptSuggestion = (idx: number, run: boolean) => {
    const s = suggestions[idx]
    if (s === undefined) return
    if (run && s.invocation !== null) {
      onSlash(s.invocation)
      setText('')
    } else {
      setText(s.insert)
    }
    setSelIndex(0)
    taRef.current?.focus()
  }

  const runSlash = (raw: string): boolean => {
    const parsed = parseSlash(raw)
    if (parsed === null) return false
    if (parsed.kind === 'command') onSlash(parsed.invocation)
    else onUnknownSlash(parsed.label)
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
        {workspaceDropdownOpen && workspaceEnabled && (
          <WorkspaceDropdown
            onClose={onCloseWorkspaceDropdown}
            onSelect={onSelectWorkspace}
            onAddWorkspace={onAddWorkspace}
          />
        )}
        {workspaceEnabled && (
          <button
            className={`${styles.chip} ${styles.workspaceChip} ${workspaceLabel !== null ? styles.chipOn : ''}`}
            onClick={onWorkspaceClick}
            title={t('workspace.select')}
            aria-label={t('workspace.select')}
          >
            <span className={styles.chipLabel}>📂</span>
            <span className={styles.workspaceAlias}>
              {workspaceLabel ?? t('workspace.select')}
            </span>
            <span className={styles.workspaceCaret}>▾</span>
          </button>
        )}
        <Tooltip label={t(MODE_META[mode].tipKey)} side="top">
          <button
            className={`${styles.chip} ${mode !== 'normal' ? styles.chipOn : ''}`}
            onClick={onCycleMode}
            aria-label={t('mode.cycle')}
          >
            <span className={styles.chipLabel}>
              {MODE_META[mode].glyph ? MODE_META[mode].glyph + ' ' : ''}
              {t(MODE_META[mode].labelKey)}
            </span>
            {paradigm !== 're_act' && paradigm !== 'plan' && (
              <span className={styles.chipParadigm}>{paradigm}{PARADIGM_GLYPH[paradigm]}</span>
            )}
          </button>
        </Tooltip>
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
        <MetricsBar metrics={metrics} />
        <div className={styles.modelSlot}>
          <EffortChip store={settingsStore} />
          <ModelSelect store={settingsStore} />
        </div>
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
        {isSlash && suggestions.length > 0 && (
          <div className={styles.slashPopup} role="listbox" aria-label={t('command.hint')}>
            {(() => {
              // Group headers render only when the visible entries span more
              // than one group — the second level always belongs to a single
              // command, so it stays header-free.
              const showHeaders = new Set(suggestions.map((s) => s.group)).size > 1
              const rows: ReactNode[] = []
              let lastGroup: CommandGroup | null = null
              suggestions.forEach((s, i) => {
                if (showHeaders && s.group !== lastGroup) {
                  lastGroup = s.group
                  rows.push(
                    <div key={`group-${s.group}`} className={styles.slashGroupHeader}>
                      {t(`command.group.${s.group}`)}
                    </div>,
                  )
                }
                rows.push(
                  <button
                    key={s.display}
                    role="option"
                    aria-selected={i === sel}
                    className={`${styles.slashItem} ${i === sel ? styles.slashItemSelected : ''}`}
                    onMouseEnter={() => setSelIndex(i)}
                    onClick={() => acceptSuggestion(i, true)}
                  >
                    <span className={styles.slashLabel}>{s.display}</span>
                    <span className={styles.slashDesc}>{t(s.descKey)}</span>
                  </button>,
                )
              })
              return rows
            })()}
            <div className={styles.slashHint}>{t('slash.keys')}</div>
          </div>
        )}
        <span className={styles.prompt} aria-hidden="true">❯</span>
        <textarea
          ref={taRef}
          className={styles.input}
          placeholder={placeholder}
          value={text}
          onChange={(e) => {
            setText(e.target.value)
            setSelIndex(0)
          }}
          onKeyDown={(e) => {
            // Palette navigation while suggestions are visible (issue #39
            // two-level flow mirrors the TUI's #30 autocomplete: ↑↓ move,
            // Tab fills the line, Enter runs it / opens the next level).
            if (suggestions.length > 0 && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
              e.preventDefault()
              setSelIndex((prev) => {
                const cur = Math.min(prev, suggestions.length - 1)
                return e.key === 'ArrowDown'
                  ? (cur + 1) % suggestions.length
                  : (cur - 1 + suggestions.length) % suggestions.length
              })
              return
            }
            if (suggestions.length > 0 && e.key === 'Tab') {
              e.preventDefault()
              acceptSuggestion(sel, false)
              return
            }
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              if (suggestions.length > 0) {
                acceptSuggestion(sel, true)
                return
              }
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
          <button
            className={`${styles.sendBtn} ${styles.stopBtn}`}
            onClick={onStop}
            aria-label={stopLabel}
          >
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden focusable="false">
              <rect x="3" y="3" width="10" height="10" rx="2" fill="currentColor" />
            </svg>
          </button>
        ) : (
          <button
            className={styles.sendBtn}
            onClick={submit}
            disabled={text.trim().length === 0 && images.length === 0}
            aria-label={sendLabel}
          >
            <svg width="20" height="20" viewBox="0 0 20 20" aria-hidden focusable="false">
              <path
                d="M10 4.2 L16 14.2 L10 11.4 L4 14.2 Z"
                fill="currentColor"
              />
            </svg>
          </button>
        )}
      </div>
    </div>
  )
}
