// SessionList — the saved-conversations list backed by the `session/list`
// synchronous RPC. Mirrors dsh's `ui-workspace` session tree (flattened for
// W1 — grouping/search land in W2). Click loads the session.

import { useLocale } from '../i18n'
import type { SessionInfo } from '../rpc/types'
import styles from './SessionList.module.css'

interface SessionListProps {
  sessions: SessionInfo[]
  currentId: string | null
  onPick: (id: string) => void
}

export function SessionList({
  sessions,
  currentId,
  onPick,
}: SessionListProps): React.ReactNode {
  const { t } = useLocale()
  if (sessions.length === 0) {
    return <div className={styles.empty}>{t('sidebar.empty')}</div>
  }
  return (
    <div className={styles.list}>
      {sessions.map((s) => (
        <button
          key={s.id}
          className={`${styles.item} ${s.id === currentId ? styles.active : ''}`}
          onClick={() => onPick(s.id)}
        >
          <span className={styles.title}>{s.title || s.id}</span>
          <span className={styles.meta}>{s.message_count}</span>
        </button>
      ))}
    </div>
  )
}
