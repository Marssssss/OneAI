// Composer — the message input + send button. Enter sends, Shift+Enter for a
// newline. Disabled (shows Stop semantics to come in W2) while a turn is in
// flight.

import { useState } from 'react'
import type { ReactNode } from 'react'
import styles from './Composer.module.css'

interface ComposerProps {
  placeholder: string
  sendLabel: string
  turnActive: boolean
  onSend: (text: string) => void
}

export function Composer({
  placeholder,
  sendLabel,
  turnActive,
  onSend,
}: ComposerProps): ReactNode {
  const [text, setText] = useState('')

  const submit = () => {
    const trimmed = text.trim()
    if (trimmed.length === 0 || turnActive) return
    onSend(trimmed)
    setText('')
  }

  return (
    <div className={styles.wrap}>
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
      <button
        className={styles.button}
        onClick={submit}
        disabled={text.trim().length === 0 || turnActive}
        title={sendLabel}
      >
        {turnActive ? '…' : '↑'}
      </button>
    </div>
  )
}
