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
import { SettingsRoot } from './settings/SettingsRoot'
import { SettingsStore } from './settings/settingsStore'
import { SkillsModal } from './skills/SkillsModal'
import { DomainPackModal } from './domainpack/DomainPackModal'
import { sessionMetaStore, useSessionMeta } from './store/sessionMeta'
import { localeStore, useLocale } from './i18n'
import './theme/markdown.css'
import styles from './App.module.css'
import type { Theme } from './theme'
import { readInitialTheme, THEME_STORAGE_KEY } from './theme'

const APP_SERVER_URL =
  (import.meta.env.VITE_APP_SERVER_URL as string | undefined) ?? 'ws://127.0.0.1:8787'

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
        await rpc.call<{ id?: string }, { id?: string }>('session/create', {})
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
  const sessionMeta = useSessionMeta()

  // Resolve the active session's display title for the conversation header
  // (client-side rename override wins over the engine's auto-derived title).
  const activeSession = sessions.find((s) => s.id === snap.sessionId) ?? null
  const sessionTitle =
    activeSession !== null
      ? (sessionMeta.titles[activeSession.id] ?? activeSession.title)
      : null

  // ── handlers ────────────────────────────────────────────────────────────────
  const handleNewSession = async () => {
    // New single-agent chat — leave any active scenario.
    projection.exitScenario()
    setDrawerOpen(false)
    try {
      await rpc.call<{ id?: string }, { id?: string }>('session/create', {})
      await sessionList.refresh()
    } catch {
      /* offline */
    }
  }
  const handlePickSession = (id: string) => {
    projection.exitScenario()
    setDrawerOpen(false)
    void projection.loadSession(id)
  }
  const handleRenameSession = (id: string, currentTitle: string) => {
    setModal({ kind: 'renameSession', id, title: currentTitle })
  }
  const handleArchiveSession = (id: string) => {
    sessionMetaStore.archive(id)
  }
  const handleUnarchiveSession = (id: string) => {
    sessionMetaStore.unarchive(id)
  }
  const handleDeleteSession = async (id: string) => {
    if (!window.confirm(t('session.confirmDelete'))) return
    try {
      await rpc.call<{ id: string }, { ok: boolean }>('session/delete', { id })
    } catch {
      /* offline — still clear local meta so the row doesn't linger broken */
    }
    sessionMetaStore.forget(id)
    await sessionList.refresh()
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
            sessionMeta={sessionMeta}
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
            onPickScenario={handlePickScenario}
            onDebrief={handleDebrief}
            onSubmitFeedback={(nodeId, kind, text) =>
              void projection.submitFeedback(nodeId, kind, text)
            }
            sessionTitle={sessionTitle}
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
          onSubmit={(title) => {
            sessionMetaStore.rename(modal.id, title)
            setModal(null)
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

