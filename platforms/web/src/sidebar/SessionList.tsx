// SessionList — the saved-conversations list backed by the `session/list`
// synchronous RPC. Each row shows the (possibly client-overridden) title + a
// "⋯" more menu (rename / archive / delete). Archived sessions (web-local
// localStorage flag) are hidden from the main list and shown under a
// collapsed "已归档 (N)" expander with an un-archive action. Delete is
// engine-backed (`session/delete`); rename + archive are web-local until a
// server-side RPC lands.
//
// Workspace grouping (deepseek-harness parity): the list defaults to grouping
// active sessions by their bound `workspace` path (the picker sets it at
// session/create); a toggle switches to flat. Sessions with no workspace
// (legacy / created without one) fall into the "其他" group, rendered last.
// The currently-selected workspace's group is pinned to the top.

import { useState } from 'react'
import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { SessionInfo } from '../rpc/types'
import type { SessionMeta } from '../store/sessionMeta'
import type { ScenarioEntry } from '../scenario/scenarioStore'
import { MoreMenu } from '../components/MoreMenu'
import { useWorkspace, workspaceStore } from '../workspace/workspaceStore'
import styles from './SessionList.module.css'

type GroupMode = 'group' | 'flat'

const GROUP_MODE_KEY = 'oneai-session-group-mode'

function readGroupMode(): GroupMode {
  try {
    const v = localStorage.getItem(GROUP_MODE_KEY)
    return v === 'flat' ? 'flat' : 'group'
  } catch {
    return 'group'
  }
}

function writeGroupMode(m: GroupMode): void {
  try {
    localStorage.setItem(GROUP_MODE_KEY, m)
  } catch {
    /* ignore */
  }
}

interface SessionListProps {
  sessions: SessionInfo[]
  currentId: string | null
  meta: SessionMeta
  /** The scenario library — used to tag scenario-derived sessions with their
   *  scenario icon (UI-2). A session whose auto-derived title is exactly a
   *  scenario name, or `<name>·<topic…>`, is matched longest-name-first so a
   *  prefix scenario (e.g. "面试") doesn't shadow a longer one ("面试演练"). */
  scenarios: ScenarioEntry[]
  onPick: (id: string) => void
  onRename: (id: string, currentTitle: string) => void
  onArchive: (id: string) => void
  onUnarchive: (id: string) => void
  onDelete: (id: string) => void
}

/** Resolve a scenario icon for a session title, or null when the title isn't
 *  scenario-derived (single-agent chats / renamed scenarios / deleted
 *  scenarios). Matches longest scenario name first to avoid prefix shadowing. */
function scenarioIconFor(
  title: string,
  scenarios: ScenarioEntry[],
): string | null {
  if (title.length === 0) return null
  // Longest-name-first so "面试演练" wins over a hypothetical "面试".
  const sorted = [...scenarios].sort(
    (a, b) => b.scenario.name.length - a.scenario.name.length,
  )
  for (const e of sorted) {
    const name = e.scenario.name
    if (name.length === 0) continue
    if (title === name || title.startsWith(name + '·')) {
      return e.scenario.icon ?? '◆'
    }
  }
  return null
}

