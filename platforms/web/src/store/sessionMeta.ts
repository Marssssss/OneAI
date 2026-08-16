// SessionMetaStore — web-local per-session metadata (archived flag + custom
// title override) backed by localStorage. The engine's `session/list` returns
// only auto-derived titles and has no archive field, so until a server-side
// `session/rename`/`session/archive` RPC lands, the web frontend tracks these
// locally. Limitation: an archived/renamed session still shows its original
// title/state in the macOS app or another frontend (no shared state). Delete
// is engine-backed (`session/delete`) and clears the local meta too.
//
// External store so `useSessionMeta` re-renders the sidebar on changes.

import { useSyncExternalStore } from 'react'

const ARCHIVED_KEY = 'oneai-session-archived'
const TITLES_KEY = 'oneai-session-titles'

export interface SessionMeta {
  archived: Set<string>
  titles: Record<string, string>
}

class SessionMetaStore {
  private meta: SessionMeta = { archived: new Set(), titles: {} }
  private listeners = new Set<() => void>()

  constructor() {
    this.meta = this.read()
  }

  private read(): SessionMeta {
    let archived = new Set<string>()
    let titles: Record<string, string> = {}
    try {
      const a = localStorage.getItem(ARCHIVED_KEY)
      if (a !== null) archived = new Set(JSON.parse(a) as string[])
      const t = localStorage.getItem(TITLES_KEY)
      if (t !== null) titles = JSON.parse(t) as Record<string, string>
    } catch {
      /* corrupt — start empty */
    }
    return { archived, titles }
  }

  private write(): void {
    try {
      localStorage.setItem(ARCHIVED_KEY, JSON.stringify([...this.meta.archived]))
      localStorage.setItem(TITLES_KEY, JSON.stringify(this.meta.titles))
    } catch {
      /* ignore quota / private mode */
    }
    for (const l of this.listeners) l()
  }

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn)
    return () => this.listeners.delete(fn)
  }
  getSnapshot = (): SessionMeta => this.meta

  isArchived(id: string): boolean {
    return this.meta.archived.has(id)
  }
  titleFor(id: string, fallback: string): string {
    return this.meta.titles[id] ?? fallback
  }

  archive(id: string): void {
    this.meta = {
      archived: new Set([...this.meta.archived, id]),
      titles: this.meta.titles,
    }
    this.write()
  }
  unarchive(id: string): void {
    const next = new Set(this.meta.archived)
    next.delete(id)
    this.meta = { archived: next, titles: this.meta.titles }
    this.write()
  }
  rename(id: string, title: string): void {
    const trimmed = title.trim()
    const titles = { ...this.meta.titles }
    if (trimmed.length === 0) {
      delete titles[id]
    } else {
      titles[id] = trimmed
    }
    this.meta = { archived: this.meta.archived, titles }
    this.write()
  }
  /** Clear local meta for a deleted session (engine delete already handled). */
  forget(id: string): void {
    const next = new Set(this.meta.archived)
    next.delete(id)
    const titles = { ...this.meta.titles }
    delete titles[id]
    this.meta = { archived: next, titles }
    this.write()
  }
}

export const sessionMetaStore = new SessionMetaStore()

export function useSessionMeta(): SessionMeta {
  return useSyncExternalStore(
    sessionMetaStore.subscribe,
    sessionMetaStore.getSnapshot,
    () => ({ archived: new Set(), titles: {} }),
  )
}
