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
import type { BusScenario, ContentBlock, FeedbackKind } from '../rpc/types'
import type { ScenarioEntry } from '../scenario/scenarioStore'
import type { InteractionResponse } from '../rpc/types'
import { ChatView } from './ChatView'
import { BackgroundTasksBar } from './BackgroundTasksBar'
import { Composer, type SlashCommand, type InteractionMode } from './Composer'
import { ApprovalPanel } from './ApprovalPanel'
import { GoalBar } from './GoalBar'
import type { SettingsStore } from '../settings/settingsStore'
import styles from './ConversationRoot.module.css'

interface ConversationRootProps {
  snapshot: ProjectionSnapshot
  connection: 'connecting' | 'open' | 'closed' | 'error'
  theme: 'light' | 'dark'
  mode: InteractionMode
  scenarios: ScenarioEntry[]
  settingsStore: SettingsStore
  /** Send a user message. `images` (W4 attachments) are image content blocks
   * dragged/dropped/pasted into the composer; group-chat mode ignores them
   * (its bus directive is plain-text only). */
  onSend: (text: string, images?: ContentBlock[]) => void
  onStop: () => void
  onCycleMode: () => void
  onSlash: (cmd: SlashCommand) => void
  onSelectTool: (nodeId: string) => void
  onRespondApproval: (requestId: string, response: InteractionResponse) => void
  /** §B5 — persist a host admission cross-session ("always") via `host/allow`,
   *  then the caller proceeds. Best-effort. */
  onAllowAlways?: (host: string) => Promise<void>
  /** §B5 — persist a host denial cross-session via `host/deny`, then abort. */
  onDenyAlways?: (host: string) => Promise<void>
  onPickScenario: (scenario: BusScenario) => void
  onDebrief: () => void
  /** §W4 B4 — record a 👍/👎/note against one assistant message node. */
  onSubmitFeedback: (nodeId: string, kind: FeedbackKind, text?: string) => void
  /** Title of the currently-active session (client-overridden title wins),
   * or null when no session is active. Shown in the header only while the
   * user is in a conversation (nodes on screen) — the "OneAI" wordmark is
   * never shown. */
  sessionTitle: string | null
  /** The workspace chip label (alias), or null when no workspace is selected.
   *  The composer renders the picker button left of the mode chip. */
  workspaceLabel: string | null
  /** Click handler for the workspace chip — the caller decides dropdown
   *  (empty state) vs. "start a new chat" prompt (mid-conversation). */
  onWorkspaceClick: () => void
  /** Whether the workspace dropdown (popover) is open. */
  workspaceDropdownOpen: boolean
  onCloseWorkspaceDropdown: () => void
  onSelectWorkspace: (path: string) => void
  /** Open the native OS folder picker (App owns the RPC). */
  onAddWorkspace: () => void
}

export function ConversationRoot({
  snapshot,
  connection,
  theme,
  mode,
  scenarios,
  settingsStore,
  onSend,
  onStop,
  onCycleMode,
  onSlash,
  onSelectTool,
  onRespondApproval,
  onAllowAlways,
  onDenyAlways,
  onPickScenario,
  onDebrief,
  onSubmitFeedback,
  sessionTitle,
  workspaceLabel,
  onWorkspaceClick,
  workspaceDropdownOpen,
  onCloseWorkspaceDropdown,
  onSelectWorkspace,
  onAddWorkspace,
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
  // Header brand: scenario name while a scenario runs, otherwise the session
  // title — but only once the user is actually in a conversation (messages on
  // screen). The welcome/empty state shows no wordmark. The "OneAI" app title
  // is intentionally never rendered here.
  const brand =
    scenarioActive && snapshot.currentScenario !== null
      ? snapshot.currentScenario.name
      : empty
        ? ''
        : (sessionTitle ?? '')

  // Shared Composer element — rendered inline inside the welcome group on the
  // empty state (so the input sits directly under the brand + slogan, not
  // pinned to the viewport bottom) and in the usual bottom slot otherwise.
  // Only one branch mounts at a time, so reusing the element is safe.
  const composerEl = (
    <Composer
      placeholder={t('composer.placeholder')}
      sendLabel={t('composer.send')}
      stopLabel={t('composer.stop')}
      turnActive={snapshot.turnActive}
      paradigm={snapshot.paradigm}
      mode={mode}
      metrics={snapshot.metrics}
      settingsStore={settingsStore}
      attachmentsEnabled={snapshot.currentScenario === null}
      workspaceEnabled={snapshot.currentScenario === null}
      workspaceLabel={workspaceLabel}
      onSend={onSend}
      onStop={onStop}
      onCycleMode={onCycleMode}
      onSlash={onSlash}
      onWorkspaceClick={onWorkspaceClick}
      workspaceDropdownOpen={workspaceDropdownOpen}
      onCloseWorkspaceDropdown={onCloseWorkspaceDropdown}
      onSelectWorkspace={onSelectWorkspace}
      onAddWorkspace={onAddWorkspace}
    />
  )

  return (
    <div className={styles.root}>
      <header className={styles.header}>
        <span className={styles.brand}>{brand}</span>
        <div className={styles.headerRight}>
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
        <BackgroundTasksBar tasks={snapshot.backgroundTasks} />
        {empty ? (
          <div className={styles.empty}>
            <div className={styles.emptyBrand}>
              <img
                className={styles.emptyBrandPic}
                src="/brand/ic_pic_white.png"
                alt="OneAI"
                draggable={false}
                style={{ filter: theme === 'dark' ? 'invert(1)' : 'none' }}
              />
              <div className={styles.emptySlogan}>{t('chat.empty.slogan')}</div>
            </div>
            <div className={styles.emptyComposer}>{composerEl}</div>
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
            onSubmitFeedback={onSubmitFeedback}
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
        onAllowAlways={onAllowAlways}
        onDenyAlways={onDenyAlways}
      />

      <GoalBar working={snapshot.working} />

      {!empty && <div className={styles.composer}>{composerEl}</div>}
    </div>
  )
}