export function SessionList({
  sessions,
  currentId,
  meta,
  scenarios,
  onPick,
  onRename,
  onArchive,
  onUnarchive,
  onDelete,
}: SessionListProps): ReactNode {
  const { t } = useLocale()
  const workspaceSnap = useWorkspace()
  const [groupMode, setGroupMode] = useState<GroupMode>(() => readGroupMode())
  const [showArchived, setShowArchived] = useState(false)
  // Collapsed group keys (workspace path, or '' for the "其他" bucket).
  const [collapsed, setCollapsed] = useState<Set<string>>(() => new Set())

  if (sessions.length === 0) {
    return <div className={styles.empty}>{t('sidebar.empty')}</div>
  }

  const active = sessions.filter((s) => !meta.archived.has(s.id))
  const archived = sessions.filter((s) => meta.archived.has(s.id))

  const row = (s: SessionInfo, archivedRow: boolean): ReactNode => {
    const title = meta.titles[s.id] ?? s.title
    // Only the engine-derived title carries the scenario marker (the title
    // is `<name>·<topics>`); a client-side rename override drops it, so a
    // renamed scenario session shows no icon (by design — the user gave it
    // a custom name).
    const icon = meta.titles[s.id] === undefined ? scenarioIconFor(s.title, scenarios) : null
    const items = archivedRow
      ? [
          { id: 'rename', label: t('session.rename') },
          { id: 'unarchive', label: t('session.unarchive') },
          { id: 'delete', label: t('session.delete'), danger: true },
        ]
      : [
          { id: 'rename', label: t('session.rename') },
          { id: 'archive', label: t('session.archive') },
          { id: 'delete', label: t('session.delete'), danger: true },
        ]
    return (
      <div
        key={s.id}
        className={`${styles.item} ${s.id === currentId ? styles.active : ''}`}
      >
        <button className={styles.pick} onClick={() => onPick(s.id)} title={title}>
          {icon !== null && <span className={styles.scenarioTag} aria-hidden>{icon}</span>}
          <span className={styles.title}>{title || s.id}</span>
        </button>
        <MoreMenu
          items={items}
          onPick={(action) => {
            if (action === 'rename') onRename(s.id, title)
            else if (action === 'archive') onArchive(s.id)
            else if (action === 'unarchive') onUnarchive(s.id)
            else if (action === 'delete') onDelete(s.id)
          }}
          ariaLabel={t('session.more')}
        />
      </div>
    )
  }

  // ── workspace grouping ──────────────────────────────────────────────────
  // Build ordered groups: [current workspace] then other workspaces (in
  // first-seen / most-recent order — `active` is already updated_at DESC),
  // then the "其他" (no-workspace) bucket last.
  const groups: { key: string; label: string; sessions: SessionInfo[] }[] = []
  const byKey = new Map<string, SessionInfo[]>()
  const noKey = '__none__'
  for (const s of active) {
    const k = s.workspace ?? noKey
    const arr = byKey.get(k) ?? []
    arr.push(s)
    byKey.set(k, arr)
  }
  // Pin the currently-selected workspace's group first (if it has sessions).
  const currentPath = workspaceSnap.current
  const order: string[] = []
  if (currentPath !== null && byKey.has(currentPath)) order.push(currentPath)
  for (const k of byKey.keys()) {
    if (k === noKey || k === currentPath) continue
    order.push(k)
  }
  // "其他" last.
  if (byKey.has(noKey)) order.push(noKey)
  for (const k of order) {
    const sessions_ = byKey.get(k)!
    const label =
      k === noKey
        ? t('workspace.other')
        : workspaceStore.labelFor(k)
    groups.push({ key: k, label, sessions: sessions_ })
  }

  const toggleGroup = (key: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })
  }

  const cycleGroupMode = () => {
    const next: GroupMode = groupMode === 'group' ? 'flat' : 'group'
    setGroupMode(next)
    writeGroupMode(next)
  }

  return (
    <div className={styles.list}>
      <button
        className={styles.modeToggle}
        onClick={cycleGroupMode}
        title={groupMode === 'group' ? t('workspace.flat') : t('workspace.groupBy')}
      >
        {groupMode === 'group' ? t('workspace.groupBy') : t('workspace.flat')}
      </button>

      {groupMode === 'flat'
        ? active.map((s) => row(s, false))
        : groups.map((g) => (
            <div key={g.key} className={styles.group}>
              <button
                className={styles.groupHeader}
                onClick={() => toggleGroup(g.key)}
                title={g.key === noKey ? undefined : g.key}
              >
                <span className={styles.groupLabel}>{g.label}</span>
                <span className={styles.groupCount}>{g.sessions.length}</span>
                <span className={styles.caret}>
                  {collapsed.has(g.key) ? '▸' : '▾'}
                </span>
              </button>
              {!collapsed.has(g.key) && (
                <div className={styles.groupList}>
                  {g.sessions.map((s) => row(s, false))}
                </div>
              )}
            </div>
          ))}

      {active.length === 0 && archived.length > 0 && (
        <div className={styles.empty}>{t('sidebar.allArchived')}</div>
      )}
      {archived.length > 0 && (
        <>
          <button
            className={styles.archivedToggle}
            onClick={() => setShowArchived((v) => !v)}
          >
            {t('session.archived')} ({archived.length}){' '}
            <span className={styles.caret}>{showArchived ? '▴' : '▾'}</span>
          </button>
          {showArchived && (
            <div className={styles.archivedList}>
              {archived.map((s) => row(s, true))}
            </div>
          )}
        </>
      )}
    </div>
  )
}
