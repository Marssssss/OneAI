// ConversationRoot — the center column: header chrome + chat scroll + approval
// bar + composer.
//
// Resident skeleton (stays mounted across empty/populated session), mirroring
// dsh's `ConversationRoot`. W2 adds: ApprovalPanel (composer-takeover when an
// approval is queued), tool/plan node dispatch, plan-mode chip, stop button,
// and the details-rail selection wiring. W3 adds: hero scenario chips on the
// empty state, a debrief button when a scenario's debrief phase is available,
// and speaker-tagged bubbles (members passed to ChatView).

import { useState } from 'react'
import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { ProjectionSnapshot } from '../store/projection'
import type { BusScenario, ContentBlock, FeedbackKind } from '../rpc/types'
import type { ScenarioEntry } from '../scenario/scenarioStore'
import type { InteractionResponse } from '../rpc/types'
import { ChatView } from './ChatView'
import { BackgroundTasksBar } from './BackgroundTasksBar'
import { Composer, type InteractionMode } from './Composer'
import type { SlashInvocation } from './slashCommands'
import { ApprovalPanel } from './ApprovalPanel'
import { GoalBar } from './GoalBar'
import { TrajectoryExplorer } from '../trajectory/TrajectoryExplorer'
import type { SettingsStore } from '../settings/settingsStore'
import { useSettings } from '../settings/settingsStore'
import styles from './ConversationRoot.module.css'

// localStorage flag for the first-run API-key onboarding banner (issue #33):
// once dismissed (× or "open settings"), the banner never returns, so it
// doesn't nag the user on every launch.
const ONBOARDING_DISMISSED_KEY = 'oneai.onboarding.apikey.dismissed'
function readOnboardingDismissed(): boolean {
  try {
    return localStorage.getItem(ONBOARDING_DISMISSED_KEY) === '1'
  } catch {
    return false
  }
}
function writeOnboardingDismissed(): void {
  try {
    localStorage.setItem(ONBOARDING_DISMISSED_KEY, '1')
  } catch {
    /* ignore — private mode etc. */
  }
}

interface ConversationRootProps {
  snapshot: ProjectionSnapshot
  connection: 'connecting' | 'open' | 'closed' | 'error'
  theme: 'light' | 'dark'
  /** Issue #40: which center-column surface is shown — conversation or the
   *  resident trajectory view. Toggled from the header-right button. */
  viewMode: 'conversation' | 'trajectory'
  onToggleView: () => void
  mode: InteractionMode
  scenarios: ScenarioEntry[]
  settingsStore: SettingsStore
  /** Send a user message. `images` (W4 attachments) are image content blocks
   * dragged/dropped/pasted into the composer; group-chat mode ignores them
   * (its bus directive is plain-text only). */
  onSend: (text: string, images?: ContentBlock[]) => void
  onStop: () => void
  onCycleMode: () => void
  /** Slash dispatch (issue #39 TUI-aligned registry — see slashCommands.ts). */
  onSlash: (invocation: SlashInvocation) => void
  onUnknownSlash: (label: string) => void
  onSelectTool: (nodeId: string) => void
  onRespondApproval: (requestId: string, response: InteractionResponse) => void
  /** §B5 — persist a host admission cross-session ("always") via `host/allow`,
   *  then the caller proceeds. Best-effort. */
  onAllowAlways?: (host: string) => Promise<void>
  /** §B5 — persist a host denial cross-session via `host/deny`, then abort. */
  onDenyAlways?: (host: string) => Promise<void>
  onPickScenario: (scenario: BusScenario) => void
  onDebrief: () => void
  /** Cancel one in-flight background sub-agent by task_id (the ✕ button on
   *  each active card in the `BackgroundTasksBar`). Reaches the app-level
   *  `BackgroundTaskRegistry` so it works even after the delegating turn
   *  ended (gap-1 fix). */
  onCancelBackground?: (taskId: string) => void
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
  /** Open the settings modal — used by the first-run API-key onboarding
   *  banner (issue #33). */
  onOpenSettings: () => void
}

