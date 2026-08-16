// App — assembles the RPC client + projection/session stores, drives the
// connection lifecycle (session/create on open), and mounts the AppFrame
// shell. W1 wires single-agent streaming chat end-to-end over the app-server
// ws transport; the details rail + scenario entry land in W2/W3.

import { useEffect, useMemo, useRef, useState } from 'react'
import { OneAiRpcClient } from './rpc/client'
import type { ConnectionStatus } from './rpc/client'
import {
  ProjectionStore,
  SessionListStore,
  useProjection,
  useSessionList,
} from './store/projection'
import { AppFrame } from './layout/AppFrame'
import { ConversationRoot } from './conversation/ConversationRoot'
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
  const prefsRef = useRef<FramePrefs>({
    sidebarWidth: 264,
    detailsWidth: 360,
    detailsOpen: false,
  })
  const [prefs, setPrefs] = useState<FramePrefs>(prefsRef.current)

  // Connect on mount; dispose on unmount. The projection's engine-event
  // subscription is driven from this effect (attach/unsubscribe) so StrictMode's
  // mount/unmount/remount cleanly re-subscribes — subscribing in the store
  // constructor is lost when the effect cleanup runs (the useMemo singleton
  // isn't reconstructed).
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
  // session/create is blocking-ack → result is {id}. We rely on the projection
  // store's session_created event for the authoritative id too, but the
  // response lets us bootstrap before any event races.
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
          onSend={(text) => void projection.runTurn(text)}
        />
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
