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
import type { SlashCommand } from './conversation/Composer'
import { DetailsPanel } from './details/DetailsPanel'
import { SidebarRoot } from './sidebar/SidebarRoot'
import {
  ScenarioListStore,
  useScenarioList,
} from './scenario/scenarioStore'
import { ScenarioPicker } from './scenario/ScenarioPicker'
import { TopicIntake } from './scenario/TopicIntake'
import { ScenarioEditor } from './scenario/ScenarioEditor'
import { SessionRenameModal } from './scenario/SessionRenameModal'
import { sessionMetaStore, useSessionMeta } from './store/sessionMeta'
import { localeStore, useLocale } from './i18n'
import './theme/markdown.css'

const APP_SERVER_URL =
  (import.meta.env.VITE_APP_SERVER_URL as string | undefined) ?? 'ws://127.0.0.1:8787'

interface FramePrefs {
  sidebarWidth: number
  detailsWidth: number
  detailsOpen: boolean
}

type Theme = 'light' | 'dark'

type ModalState =
  | { kind: 'picker' }
  | { kind: 'intake'; scenario: BusScenario }
  | { kind: 'editor'; scenario: BusScenario | null }
  | { kind: 'renameSession'; id: string; title: string }
  | null

export default function App(): React.ReactNode {
  const rpc = useMemo(() => new OneAiRpcClient(APP_SERVER_URL), [])
  const projection = useMemo(() => new ProjectionStore(rpc), [rpc])
  const sessionList = useMemo(() => new SessionListStore(rpc), [rpc])
  const scenarioList = useMemo(() => new ScenarioListStore(rpc), [rpc])
  const [status, setStatus] = useState<ConnectionStatus>('closed')
  const [theme, setTheme] = useState<Theme>(() => readInitialTheme())
  const { locale, t } = useLocale()
  // planMode is the App-owned toggle synced via config/update; the live
  // paradigm (from paradigm_switch yields) also feeds the chip's "on" state.
  const [planMode, setPlanMode] = useState(false)
  const [modal, setModal] = useState<ModalState>(null)
  const prefsRef = useRef<FramePrefs>({
    sidebarWidth: 264,
    detailsWidth: 360,
    detailsOpen: false,
  })
  const [prefs, setPrefs] = useState<FramePrefs>(prefsRef.current)

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

  // Apply the theme attribute.
  useEffect(() => {
    document.body.setAttribute('data-oneai-theme', theme)
    try {
      localStorage.setItem('oneai-theme', theme)
    } catch {
      /* ignore */
    }
  }, [theme])

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
      creatingRef.current = false
    })()
  }, [status, rpc, sessionList, scenarioList, locale])

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

  // ── handlers ────────────────────────────────────────────────────────────────
  const handleNewSession = async () => {
    // New single-agent chat — leave any active scenario.
    projection.exitScenario()
    try {
      await rpc.call<{ id?: string }, { id?: string }>('session/create', {})
      await sessionList.refresh()
    } catch {
      /* offline */
    }
  }
  const handlePickSession = (id: string) => {
    projection.exitScenario()
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
  const toggleTheme = () => setTheme((th) => (th === 'dark' ? 'light' : 'dark'))
  const toggleLocale = () =>
    localeStore.setLocale(localeStore.getLocale() === 'zh' ? 'en' : 'zh')

  // Selecting a tool node opens the details rail and records the selection.
  const handleSelectTool = (nodeId: string) => {
    projection.selectTool(nodeId)
    if (!prefs.detailsOpen) {
      setPrefs({ ...prefs, detailsOpen: true })
    }
  }
  const handleCloseDetails = () => {
    projection.clearSelection()
    setPrefs({ ...prefs, detailsOpen: false })
  }

  // Plan-mode toggle: flip plan_mode (config/update) and switch the
  // paradigm to/from 'plan'. The chip reflects both the local flag and the
  // live paradigm.
  const handleTogglePlan = () => {
    const next = !(planMode || snap.paradigm === 'plan')
    setPlanMode(next)
    void projection.setPlanMode(next)
    void projection.switchParadigm(next ? 'plan' : 're_act')
  }

  const handleSlash = (cmd: SlashCommand) => {
    if (cmd === 'plan') {
      handleTogglePlan()
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
    }
  }

  const handleRespondApproval = (requestId: string, response: InteractionResponse) => {
    void projection.respondApproval(requestId, response)
  }

  // ── scenario handlers ───────────────────────────────────────────────────────
  const handlePickScenario = (scenario: BusScenario) => {
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
          />
        }
        center={
          <ConversationRoot
            snapshot={snap}
            connection={status}
            theme={theme}
            planMode={planMode || snap.paradigm === 'plan'}
            scenarios={scenarios}
            onSend={(text) => void projection.sendMessage(text)}
            onStop={() => void projection.cancelTurn()}
            onTogglePlan={handleTogglePlan}
            onSlash={handleSlash}
            onSelectTool={handleSelectTool}
            onRespondApproval={handleRespondApproval}
            onPickScenario={handlePickScenario}
            onDebrief={handleDebrief}
          />
        }
        details={
          prefs.detailsOpen ? (
            <DetailsPanel node={selectedToolNode} onClose={handleCloseDetails} />
          ) : undefined
        }
        prefs={prefs}
        onPrefsChange={setPrefs}
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
    </>
  )
}

function readInitialTheme(): Theme {
  try {
    const stored = localStorage.getItem('oneai-theme')
    if (stored === 'dark' || stored === 'light') return stored
  } catch {
    /* ignore */
  }
  if (
    typeof window !== 'undefined' &&
    window.matchMedia?.('(prefers-color-scheme: dark)').matches
  ) {
    return 'dark'
  }
  return 'light'
}
