// App — assembles the RPC client + projection/session/scenario stores, drives
// the connection lifecycle (session/create on open), and mounts the AppFrame
// shell. W2 wires: tool/plan/approval consumes (in projection), the details
// rail, plan-mode toggle, Stop, slash commands, and the parallel approval
// queue. W3 wires: the scenario library (scenario/* CRUD + local presets),
// the scenario modal flow (picker → topic intake → group chat → debrief),
// the React scenario editor, speaker-tagged bubbles (members on the
// snapshot), hero chips, and smartSend (group/run vs turn/run).

import { useEffect, useMemo, useRef, useState } from 'react'
import { OneAiRpcClient } from './rpc/client'
import type { ConnectionStatus } from './rpc/client'
import {
  ProjectionStore,
  SessionListStore,
  useProjection,
  useSessionList,
} from './store/projection'
import type { BusLocale, BusScenario, InteractionResponse } from './rpc/types'
import { AppFrame } from './layout/AppFrame'
import { ConversationRoot } from './conversation/ConversationRoot'
import { type SlashCommand, type InteractionMode, nextMode } from './conversation/Composer'
import { DetailsPanel, type DetailsTab } from './details/DetailsPanel'
import { SidebarRoot } from './sidebar/SidebarRoot'
import {
  ScenarioListStore,
  useScenarioList,
} from './scenario/scenarioStore'
import { ScenarioPicker } from './scenario/ScenarioPicker'
import { TopicIntake } from './scenario/TopicIntake'
import { ScenarioEditor } from './scenario/ScenarioEditor'
import { SessionRenameModal } from './scenario/SessionRenameModal'
import { ConfirmDialog } from './components/ConfirmDialog'
import { workspaceStore, useWorkspace } from './workspace/workspaceStore'
import { SettingsRoot } from './settings/SettingsRoot'
import { SettingsStore } from './settings/settingsStore'
import { SkillsModal } from './skills/SkillsModal'
import { DomainPackModal } from './domainpack/DomainPackModal'
import { localeStore, useLocale } from './i18n'
import './theme/markdown.css'
import styles from './App.module.css'
import type { Theme } from './theme'
import { readInitialTheme, THEME_STORAGE_KEY } from './theme'

// When `VITE_APP_SERVER_URL` is set (e.g. `npm run dev` pointing at a separate
// `oneai app-server --listen ws://127.0.0.1:8787`), use it. Otherwise derive
// from the page origin so the SAME built dist works on any host:port — i.e.
// the `oneai web` single-port server (SPA + /ws same-origin) needs no rebuild.
const APP_SERVER_URL =
  (import.meta.env.VITE_APP_SERVER_URL as string | undefined) ??
  (() => {
    if (typeof window === 'undefined') return 'ws://127.0.0.1:8787'
    const proto = window.location.protocol === 'https:' ? 'wss' : 'ws'
    return `${proto}://${window.location.host}/ws`
  })()

interface FramePrefs {
  sidebarWidth: number
  detailsWidth: number
  detailsOpen: boolean
}


type ModalState =
  | { kind: 'picker' }
  | { kind: 'intake'; scenario: BusScenario }
  | { kind: 'editor'; scenario: BusScenario | null }
  | { kind: 'renameSession'; id: string; title: string }
  | { kind: 'confirmDeleteSession'; id: string }
  | { kind: 'workspaceSwitchBlocked' }
  | { kind: 'settings' }
  | { kind: 'skills' }
  | { kind: 'domainpack' }
  | null

