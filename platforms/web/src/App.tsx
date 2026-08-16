// App — assembles the RPC client + projection/session stores, drives the
// connection lifecycle (session/create on open), and mounts the AppFrame
// shell. W2 wires: tool/plan/approval consumes (in projection), the details
// rail (selected tool → DetailsPanel), plan-mode toggle (config/update +
// paradigm/switch), Stop (turn/cancel), slash commands, and the parallel
// approval queue.

import { useEffect, useMemo, useRef, useState } from 'react'
import { OneAiRpcClient } from './rpc/client'
import type { ConnectionStatus } from './rpc/client'
import {
  ProjectionStore,
  SessionListStore,
  useProjection,
  useSessionList,
} from './store/projection'
import type { InteractionResponse } from './rpc/types'
import { AppFrame } from './layout/AppFrame'
import { ConversationRoot } from './conversation/ConversationRoot'
import type { SlashCommand } from './conversation/Composer'
import { DetailsPanel } from './details/DetailsPanel'
import { SidebarRoot } from './sidebar/SidebarRoot'
import { localeStore } from './i18n'
import './theme/markdown.css'

const APP_SERVER_URL =
  (import.meta.env.VITE_APP_SERVER_URL as string | undefined) ?? 'ws://127.0.0.1:8787'

interface FramePrefs {
  sidebarWidth: number
  detailsWidth: number
  detailsOpen: boolean
}

type Theme = 'light' | 'dark'

export default function App(): React.ReactNode {
  const rpc = useMemo(() => new OneAiRpcClient(APP_SERVER_URL), [])
  const projection = useMemo(() => new ProjectionStore(rpc), [rpc])
  const sessionList = useMemo(() => new SessionListStore(rpc), [rpc])
  const [status, setStatus] = useState<ConnectionStatus>('closed')
  const [theme, setTheme] = useState<Theme>(() => readInitialTheme())
  // planMode is the App-owned toggle synced via config/update; the live
  // paradigm (from paradigm_switch yields) also feeds the chip's "on" state.
  const [planMode, setPlanMode] = useState(false)
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

  // When the socket opens: ensure we have a session, then load the list.
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
      creatingRef.current = false
    })()
  }, [status, rpc, sessionList])

  // Refresh the session list after each completed turn (new title/counts).
  useEffect(() => {
    const off = rpc.onEvent((y) => {
      if (y.kind === 'turn_complete' || y.kind === 'session_deleted' || y.kind === 'session_cleared') {
        void sessionList.refresh()
      }
    })
    return off
  }, [rpc, sessionList])

  const snap = useProjection(projection)
  const sessions = useSessionList(sessionList)

  // ── handlers ────────────────────────────────────────────────────────────────
  const handleNewSession = async () => {
    try {
      await rpc.call<{ id?: string }, { id?: string }>('session/create', {})
      await sessionList.refresh()
    } catch {
      /* offline */
    }
  }
  const handlePickSession = (id: string) => {
    void projection.loadSession(id)
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
      void projection.clearSession()
    } else if (cmd === 'compact') {
      void projection.compact(10)
    }
  }

  const handleRespondApproval = (requestId: string, response: InteractionResponse) => {
    void projection.respondApproval(requestId, response)
  }

  const selectedToolNode =
    snap.selectedToolNodeId !== null
      ? snap.nodes.find((n) => n.id === snap.selectedToolNodeId) ?? null
      : null

  return (
    <AppFrame
      sidebar={
        <SidebarRoot
          sessions={sessions}
          currentSessionId={snap.sessionId}
          theme={theme}
          onNewSession={handleNewSession}
          onPickSession={handlePickSession}
          onToggleTheme={toggleTheme}
          onToggleLocale={toggleLocale}
        />
      }
      center={
        <ConversationRoot
          snapshot={snap}
          connection={status}
          theme={theme}
          planMode={planMode || snap.paradigm === 'plan'}
          onSend={(text) => void projection.runTurn(text)}
          onStop={() => void projection.cancelTurn()}
          onTogglePlan={handleTogglePlan}
          onSlash={handleSlash}
          onSelectTool={handleSelectTool}
          onRespondApproval={handleRespondApproval}
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
