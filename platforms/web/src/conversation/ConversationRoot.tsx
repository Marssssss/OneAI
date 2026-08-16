// ConversationRoot — the center column: header chrome + chat scroll + composer.
//
// Resident skeleton (stays mounted across empty/populated session), mirroring
// dsh's `ConversationRoot`. W1 renders the status bar + empty-state hero +
// ChatView + Composer; the details rail + approval panel land in W2/W4.

import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { ProjectionSnapshot } from '../store/projection'
import { ChatView } from './ChatView'
import { Composer } from './Composer'
import styles from './ConversationRoot.module.css'

interface ConversationRootProps {
  snapshot: ProjectionSnapshot
  connection: 'connecting' | 'open' | 'closed' | 'error'
  onSend: (text: string) => void
}

export function ConversationRoot({
  snapshot,
  connection,
  onSend,
}: ConversationRootProps): ReactNode {
  const { t } = useLocale()
  const statusText =
    connection === 'open'
      ? t('status.open')
      : connection === 'connecting'
        ? t('status.connecting')
        : connection === 'error'
          ? t('status.error')
          : t('status.closed')

  const empty = snapshot.nodes.length === 0

  return (
    <div className={styles.root}>
      <header className={styles.header}>
        <span className={styles.brand}>{t('app.title')}</span>
        <span className={`${styles.status} ${styles[`status_${connection}`]}`}>
          {statusText}
        </span>
      </header>

      <div className={styles.body}>
        {snapshot.lastError !== null && (
          <div className={styles.errorBanner} title={snapshot.lastError}>
            {t('chat.error')}: {snapshot.lastError}
          </div>
        )}
        {empty ? (
          <div className={styles.empty}>
            <div className={styles.emptyTitle}>{t('chat.empty.title')}</div>
            <div className={styles.emptySubtitle}>{t('chat.empty.subtitle')}</div>
          </div>
        ) : (
          <ChatView nodes={snapshot.nodes} turnActive={snapshot.turnActive} />
        )}
      </div>

      <div className={styles.composer}>
        <Composer
          placeholder={t('composer.placeholder')}
          sendLabel={t('composer.send')}
          turnActive={snapshot.turnActive}
          onSend={onSend}
        />
      </div>
    </div>
  )
}
