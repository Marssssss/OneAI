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
        {nodes.map((n) => (
          <ChatNodeSeat
            key={n.id}
            node={n}
            theme={theme}
            members={members}
            selected={selectedToolNodeId === n.id}
            onSelect={onSelectTool}
            onOpenImage={onOpenImage}
            onSubmitFeedback={onSubmitFeedback}
          />
        ))}
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
}: {
  node: ChatNode
  theme: 'light' | 'dark'
  members: BusScenarioMember[] | null
  selected: boolean
  onSelect: (nodeId: string) => void
  onOpenImage: (src: string, alt: string) => void
  onSubmitFeedback: (nodeId: string, kind: FeedbackKind, text?: string) => void
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
  const showSpeaker = speaker !== null && node.kind !== 'error'

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
        <SpeakerHeader meta={speaker} thinking />
        <div className={styles.thinking}>
          <span className={styles.thinkingLabel}>· {t('thinking')}</span>
          <div className={styles.thinkingText}>{node.text}</div>
        </div>
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
  // Feedback is only available on the **current live conversation** — a node
  // has a `turnId` only while it's a live turn's output. Reloaded/historical
  // messages replay without a turn_id, so they render no 👍/👎 buttons (by
  // design: feedback reacts to a fresh model output, not stored history).
  const feedbackDone =
    node.state === 'done' && !empty && node.turnId !== null && node.turnId.length > 0
  const currentKind = node.feedback?.kind
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
      {feedbackDone && (
        <div className={styles.feedbackRow}>
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
  )
})

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
