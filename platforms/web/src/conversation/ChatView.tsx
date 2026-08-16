// ChatView — the keyed message stream. Mirrors dsh's `ChatView` /
// `ChatNodeSeat`: each row subscribes to one node id so an assistant delta
// or a tool lifecycle update only re-renders its own row, not the whole list.
//
// W1 renders user/assistant-text/thinking/error nodes. Tool calls, approvals,
// deliverables, and group-chat speaker routing land in W2/W3.

import { memo, useEffect, useRef } from 'react'
import type { ReactNode } from 'react'
import type { ChatNode, NodeState } from '../store/projection'
import { Markdown } from './Markdown'
import styles from './ChatView.module.css'

interface ChatViewProps {
  nodes: ChatNode[]
  turnActive: boolean
}

const FOLLOW_THRESHOLD = 80 // px from bottom — keep auto-following

export function ChatView({ nodes, turnActive }: ChatViewProps): ReactNode {
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
          <ChatNodeSeat key={n.id} node={n} />
        ))}
        {turnActive && <TypingDots />}
      </div>
    </div>
  )
}

// One row — memoized so a streaming assistant node's text delta doesn't
// re-render its siblings.
const ChatNodeSeat = memo(function ChatNodeSeat({ node }: { node: ChatNode }) {
  if (node.role === 'user') {
    return (
      <div className={styles.row}>
        <div className={`${styles.bubble} ${styles.userBubble}`}>{node.text}</div>
      </div>
    )
  }
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
        <div className={styles.thinking}>
          <span className={styles.thinkingLabel}>· thinking</span>
          <div className={styles.thinkingText}>{node.text}</div>
        </div>
      </div>
    )
  }
  // assistant text
  const empty = node.text.length === 0
  return (
    <div className={styles.row}>
      <div className={`${styles.bubble} ${styles.assistantBubble}`}>
        {empty ? <span className={styles.placeholder}>…</span> : <Markdown text={node.text} />}
        {node.state === 'streaming' && !empty && <span className={styles.cursor} />}
      </div>
    </div>
  )
})

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

export type { NodeState }
