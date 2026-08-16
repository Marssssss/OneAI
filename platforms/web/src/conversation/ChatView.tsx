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

import { memo, useEffect, useRef } from 'react'
import type { ReactNode } from 'react'
import type { BusScenarioMember } from '../rpc/types'
import type { ChatNode } from '../store/projection'
import { useLocale } from '../i18n'
import { IncrementalMarkdown } from './IncrementalMarkdown'
import { ToolCallNode } from './ToolCallNode'
import { PlanNode } from './PlanNode'
import styles from './ChatView.module.css'

interface ChatViewProps {
  nodes: ChatNode[]
  turnActive: boolean
  selectedToolNodeId: string | null
  onSelectTool: (nodeId: string) => void
  theme: 'light' | 'dark'
  /** Active scenario members (for speaker name/color) — null in single-agent. */
  members: BusScenarioMember[] | null
}

const FOLLOW_THRESHOLD = 80 // px from bottom — keep auto-following

export function ChatView({
  nodes,
  turnActive,
  selectedToolNodeId,
  onSelectTool,
  theme,
  members,
}: ChatViewProps): ReactNode {
  const scrollRef = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

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
          />
        ))}
        {turnActive && <TypingDots />}
      </div>
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
    return { name: userLabel, color: '#8A8A8A' }
  }
  if (members !== null) {
    const m = members.find((x) => x.id === speaker)
    if (m !== undefined) {
      return { name: m.name, color: m.color ?? '#8A8A8A' }
    }
  }
  return { name: speaker, color: '#8A8A8A' }
}

// One row — memoized so a streaming assistant node's text delta doesn't
// re-render its siblings.
const ChatNodeSeat = memo(function ChatNodeSeat({
  node,
  theme,
  members,
  selected,
  onSelect,
}: {
  node: ChatNode
  theme: 'light' | 'dark'
  members: BusScenarioMember[] | null
  selected: boolean
  onSelect: (nodeId: string) => void
}): ReactNode {
  const { t } = useLocale()
  if (node.role === 'user') {
    return (
      <div className={styles.row}>
        <div className={`${styles.bubble} ${styles.userBubble}`}>{node.text}</div>
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
