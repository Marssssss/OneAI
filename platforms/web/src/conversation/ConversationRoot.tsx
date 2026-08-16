// ConversationRoot — the center column: header chrome + chat scroll + approval
// bar + composer.
//
// Resident skeleton (stays mounted across empty/populated session), mirroring
// dsh's `ConversationRoot`. W2 adds: ApprovalPanel (composer-takeover when an
// approval is queued), tool/plan node dispatch, plan-mode chip, stop button,
// and the details-rail selection wiring.

import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { ProjectionSnapshot } from '../store/projection'
import type { InteractionResponse } from '../rpc/types'
import { ChatView } from './ChatView'
import { Composer, type SlashCommand } from './Composer'
import { ApprovalPanel } from './ApprovalPanel'
import styles from './ConversationRoot.module.css'

interface ConversationRootProps {
  snapshot: ProjectionSnapshot
  connection: 'connecting' | 'open' | 'closed' | 'error'
  theme: 'light' | 'dark'
  planMode: boolean
  onSend: (text: string) => void
  onStop: () => void
  onTogglePlan: () => void
  onSlash: (cmd: SlashCommand) => void
  onSelectTool: (nodeId: string) => void
  onRespondApproval: (requestId: string, response: InteractionResponse) => void
}

export function ConversationRoot({
  snapshot,
  connection,
  theme,
  planMode,
  onSend,
  onStop,
  onTogglePlan,
  onSlash,
  onSelectTool,
  onRespondApproval,
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
          {snapshot.paradigm !== 're_act' && (
            <span className={styles.paradigm}> · {snapshot.paradigm}</span>
          )}
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
          <ChatView
            nodes={snapshot.nodes}
            turnActive={snapshot.turnActive}
            selectedToolNodeId={snapshot.selectedToolNodeId}
            onSelectTool={onSelectTool}
            theme={theme}
          />
        )}
      </div>

      <ApprovalPanel
        current={snapshot.currentApproval}
        queueDepth={snapshot.approvalQueueDepth}
        onRespond={onRespondApproval}
      />

      <div className={styles.composer}>
        <Composer
          placeholder={t('composer.placeholder')}
          sendLabel={t('composer.send')}
          stopLabel={t('composer.stop')}
          turnActive={snapshot.turnActive}
          paradigm={snapshot.paradigm}
          planMode={planMode}
          onSend={onSend}
          onStop={onStop}
          onTogglePlan={onTogglePlan}
          onSlash={onSlash}
        />
      </div>
    </div>
  )
}
