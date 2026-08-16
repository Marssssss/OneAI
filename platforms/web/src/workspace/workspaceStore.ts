// WorkspaceStore — web-local "select a working directory" affordance
// (deepseek-harness parity). A workspace is a host-filesystem path the user
// binds a session to at creation; the engine persists it in
// `conversation.metadata["workspace"]` and (Part C) threads it as the
// session's active cwd. The web can't read a real absolute path from a file
// picker (browsers hide it), so the user types/pastes a host path and gives
// it a display alias; the engine sidecar runs on the same host, so the path
// is real to it.
//
// External store so `useWorkspace` re-renders the sidebar + composer chip on
// change. localStorage-backed (like SessionMetaStore); limitation: a workspace
// added in the web UI isn't shared with the macOS/native app (no shared store
// for the alias map — the engine only knows the path string).

import { useSyncExternalStore } from 'react'

const LIST_KEY = 'oneai-workspaces'
const CURRENT_KEY = 'oneai-workspace-current'

export interface Workspace {
  /** Host-filesystem path (the value sent to `session/create` as `workspace`
   * and stored in `conversation.metadata["workspace"]`). */
  path: string
  /** Short display name. Defaults to the path's last segment. */
  alias: string
}

/** Derive a display alias from a path when the user didn't name it. */
export function workspaceLabel(path: string): string {
  const trimmed = path.replace(/\/+$/, '')
  const base = trimmed.split('/').filter(Boolean).pop()
  return base && base.length > 0 ? base : trimmed || path
}

function normalizePath(p: string): string {
  return p.trim().replace(/\/+$/, '')
}

class WorkspaceStore {
  private list: Workspace[] = []
  private current: string | null = null
  // Cached snapshot — `useSyncExternalStore` requires `getSnapshot` to return
  // a referentially-stable object and only change when the store actually
  // changes (returning a fresh literal each call triggers an infinite render
  // loop). Rebuilt in `write()` after every mutation.
  private snapshot: WorkspaceSnapshot = { list: [], current: null }
  private listeners = new Set<() => void>()

  constructor() {
    this.read()
  }

  private read(): void {
    try {
      const l = localStorage.getItem(LIST_KEY)
      if (l !== null) this.list = JSON.parse(l) as Workspace[]
      const c = localStorage.getItem(CURRENT_KEY)
      this.current = c !== null ? (JSON.parse(c) as string) : null
    } catch {
      this.list = []
      this.current = null
    }
    this.rebuildSnapshot()
  }

  private rebuildSnapshot(): void {
    this.snapshot = { list: this.list, current: this.current }
  }

  private write(): void {
    try {
      localStorage.setItem(LIST_KEY, JSON.stringify(this.list))
      localStorage.setItem(
        CURRENT_KEY,
        JSON.stringify(this.current),
      )
    } catch {
      /* quota / private mode */
    }
    this.rebuildSnapshot()
    for (const l of this.listeners) l()
  }

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn)
    return () => {
      this.listeners.delete(fn)
    }
  }

  /** Stable snapshot for `useSyncExternalStore` — cached, rebuilt only on
   *  mutation. */
  getSnapshot = (): WorkspaceSnapshot => this.snapshot

  list_all(): Workspace[] {
    return this.list
  }

  /** Resolve a path to its display alias (known workspace alias, else the
   *  path's basename). Used by the session-list group headers + chip. */
  labelFor(path: string | null | undefined): string {
    if (!path) return ''
    const known = this.list.find((w) => w.path === path)
    return known?.alias ?? workspaceLabel(path)
  }

  currentWorkspace(): Workspace | null {
    if (this.current === null) return null
    const known = this.list.find((w) => w.path === this.current)
    if (known) return known
    return { path: this.current, alias: workspaceLabel(this.current) }
  }

  /** Add (or update the alias of) a workspace by path. Returns the path
   *  normalized. */
  upsert(path: string, alias?: string): string {
    const p = normalizePath(path)
    if (p.length === 0) return ''
    const next = this.list.filter((w) => w.path !== p)
    next.push({ path: p, alias: (alias ?? '').trim() || workspaceLabel(p) })
    this.list = next
    this.write()
    return p
  }

  remove(path: string): void {
    const p = normalizePath(path)
    this.list = this.list.filter((w) => w.path !== p)
    if (this.current === p) this.current = null
    this.write()
  }

  setCurrent(path: string | null): void {
    if (path === null) {
      this.current = null
    } else {
      const p = normalizePath(path)
      this.current = p.length > 0 ? p : null
    }
    this.write()
  }

  /** Test-only: re-read from localStorage so the singleton can be reset
   *  between tests (the constructor only reads once). Not for app use. */
  __resetForTests(): void {
    this.list = []
    this.current = null
    this.read()
  }
}

export const workspaceStore = new WorkspaceStore()

export interface WorkspaceSnapshot {
  list: Workspace[]
  current: string | null
}

export function useWorkspace(): WorkspaceSnapshot {
  return useSyncExternalStore(
    workspaceStore.subscribe,
    workspaceStore.getSnapshot,
    () => ({ list: [], current: null }),
  )
}