export function ConversationRoot({
  snapshot,
  connection,
  theme,
  viewMode,
  onToggleView,
  mode,
  scenarios,
  settingsStore,
  onSend,
  onStop,
  onCycleMode,
  onSlash,
  onUnknownSlash,
  onSelectTool,
  onRespondApproval,
  onAllowAlways,
  onDenyAlways,
  onPickScenario,
  onDebrief,
  onCancelBackground,
  onSubmitFeedback,
  sessionTitle,
  workspaceLabel,
  onWorkspaceClick,
  workspaceDropdownOpen,
  onCloseWorkspaceDropdown,
  onSelectWorkspace,
  onAddWorkspace,
  onOpenSettings,
}: ConversationRootProps): ReactNode {
  const { t } = useLocale()
  const settings = useSettings(settingsStore)
  // First-run onboarding (issue #33): prompt "添加一个 API Key 开始使用" once
  // when no provider is configured. Dismissed permanently via localStorage so
  // it never nags — both × and "open settings" set the flag.
  const [onboardingDismissed, setOnboardingDismissed] = useState(readOnboardingDismissed)
  const noProvider = settings.providers.length === 0
  const dismissOnboarding = () => {
    writeOnboardingDismissed()
    setOnboardingDismissed(true)
  }
  const openSettingsFromOnboarding = () => {
    writeOnboardingDismissed()
    setOnboardingDismissed(true)
    onOpenSettings()
  }
  const statusText =
    connection === 'open'
      ? t('status.open')
      : connection === 'connecting'
        ? t('status.connecting')
        : connection === 'error'
          ? t('status.error')
          : t('status.closed')

  const empty = snapshot.nodes.length === 0
  const showOnboarding = empty && noProvider && !onboardingDismissed
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
      onUnknownSlash={onUnknownSlash}
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
          {/* Issue #40: resident trajectory toggle. In conversation mode the
              button reads "轨迹"; in trajectory mode it reads "对话". The
              app-server connection status used to live here but conflicted
              with this button — it now floats at the bottom-right corner. */}
          <button className={styles.viewToggle} onClick={onToggleView} title={viewMode === 'conversation' ? t('trajectory.tab') : t('trajectory.conversationTab')}>
            {viewMode === 'conversation' ? t('trajectory.tab') : t('trajectory.conversationTab')}
          </button>
        </div>
      </header>

      <div className={styles.body}>
        {snapshot.lastError !== null && (
          <div className={styles.errorBanner} title={snapshot.lastError}>
            {t('chat.error')}: {snapshot.lastError}
          </div>
        )}
        <BackgroundTasksBar tasks={snapshot.backgroundTasks} onCancel={onCancelBackground} />
        {viewMode === 'trajectory' ? (
          <TrajectoryExplorer snapshot={snapshot} />
        ) : empty ? (
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
            {showOnboarding && (
              <div className={styles.onboarding}>
                <span className={styles.onboardingText}>
                  {t('onboarding.apikey.title')}
                </span>
                <button
                  className={styles.onboardingAction}
                  onClick={openSettingsFromOnboarding}
                >
                  {t('onboarding.apikey.action')}
                </button>
                <button
                  className={styles.onboardingClose}
                  onClick={dismissOnboarding}
                  aria-label={t('onboarding.apikey.dismiss')}
                  title={t('onboarding.apikey.dismiss')}
                >
                  ×
                </button>
              </div>
            )}
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

      {/* App-server connection status — a floating badge at the bottom-right
          corner (moved out of the header where it collided with the trajectory
          toggle, issue #40 follow-up). */}
      <div className={styles.statusCorner}>
        <span className={`${styles.status} ${styles[`status_${connection}`]}`}>
          {statusText}
          {snapshot.paradigm !== 're_act' && (
            <span className={styles.paradigm}> · {snapshot.paradigm}</span>
          )}
        </span>
      </div>

      {!empty && <div className={styles.composer}>{composerEl}</div>}
    </div>
  )
}
