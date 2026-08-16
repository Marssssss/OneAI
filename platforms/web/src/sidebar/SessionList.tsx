// SessionList — the saved-conversations list backed by the `session/list`
// synchronous RPC. Each row shows the (possibly client-overridden) title +
// message count + a "⋯" more menu (rename / archive / delete). Archived
// sessions (web-local localStorage flag) are hidden from the main list and
// shown under a collapsed "已归档 (N)" expander with an un-archive action.
// Delete is engine-backed (`session/delete`); rename + archive are web-local
// until a server-side RPC lands.

import { useState } from 'react'
import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { SessionInfo } from '../rpc/types'
import type { SessionMeta } from '../store/sessionMeta'
import { MoreMenu } from '../components/MoreMenu'
import styles from './SessionList.module.css'

interface SessionListProps {
  sessions: SessionInfo[]
  currentId: string | null
  meta: SessionMeta
  onPick: (id: string) => void
  onRename: (id: string, currentTitle: string) => void
  onArchive: (id: string) => void
  onUnarchive: (id: string) => void
  onDelete: (id: string) => void
}

export function SessionList({
  sessions,
  currentId,
  meta,
  onPick,
  onRename,
  onArchive,
  onUnarchive,
  onDelete,
}: SessionListProps): ReactNode {
  const { t } = useLocale()
  const [showArchived, setShowArchived] = useState(false)

  if (sessions.length === 0) {
    return <div className={styles.empty}>{t('sidebar.empty')}</div>
  }

  const active = sessions.filter((s) => !meta.archived.has(s.id))
  const archived = sessions.filter((s) => meta.archived.has(s.id))

  const row = (s: SessionInfo, archivedRow: boolean): ReactNode => {
    const title = meta.titles[s.id] ?? s.title
    const items = archivedRow
      ? [
          { id: 'rename', label: t('session.rename') },
          { id: 'unarchive', label: t('session.unarchive') },
          { id: 'delete', label: t('session.delete'), danger: true },
        ]
      : [
          { id: 'rename', label: t('session.rename') },
          { id: 'archive', label: t('session.archive') },
          { id: 'delete', label: t('session.delete'), danger: true },
        ]
    return (
      <div
        key={s.id}
        className={`${styles.item} ${s.id === currentId ? styles.active : ''}`}
      >
        <button className={styles.pick} onClick={() => onPick(s.id)} title={title}>
          <span className={styles.title}>{title || s.id}</span>
          <span className={styles.meta}>{s.message_count}</span>
        </button>
        <MoreMenu
          items={items}
          onPick={(action) => {
            if (action === 'rename') onRename(s.id, title)
            else if (action === 'archive') onArchive(s.id)
            else if (action === 'unarchive') onUnarchive(s.id)
            else if (action === 'delete') onDelete(s.id)
          }}
          ariaLabel={t('session.more')}
        />
      </div>
    )
  }

  return (
    <div className={styles.list}>
      {active.map((s) => row(s, false))}
      {active.length === 0 && archived.length > 0 && (
        <div className={styles.empty}>{t('sidebar.allArchived')}</div>
      )}
      {archived.length > 0 && (
        <>
          <button
            className={styles.archivedToggle}
            onClick={() => setShowArchived((v) => !v)}
          >
            {t('session.archived')} ({archived.length}){' '}
            <span className={styles.caret}>{showArchived ? '▴' : '▾'}</span>
          </button>
          {showArchived && (
            <div className={styles.archivedList}>
              {archived.map((s) => row(s, true))}
            </div>
          )}
        </>
      )}
    </div>
  )
}
