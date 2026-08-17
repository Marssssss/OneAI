// ChatView — the keyed message stream. Mirrors dsh's `ChatView` /
// `ChatNodeSeat`: each row subscribes to one node id so an assistant delta
// or a tool lifecycle update only re-renders its own row, not the whole list.
//
// Node kinds: user / assistant-text (IncrementalMarkdown) / thinking / error
// / tool (ToolCallNode disclosure) / plan (PlanNode checklist).
//
// W3: in scenario (group) mode, assistant/thinking bubbles render a speaker
// header (name + color dot) resolved from the active scenario's members —
// the `speaker` field on each fragment drives bubble attribution.
//
// W4: user bubbles render attachment thumbnails (drag-drop images); assistant
// text bubbles render a per-turn deliverable strip (file artifacts) + 👍/👎
// feedback controls; image attachments + image deliverables open a Lightbox.

import { memo, useCallback, useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import type { BusScenarioMember, FeedbackKind } from '../rpc/types'
import type { ChatNode } from '../store/projection'
import { useLocale } from '../i18n'
import { IncrementalMarkdown } from './IncrementalMarkdown'
import { ToolCallNode } from './ToolCallNode'
import { PlanNode } from './PlanNode'
import { DeliverableStrip } from './DeliverableStrip'
import { Lightbox } from './Lightbox'
import styles from './ChatView.module.css'

interface ChatViewProps {
  nodes: ChatNode[]
  turnActive: boolean
  selectedToolNodeId: string | null
  onSelectTool: (nodeId: string) => void
  theme: 'light' | 'dark'
  /** Active scenario members (for speaker name/color) — null in single-agent. */
  members: BusScenarioMember[] | null
  /** §W4 B4 — record 👍/👎/note against one assistant text node. */
  onSubmitFeedback: (nodeId: string, kind: FeedbackKind, text?: string) => void
}

const FOLLOW_THRESHOLD = 80 // px from bottom — keep auto-following

interface LightboxState {
  src: string
  alt: string
}

export function ChatView({
  nodes,
  turnActive,
  selectedToolNodeId,
  onSelectTool,
  theme,
  members,
  onSubmitFeedback,
}: ChatViewProps): ReactNode {
  const scrollRef = useRef<HTMLDivElement>(null)
  const stick = useRef(true)
  const [lightbox, setLightbox] = useState<LightboxState | null>(null)

  // Stable open/close so memoized seats don't all re-render on a parent tick.
  const onOpenImage = useCallback((src: string, alt: string) => {
    setLightbox({ src, alt })
  }, [])
  const onCloseLightbox = useCallback(() => setLightbox(null), [])

  // Bottom-follow: only auto-scroll if the user is near the bottom; once they
  // scroll up during a turn, stop following (resume when they hit bottom).
  useEffect(() => {
    const el = scrollRef.current
    if (el === null) return
    const onScroll = () => {
      const nearBottom =
        el.scrollHeight - el.scrollTop - el.clientHeight < FOLLOW_THRESHOLD
      stick.current = nearBottom
    }
    el.addEventListener('scroll', onScroll, { passive: true })
    return () => el.removeEventListener('scroll', onScroll)
  }, [])

  useEffect(() => {
    const el = scrollRef.current
    if (el !== null && stick.current) {
      el.scrollTop = el.scrollHeight
    }
  }, [nodes, turnActive])

  return (
    <div className={styles.scroll} ref={scrollRef}>
      <div className={styles.list}>
        {/* The 👍/👎 affordance belongs only to a turn's final text output, not
         * to every intermediate step's text (the agent loop often emits several
         * text blocks across iterations before its terminal answer). Walk the
         * node list once to mark, per turn_id, the last assistant text node —
         * ChatNodeSeat gates the feedback row on `isTurnFinal`. */}
        {(() => {
          const finalByTurn = new Map<string, string>()
          for (let i = nodes.length - 1; i >= 0; i -= 1) {
            const n = nodes[i]
            if (
              n.kind === 'text' &&
              n.role === 'assistant' &&
              n.turnId !== null
            ) {
              if (!finalByTurn.has(n.turnId)) finalByTurn.set(n.turnId, n.id)
            }
          }
          // Speaker-header merging (UI-1): a single speaker within one turn
          // may emit several blocks — a thinking fragment, then the final
          // text answer (and possibly tool cards). Tagging each block with
          // the speaker name repeats the label under every block. Instead,
          // show the speaker header ONLY on the first block of a contiguous
          // same-speaker run within a turn: a block is a "head" when no
          // preceding block in the same turn carried the same speaker. (A
          // user message or a different speaker always starts a new run.)
          const headIds = new Set<string>()
          let prevSpeaker: string | null = null
          let prevTurnId: string | null = null
          for (const n of nodes) {
            const sp = n.role === 'user' ? null : n.speaker
            const isHead =
              sp !== null &&
              (prevTurnId !== n.turnId || prevSpeaker !== sp)
            if (sp !== null && isHead) headIds.add(n.id)
            prevSpeaker = sp
            prevTurnId = n.turnId
          }
          return nodes.map((n) => (
            <ChatNodeSeat
              key={n.id}
              node={n}
              theme={theme}
              members={members}
              selected={selectedToolNodeId === n.id}
              onSelect={onSelectTool}
              onOpenImage={onOpenImage}
              onSubmitFeedback={onSubmitFeedback}
              isTurnFinal={
                n.turnId !== null && finalByTurn.get(n.turnId) === n.id
              }
              isSpeakerHead={headIds.has(n.id)}
            />
          ))
        })()}
        {turnActive && <TypingDots />}
      </div>
      {lightbox !== null && (
        <Lightbox src={lightbox.src} alt={lightbox.alt} onClose={onCloseLightbox} />
      )}
    </div>
  )
}

interface SpeakerMeta {
  name: string
  color: string
}

function resolveSpeaker(
  speaker: string | null,
  members: BusScenarioMember[] | null,
  userLabel: string,
): SpeakerMeta | null {
  if (speaker === null) return null
  if (speaker === 'user' || speaker.length === 0) {
    return { name: userLabel, color: 'var(--oneai-speaker-fallback)' }
  }
  if (members !== null) {
    const m = members.find((x) => x.id === speaker)
    if (m !== undefined) {
      return { name: m.name, color: m.color ?? 'var(--oneai-speaker-fallback)' }
    }
  }
  return { name: speaker, color: 'var(--oneai-speaker-fallback)' }
}

/** Derive a `data:` URL from an image content block's base64 data. */
function imagePreviewSrc(block: { type: string; mime_type?: string; data?: string; uri?: string }): string {
  if (block.type === 'file' && block.uri !== undefined) return block.uri
  if (block.type === 'image' && block.data !== undefined && block.mime_type !== undefined) {
    return `data:${block.mime_type};base64,${block.data}`
  }
  return ''
}

// One row — memoized so a streaming assistant node's text delta doesn't
// re-render its siblings.
const ChatNodeSeat = memo(function ChatNodeSeat({
  node,
  theme,
  members,
  selected,
  onSelect,
  onOpenImage,
  onSubmitFeedback,
  isTurnFinal,
  isSpeakerHead,
}: {
  node: ChatNode
  theme: 'light' | 'dark'
  members: BusScenarioMember[] | null
  selected: boolean
  onSelect: (nodeId: string) => void
  onOpenImage: (src: string, alt: string) => void
  onSubmitFeedback: (nodeId: string, kind: FeedbackKind, text?: string) => void
  /** True when this is the last assistant text node of its turn — the only
   * node eligible for the 👍/👎 row (intermediate step outputs are not
   * feedbackable; feedback reacts to a turn's terminal answer). */
  isTurnFinal: boolean
  /** True when this block is the first of a contiguous same-speaker run
   *  within its turn — the only block that renders the speaker header, so
   *  a single speaker's thinking + final answer show one label, not one
   *  per block (UI-1). */
  isSpeakerHead: boolean
}): ReactNode {
  const { t } = useLocale()
  if (node.role === 'user') {
    const images = (node.attachments ?? []).filter(
      (b) => b.type === 'image' || b.type === 'file',
    )
    return (
      <div className={styles.row}>
        <div className={`${styles.bubble} ${styles.userBubble}`}>
          {node.text.length > 0 && <div>{node.text}</div>}
          {images.length > 0 && (
            <div className={styles.attachments}>
              {images.map((b, i) => {
                const src = imagePreviewSrc(b)
                if (src.length === 0) return null
                return (
                  <img
                    key={i}
                    className={styles.attachmentThumb}
                    src={src}
                    alt="attachment"
                    onClick={() => onOpenImage(src, 'attachment')}
                  />
                )
              })}
            </div>
          )}
        </div>
      </div>
    )
  }
  const speaker = resolveSpeaker(node.speaker, members, t('speaker.you'))
  const showSpeaker = isSpeakerHead && speaker !== null && node.kind !== 'error'

  if (node.kind === 'error') {
    return (
      <div className={styles.row}>
        <div className={styles.errorBubble}>{node.text}</div>
      </div>
    )
  }
  if (node.kind === 'thinking') {
    return (
      <div className={styles.row}>
        {showSpeaker && <SpeakerHeader meta={speaker} thinking />}
        <ThinkingBlock node={node} label={t('thinking')} />
      </div>
    )
  }
  if (node.kind === 'tool') {
    return (
      <div className={styles.row}>
        {showSpeaker && <SpeakerHeader meta={speaker} />}
        <ToolCallNode node={node} selected={selected} onSelect={onSelect} />
      </div>
    )
  }
  if (node.kind === 'plan') {
    return (
      <div className={styles.row}>
        <PlanNode steps={node.planSteps ?? []} revision={node.planRevision} />
      </div>
    )
  }
  // assistant text
  const empty = node.text.length === 0
  // Feedback is only available on the **current live conversation's terminal
  // answer** — a node has a `turn_id` only while it's a live turn's output, and
  // `isTurnFinal` narrows it to the turn's last text node (intermediate step
  // outputs are not feedbackable). Reloaded/historical messages replay without
  // a turn_id, so they render no 👍/👎 buttons either (by design: feedback
  // reacts to a fresh model output, not stored history).
  const feedbackDone =
    isTurnFinal &&
    node.state === 'done' &&
    !empty &&
    node.turnId !== null &&
    node.turnId.length > 0
  const currentKind = node.feedback?.kind
  // Copy is a client-side utility with no turn-id dependency, so it surfaces on
  // every model answer — the live turn's final text (`isTurnFinal`) AND every
  // reloaded/historical assistant message (`turnId === null`, where each stored
  // assistant block is itself a discrete turn's terminal answer). It is gated to
  // `state === 'done'` so the affordance appears only once the answer settles,
  // not mid-stream. Intermediate live step outputs (non-final, turnId !== null)
  // are excluded.
  const showCopy =
    !empty &&
    node.state === 'done' &&
    (isTurnFinal || node.turnId === null || node.turnId.length === 0)
  return (
    <div className={styles.row}>
      {showSpeaker && <SpeakerHeader meta={speaker} />}
      <div className={`${styles.bubble} ${styles.assistantBubble}`}>
        {empty ? (
          <span className={styles.placeholder}>…</span>
        ) : (
          <IncrementalMarkdown text={node.text} theme={theme} />
        )}
        {node.state === 'streaming' && !empty && <span className={styles.cursor} />}
      </div>
      {node.deliverables !== undefined && node.deliverables.length > 0 && (
        <DeliverableStrip artifacts={node.deliverables} onOpenImage={onOpenImage} />
      )}
      {(feedbackDone || showCopy) && (
        <div className={styles.feedbackRow}>
          {showCopy && (
            <CopyButton
              text={node.text}
              copyLabel={t('chat.copy')}
              copiedLabel={t('chat.copied')}
            />
          )}
          {feedbackDone && (
            <div className={styles.feedbackGroup}>
              <button
                className={`${styles.feedbackBtn} ${
                  currentKind === 'up' ? styles.feedbackOn : ''
                } ${node.feedbackPending ? styles.feedbackPending : ''}`}
                onClick={() => onSubmitFeedback(node.id, 'up')}
                aria-label="thumbs up"
                disabled={node.feedbackPending === true}
              >
                👍
              </button>
              <button
                className={`${styles.feedbackBtn} ${
                  currentKind === 'down' ? styles.feedbackOn : ''
                } ${node.feedbackPending ? styles.feedbackPending : ''}`}
                onClick={() => onSubmitFeedback(node.id, 'down')}
                aria-label="thumbs down"
                disabled={node.feedbackPending === true}
              >
                👎
              </button>
              {node.feedback?.text !== undefined && (
                <span className={styles.feedbackNote}>{node.feedback.text}</span>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  )
})

// CopyButton — a persistent copy affordance for a model answer's text. Lives
// on the actions row at full opacity (does NOT fade with the 👍/👎 group) so the
// user always sees it. `copied` is a local 1.5s flash state scoped to this
// memoized component — toggling it re-renders only this button, not the seat.
// Uses the async Clipboard API with a hidden-textarea `execCommand` fallback for
// non-secure contexts / older browsers.
const CopyButton = memo(function CopyButton({
  text,
  copyLabel,
  copiedLabel,
}: {
  text: string
  copyLabel: string
  copiedLabel: string
}): ReactNode {
  const [copied, setCopied] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(
    () => () => {
      if (timer.current !== null) clearTimeout(timer.current)
    },
    [],
  )

  const onClick = useCallback(async () => {
    try {
      if (navigator.clipboard?.writeText !== undefined) {
        await navigator.clipboard.writeText(text)
      } else {
        fallbackCopy(text)
      }
    } catch {
      fallbackCopy(text)
    }
    setCopied(true)
    if (timer.current !== null) clearTimeout(timer.current)
    timer.current = setTimeout(() => setCopied(false), 1500)
  }, [text])

  return (
    <button
      type="button"
      className={styles.copyBtn}
      onClick={onClick}
      aria-label={copied ? copiedLabel : copyLabel}
      title={copied ? copiedLabel : copyLabel}
    >
      {copied ? (
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden focusable="false">
          <path
            d="M3.5 8.5 L6.5 11.5 L12.5 4.5"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      ) : (
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden focusable="false">
          <rect
            x="5"
            y="1.5"
            width="7"
            height="3"
            rx="1"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
          />
          <path
            d="M4 4.5 H12 V14.5 H4 Z"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinejoin="round"
          />
        </svg>
      )}
    </button>
  )
})

/** Hidden-textarea copy fallback for environments without the async Clipboard API
 * (non-HTTPS pages, sandboxed iframes without `allow-scripts`). Best-effort. */
function fallbackCopy(text: string): void {
  const ta = document.createElement('textarea')
  ta.value = text
  ta.style.position = 'fixed'
  ta.style.opacity = '0'
  document.body.appendChild(ta)
  ta.select()
  try {
    document.execCommand('copy')
  } catch {
    /* noop — clipboard unavailable */
  }
  document.body.removeChild(ta)
}

function SpeakerHeader({
  meta,
  thinking,
}: {
  meta: SpeakerMeta | null
  thinking?: boolean
}): ReactNode {
  if (meta === null) return null
  return (
    <div className={`${styles.speakerRow} ${thinking ? styles.speakerRowThinking : ''}`}>
      <span
        className={styles.speakerDot}
        style={{ background: meta.color }}
        aria-hidden
      />
      <span className={styles.speakerName}>{meta.name}</span>
    </div>
  )
}

// ThinkingBlock — deepseek-harness style: a single-line, collapsed-by-default
// reasoning affordance. While streaming, the one line follows the live text
// (auto-scrolled to the tail so the latest reasoning shows). Once the fragment
// is done, the line shows the first sentence; clicking expands the full text.
const ThinkingBlock = memo(function ThinkingBlock({
  node,
  label,
}: {
  node: ChatNode
  label: string
}): ReactNode {
  const [expanded, setExpanded] = useState(false)
  const lineRef = useRef<HTMLDivElement>(null)
  const streaming = node.state === 'streaming'
  const collapsedLine = streaming ? node.text : firstSentence(node.text)

  // While streaming & collapsed, keep the single line pinned to the tail so
  // the latest reasoning is visible (mirrors dsh's scrolling one-line display).
  useEffect(() => {
    if (expanded || !streaming) return
    const el = lineRef.current
    if (el !== null) {
      el.scrollLeft = el.scrollWidth
    }
  }, [node.text, expanded, streaming])

  return (
    <div className={`${styles.thinking} ${expanded ? styles.thinkingOpen : ''}`}>
      <button
        className={styles.thinkingToggle}
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
      >
        <span className={styles.thinkingLabel}>
          {streaming ? '💭 ' + label + '…' : '💭 ' + label}
        </span>
        {!expanded && (
          <span className={styles.thinkingLine} ref={lineRef}>
            {collapsedLine.length > 0 ? collapsedLine : ''}
          </span>
        )}
        <span className={styles.thinkingChevron} aria-hidden>
          {expanded ? '▾' : '▸'}
        </span>
      </button>
      {expanded && <div className={styles.thinkingText}>{node.text}</div>}
    </div>
  )
})

/** First sentence of a thinking fragment — split on terminal punctuation or
 * newline, trimmed + truncated to one line. Returns '' for empty input. */
function firstSentence(text: string): string {
  const t = text.trim()
  if (t.length === 0) return ''
  const m = t.split(/[。.!?\n]/)[0] ?? ''
  const s = m.trim().replace(/\s+/g, ' ')
  return s.length > 120 ? s.slice(0, 119) + '…' : s
}

function TypingDots(): ReactNode {
  return (
    <div className={styles.row}>
      <div className={styles.typing}>
        <span />
        <span />
        <span />
      </div>
    </div>
  )
}