export default function App(): React.ReactNode {
  const rpc = useMemo(() => new OneAiRpcClient(APP_SERVER_URL), [])
  const projection = useMemo(() => new ProjectionStore(rpc), [rpc])
  const sessionList = useMemo(() => new SessionListStore(rpc), [rpc])
  const scenarioList = useMemo(() => new ScenarioListStore(rpc), [rpc])
  const settings = useMemo(() => new SettingsStore(rpc), [rpc])
  const [status, setStatus] = useState<ConnectionStatus>('closed')
  const [theme, setTheme] = useState<Theme>(() => readInitialTheme().theme)
  // Tracks whether the user has explicitly chosen a theme (toggle). Until they
  // do, the theme follows the OS `prefers-color-scheme` live — so an OS flip
  // after load still re-themes the app. Once explicit, we persist + stop
  // following (explicit choice wins, mirrors dsh's behavior).
  const themeExplicit = useRef<boolean>(readInitialTheme().explicit)
  const { locale, t } = useLocale()
  // Interaction mode (Normal → Auto → Plan), mirroring the TUI's
  // InteractionMode. Replaces the old binary planMode toggle. The live
  // paradigm (from paradigm_switch yields) is surfaced separately by the
  // Composer chip when the model auto-switches to reflect/explore.
  const [interactionMode, setInteractionMode] = useState<InteractionMode>('normal')
  const [modal, setModal] = useState<ModalState>(null)
  const prefsRef = useRef<FramePrefs>({
    sidebarWidth: 264,
    detailsWidth: 360,
    detailsOpen: false,
  })
  const [prefs, setPrefs] = useState<FramePrefs>(prefsRef.current)
  const [detailsTab, setDetailsTab] = useState<DetailsTab>('tool')
  // Mobile nav drawer (controlled by App so pick handlers can close it).
  const [drawerOpen, setDrawerOpen] = useState(false)
  // Workspace dropdown (popover, not a modal) — open only on the welcome
  // (empty) state; mid-conversation the chip shows a "start a new chat" prompt.
  const [workspaceDropdownOpen, setWorkspaceDropdownOpen] = useState(false)

  // Connect on mount; dispose on unmount.
  useEffect(() => {
    const offStatus = rpc.onStatus(setStatus)
    const offEvents = projection.attach()
    rpc.connect()
    return () => {
      offStatus()
      offEvents()
      rpc.dispose()
    }
  }, [rpc, projection])

  // Apply the theme attribute. Persistence happens only on explicit toggle
  // (toggleTheme) — an OS-derived theme is intentionally NOT persisted so
  // live OS-theme following keeps working until the user picks one.
  useEffect(() => {
    document.body.setAttribute('data-oneai-theme', theme)
  }, [theme])

  // Follow the OS theme live while the user hasn't explicitly chosen one.
  useEffect(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return
    const mq = window.matchMedia('(prefers-color-scheme: dark)')
    const onChange = (e: MediaQueryListEvent) => {
      if (!themeExplicit.current) setTheme(e.matches ? 'dark' : 'light')
    }
    mq.addEventListener('change', onChange)
    return () => mq.removeEventListener('change', onChange)
  }, [])

  // When the socket opens: ensure we have a session, then load the lists.
  const creatingRef = useRef(false)
  useEffect(() => {
    if (status !== 'open') return
    if (creatingRef.current) return
    creatingRef.current = true
    ;(async () => {
      try {
        // Bind the persisted workspace (if any) so a user who reloads with a
        // workspace selected starts their first chat in it without re-picking.
        const ws = workspaceStore.getSnapshot().current ?? undefined
        await rpc.call<{ id?: string; workspace?: string }, { id?: string }>(
          'session/create',
          { workspace: ws },
        )
      } catch {
        /* engine may already have a live session — ignore */
      }
      await sessionList.refresh()
      await scenarioList.refresh(locale)
      await settings.refresh()
      creatingRef.current = false
    })()
  }, [status, rpc, sessionList, scenarioList, settings, locale])

  // Refresh the session list after each completed turn (new title/counts).
  useEffect(() => {
    const off = rpc.onEvent((y) => {
      if (y.kind === 'turn_complete' || y.kind === 'session_deleted' || y.kind === 'session_cleared') {
        void sessionList.refresh()
      }
    })
    return off
  }, [rpc, sessionList])

  // Re-merge presets when the locale flips (presets are locale-bound).
  useEffect(() => {
    void scenarioList.refresh(locale)
  }, [locale, scenarioList])

  const snap = useProjection(projection)
  const sessions = useSessionList(sessionList)
  const scenarios = useScenarioList(scenarioList)
  const workspaceSnap = useWorkspace()

  // Resolve the active session's display title for the conversation header.
  // The title is engine-authoritative (a `session/rename` RPC persists it), so
  // no client-side override remains.
  const activeSession = sessions.find((s) => s.id === snap.sessionId) ?? null
  const sessionTitle = activeSession !== null ? activeSession.title : null

  // ── handlers ────────────────────────────────────────────────────────────────
  const currentWorkspacePath = workspaceSnap.current
  const workspaceLabel =
    currentWorkspacePath === null
      ? null
      : workspaceStore.labelFor(currentWorkspacePath)

  const handleNewSession = async () => {
    // New single-agent chat — leave any active scenario. Binds the currently
    // selected workspace (deepseek-harness parity) so the agent operates in
    // that directory (engine persists metadata["workspace"] + Part C cwd).
    // Read the workspace LIVE from the store — a render-captured const would
    // be stale when this fires right after `workspaceStore.setCurrent` in the
    // same synchronous handler (the picker auto-bind path).
    projection.exitScenario()
    setDrawerOpen(false)
    try {
      const ws = workspaceStore.getSnapshot().current ?? undefined
      await rpc.call<{ id?: string; workspace?: string }, { id?: string }>(
        'session/create',
        { workspace: ws },
      )
      await sessionList.refresh()
    } catch {
      /* offline */
    }
  }
  const handlePickSession = (id: string) => {
    projection.exitScenario()
    setDrawerOpen(false)
    // Sync the workspace chip to the loaded session's bound working dir
    // (persisted in conversation.metadata["workspace"]) — otherwise the chip
    // keeps showing the previously-picked workspace and reads as stale. Absent
    // ⇒ no-workspace (the app-global cwd). This also makes a subsequent "new
    // chat" inherit the now-active workspace, matching deepseek-harness parity.
    const picked = sessions.find((s) => s.id === id) ?? null
    workspaceStore.setCurrent(picked?.workspace ?? null)
    void projection.loadSession(id)
  }
  const handleRenameSession = (id: string, currentTitle: string) => {
    setModal({ kind: 'renameSession', id, title: currentTitle })
  }
  const handleArchiveSession = async (id: string) => {
    try {
      await rpc.call<{ id: string; archived: boolean }, { ok: boolean }>(
        'session/archive',
        { id, archived: true },
      )
    } catch {
      /* offline — keep the list as-is */
    }
    await sessionList.refresh()
  }
  const handleUnarchiveSession = async (id: string) => {
    try {
      await rpc.call<{ id: string; archived: boolean }, { ok: boolean }>(
        'session/archive',
        { id, archived: false },
      )
    } catch {
      /* offline */
    }
    await sessionList.refresh()
  }
  const handleDeleteSession = (id: string) => {
    // In-page confirm (no browser `window.confirm`). The actual delete runs on
    // confirm — see `confirmDeleteSession`.
    setModal({ kind: 'confirmDeleteSession', id })
  }
  const performDeleteSession = async (id: string) => {
    setModal(null)
    try {
      await rpc.call<{ id: string }, { ok: boolean }>('session/delete', { id })
    } catch {
      /* offline — the row lingers; refresh will drop it when the engine
         catches up, or it stays until reconnect */
    }
    await sessionList.refresh()
  }

  // Workspace chip click: on the welcome/empty page open the picker; mid-
  // conversation, prompt that switching needs a new chat (deepseek parity).
  const handleWorkspaceClick = () => {
    if (snap.nodes.length === 0) {
      setWorkspaceDropdownOpen(true)
    } else {
      setModal({ kind: 'workspaceSwitchBlocked' })
    }
  }
  // Dropdown "select" — set the chosen workspace current and bind it to a
  // fresh session (the welcome page is empty, so re-creating is lossless).
  // An empty path means "no workspace" (the default app-global cwd). The
  // dropdown closes itself via its own onClose before calling onSelect.
  const handleWorkspaceSelect = (path: string) => {
    workspaceStore.setCurrent(path.length > 0 ? path : null)
    void handleNewSession()
  }
  // "添加工作区" — close the dropdown, ask the sidecar to show the native OS
  // folder picker (`osascript choose folder` / `zenity` / `kdialog`), and on a
  // real path upsert + bind to a fresh session. Browsers can't get a host
  // path; the local sidecar can (deepseek-harness parity).
  const handleAddWorkspace = async () => {
    setWorkspaceDropdownOpen(false)
    try {
      const res = await rpc.call<unknown, { path?: string | null }>(
        'dialog/pick_directory',
        {},
      )
      const p = res?.path ?? null
      if (p && p.length > 0) {
        workspaceStore.upsert(p)
        handleWorkspaceSelect(p)
      }
    } catch (e) {
      // Most common cause: the running sidecar is a stale binary that
      // predates the `dialog/pick_directory` RPC (→ "method not found").
      // Rebuild the sidecar (`cargo build --release -p oneai-cli`) and
      // restart. Log the detail so devtools shows which.
      // eslint-disable-next-line no-console
      console.error('[workspace] dialog/pick_directory failed:', e)
      alert(
        t('workspace.pickFailed') +
          '\n' +
          (e instanceof Error ? e.message : String(e)),
      )
    }
  }
  const toggleTheme = () => {
    themeExplicit.current = true
    setTheme((th) => {
      const next = th === 'dark' ? 'light' : 'dark'
      try {
        localStorage.setItem(THEME_STORAGE_KEY, next)
      } catch {
        /* ignore */
      }
      return next
    })
  }
  const toggleLocale = () =>
    localeStore.setLocale(localeStore.getLocale() === 'zh' ? 'en' : 'zh')

  // Selecting a tool node opens the details rail and records the selection.
  const handleSelectTool = (nodeId: string) => {
    projection.selectTool(nodeId)
    setDetailsTab('tool')
    if (!prefs.detailsOpen) {
      setPrefs({ ...prefs, detailsOpen: true })
    }
  }
  const handleCloseDetails = () => {
    projection.clearSelection()
    setPrefs({ ...prefs, detailsOpen: false })
  }

  // Apply an interaction mode: Normal (default approval, re_act), Auto
  // (silently allow tools — frontend short-circuits the approval bar), Plan
  // (block tool execution — engine plan_mode + paradigm plan). Side-effects
  // fire on every transition so engine + projection stay in sync with the UI.
  const applyMode = (mode: InteractionMode) => {
    setInteractionMode(mode)
    const plan = mode === 'plan'
    void projection.setPlanMode(plan)
    void projection.switchParadigm(plan ? 'plan' : 're_act')
    projection.setAutoApprove(mode === 'auto')
  }

  const handleCycleMode = () => {
    applyMode(nextMode(interactionMode))
  }

  const handleSlash = (cmd: SlashCommand) => {
    if (cmd === 'plan') {
      applyMode(interactionMode === 'plan' ? 'normal' : 'plan')
    } else if (cmd === 'clear') {
      projection.exitScenario()
      void projection.clearSession()
    } else if (cmd === 'compact') {
      void projection.compact(10)
    } else if (cmd === 'scenario') {
      setModal({ kind: 'picker' })
    } else if (cmd === 'newScenario') {
      setModal({ kind: 'editor', scenario: null })
    } else if (cmd === 'editScenario') {
      if (snap.currentScenario !== null) {
        setModal({ kind: 'editor', scenario: snap.currentScenario })
      } else {
        setModal({ kind: 'picker' })
      }
    } else if (cmd === 'trajectory') {
      setDetailsTab('trajectory')
      setPrefs({ ...prefs, detailsOpen: true })
    } else if (cmd === 'settings') {
      setModal({ kind: 'settings' })
    } else if (cmd === 'skills') {
      setModal({ kind: 'skills' })
    } else if (cmd === 'domainpack') {
      setModal({ kind: 'domainpack' })
    }
  }

  const handleRespondApproval = (requestId: string, response: InteractionResponse) => {
    void projection.respondApproval(requestId, response)
  }

  // §B5 — durable host allow/deny from the NetworkApproval panel. The persist
  // is best-effort (projection swallows network errors); the ApprovalPanel
  // proceeds/aborts regardless so the turn never wedges on a failed RPC.
  const handleAllowAlways = (host: string) => projection.admitHost(host)
  const handleDenyAlways = (host: string) => projection.denyHost(host)

  // ── scenario handlers ───────────────────────────────────────────────────────
  const handlePickScenario = (scenario: BusScenario) => {
    setDrawerOpen(false)
    const hasTopics = (scenario.topic_fields ?? []).length > 0
    if (hasTopics) {
      setModal({ kind: 'intake', scenario })
    } else {
      setModal(null)
      void projection.startScenario(scenario, {}, locale as BusLocale)
    }
  }
  const handleIntakeSubmit = (values: Record<string, string>) => {
    if (modal?.kind === 'intake') {
      const scenario = modal.scenario
      setModal(null)
      void projection.startScenario(scenario, values, locale as BusLocale)
    }
  }
  const handleScenarioSaved = async (_id: string) => {
    setModal(null)
    await scenarioList.refresh(locale)
  }
  const handleScenarioDeleted = async (_id: string) => {
    setModal(null)
    await scenarioList.refresh(locale)
  }
  const handleDebrief = () => {
    void projection.debrief(t('scenario.debrief'))
  }

  const selectedToolNode =
    snap.selectedToolNodeId !== null
      ? snap.nodes.find((n) => n.id === snap.selectedToolNodeId) ?? null
      : null

  return (
    <>
      <AppFrame
        sidebar={
          <SidebarRoot
            sessions={sessions}
            currentSessionId={snap.sessionId}
            theme={theme}
            scenarios={scenarios}
            onNewSession={handleNewSession}
            onPickSession={handlePickSession}
            onRenameSession={handleRenameSession}
            onArchiveSession={handleArchiveSession}
            onUnarchiveSession={handleUnarchiveSession}
            onDeleteSession={handleDeleteSession}
            onToggleTheme={toggleTheme}
            onToggleLocale={toggleLocale}
            onPickScenario={handlePickScenario}
            onNewScenario={() => setModal({ kind: 'editor', scenario: null })}
            onEditScenario={(s) => setModal({ kind: 'editor', scenario: s })}
            onOpenSettings={() => setModal({ kind: 'settings' })}
          />
        }
        center={
          <ConversationRoot
            snapshot={snap}
            connection={status}
            theme={theme}
            mode={interactionMode}
            scenarios={scenarios}
            settingsStore={settings}
            onSend={(text, images) => void projection.sendMessage(text, images)}
            onStop={() => void projection.cancelTurn()}
            onCycleMode={handleCycleMode}
            onSlash={handleSlash}
            onSelectTool={handleSelectTool}
            onRespondApproval={handleRespondApproval}
            onAllowAlways={handleAllowAlways}
            onDenyAlways={handleDenyAlways}
            onPickScenario={handlePickScenario}
            onDebrief={handleDebrief}
            onCancelBackground={(taskId) => void projection.cancelBackgroundTask(taskId)}
            onSubmitFeedback={(nodeId, kind, text) =>
              void projection.submitFeedback(nodeId, kind, text)
            }
            sessionTitle={sessionTitle}
            workspaceLabel={workspaceLabel}
            onWorkspaceClick={handleWorkspaceClick}
            workspaceDropdownOpen={workspaceDropdownOpen}
            onCloseWorkspaceDropdown={() => setWorkspaceDropdownOpen(false)}
            onSelectWorkspace={handleWorkspaceSelect}
            onAddWorkspace={handleAddWorkspace}
            onOpenSettings={() => setModal({ kind: 'settings' })}
          />
        }
        details={
          prefs.detailsOpen ? (
            <DetailsPanel
              node={selectedToolNode}
              tab={detailsTab}
              onTabChange={setDetailsTab}
              trajectory={snap.trajectory}
              usage={snap.usage}
              subagents={snap.subagents}
              turnTimings={snap.turnTimings}
              onClose={handleCloseDetails}
            />
          ) : undefined
        }
        prefs={prefs}
        onPrefsChange={setPrefs}
        drawerOpen={drawerOpen}
        onDrawerOpenChange={setDrawerOpen}
        mobileBar={
          <>
            <span className={styles.mobileTitle}>{t('app.title')}</span>
            <button
              className={styles.mobileThemeBtn}
              onClick={toggleTheme}
              title={t('theme.toggle')}
              aria-label={t('theme.toggle')}
            >
              {theme === 'dark' ? '☀' : '☾'}
            </button>
          </>
        }
      />
      {modal?.kind === 'picker' && (
        <ScenarioPicker
          entries={scenarios}
          onPick={handlePickScenario}
          onEdit={(s) => setModal({ kind: 'editor', scenario: s })}
          onNew={() => setModal({ kind: 'editor', scenario: null })}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.kind === 'intake' && (
        <TopicIntake
          scenario={modal.scenario}
          onSubmit={handleIntakeSubmit}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.kind === 'editor' && (
        <ScenarioEditor
          scenario={modal.scenario}
          store={scenarioList}
          onSaved={handleScenarioSaved}
          onDeleted={handleScenarioDeleted}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.kind === 'renameSession' && (
        <SessionRenameModal
          currentTitle={modal.title}
          onSubmit={async (title) => {
            try {
              await rpc.call<{ id: string; title: string }, { ok: boolean }>(
                'session/rename',
                { id: modal.id, title },
              )
            } catch {
              /* offline — keep the prior title */
            }
            await sessionList.refresh()
            setModal(null)
          }}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.kind === 'confirmDeleteSession' && (
        <ConfirmDialog
          title={t('session.delete')}
          message={t('session.confirmDelete')}
          confirmLabel={t('session.delete')}
          cancelLabel={t('scenario.cancel')}
          danger
          onConfirm={() => void performDeleteSession(modal.id)}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.kind === 'workspaceSwitchBlocked' && (
        <ConfirmDialog
          title={t('workspace.switchBlocked')}
          message={t('workspace.switchBlockedHint')}
          confirmLabel={t('workspace.newChat')}
          cancelLabel={t('scenario.cancel')}
          onConfirm={() => {
            setModal(null)
            void handleNewSession()
          }}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.kind === 'settings' && (
        <SettingsRoot
          store={settings}
          theme={theme}
          locale={locale}
          planMode={interactionMode === 'plan'}
          connection={status}
          onToggleTheme={toggleTheme}
          onToggleLocale={toggleLocale}
          onTogglePlan={() => applyMode(interactionMode === 'plan' ? 'normal' : 'plan')}
          hostOps={{
            list: () => projection.listHosts(),
            remove: (host) => projection.removeHost(host),
            removeDenied: (host) => projection.removeDeniedHost(host),
          }}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.kind === 'skills' && (
        <SkillsModal store={settings} onClose={() => setModal(null)} />
      )}
      {modal?.kind === 'domainpack' && (
        <DomainPackModal store={settings} onClose={() => setModal(null)} />
      )}
    </>
  )
}

