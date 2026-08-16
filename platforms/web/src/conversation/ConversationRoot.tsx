// ConversationRoot — the center column: header chrome + chat scroll + approval
// bar + composer.
//
// Resident skeleton (stays mounted across empty/populated session), mirroring
// dsh's `ConversationRoot`. W2 adds: ApprovalPanel (composer-takeover when an
// approval is queued), tool/plan node dispatch, plan-mode chip, stop button,
// and the details-rail selection wiring. W3 adds: hero scenario chips on the
// empty state, a debrief button when a scenario's debrief phase is available,
// and speaker-tagged bubbles (members passed to ChatView).

import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { ProjectionSnapshot } from '../store/projection'
import type { BusScenario } from '../rpc/types'
import type { ScenarioEntry } from '../scenario/scenarioStore'
import type { InteractionResponse } from '../rpc/types'
import { ChatView } from './ChatView'
import { Composer, type SlashCommand } from './Composer'
import { ApprovalPanel } from './ApprovalPanel'
import { GoalBar } from './GoalBar'
import { ModelSelect } from './ModelSelect'
import type { SettingsStore } from '../settings/settingsStore'
import styles from './ConversationRoot.module.css'

interface ConversationRootProps {
  snapshot: ProjectionSnapshot
  connection: 'connecting' | 'open' | 'closed' | 'error'
  theme: 'light' | 'dark'
  planMode: boolean
  scenarios: ScenarioEntry[]
  settingsStore: SettingsStore
  onSend: (text: string) => void
  onStop: () => void
  onTogglePlan: () => void
  onSlash: (cmd: SlashCommand) => void
  onSelectTool: (nodeId: string) => void
  onRespondApproval: (requestId: string, response: InteractionResponse) => void
  onPickScenario: (scenario: BusScenario) => void
  onDebrief: () => void
}

export function ConversationRoot({
  snapshot,
  connection,
  theme,
  planMode,
  scenarios,
  settingsStore,
  onSend,
  onStop,
  onTogglePlan,
  onSlash,
  onSelectTool,
  onRespondApproval,
  onPickScenario,
  onDebrief,
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
  const scenarioActive = snapshot.currentScenario !== null
  const heroChips = scenarios.slice(0, 5)

  return (
    <div className={styles.root}>
      <header className={styles.header}>
        <span className={styles.brand}>
          {scenarioActive && snapshot.currentScenario !== null
            ? snapshot.currentScenario.name
            : t('app.title')}
        </span>
        <div className={styles.headerRight}>
          <ModelSelect store={settingsStore} />
          <span className={`${styles.status} ${styles[`status_${connection}`]}`}>
            {statusText}
            {snapshot.paradigm !== 're_act' && (
              <span className={styles.paradigm}> · {snapshot.paradigm}</span>
            )}
          </span>
        </div>
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
            {heroChips.length > 0 && (
              <div className={styles.heroChips}>
                {heroChips.map((e) => (
                  <button
                    key={e.scenario.id}
                    className={styles.heroChip}
                    onClick={() => onPickScenario(e.scenario)}
                  >
                    <span className={styles.heroIcon}>{e.scenario.icon ?? '◆'}</span>
                    <span>{e.scenario.name}</span>
                  </button>
                ))}
              </div>
            )}
          </div>
        ) : (
          <ChatView
            nodes={snapshot.nodes}
            turnActive={snapshot.turnActive}
            selectedToolNodeId={snapshot.selectedToolNodeId}
            onSelectTool={onSelectTool}
            theme={theme}
            members={snapshot.scenarioMembers}
          />
        )}
      </div>

      {snapshot.debriefAvailable && !snapshot.turnActive && (
        <div className={styles.debriefBar}>
          <button className={styles.debriefBtn} onClick={onDebrief}>
            {snapshot.currentScenario?.debrief?.button_label ?? t('scenario.debrief')}
          </button>
        </div>
      )}

      <ApprovalPanel
        current={snapshot.currentApproval}
        queueDepth={snapshot.approvalQueueDepth}
        onRespond={onRespondApproval}
      />

      <GoalBar working={snapshot.working} />

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
