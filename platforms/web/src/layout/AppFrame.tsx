// AppFrame — the three-column shell (sidebar | center | details) with the
// concession geometry solver, mirroring dsh's `AppFrame` + `computeColumns`.
//
// Concession rules (center never yields below its floor unless forced):
//  1. collapse details to its min, then to zero (auto-close);
//  2. collapse sidebar to its rail width;
//  3. only as a last resort shrink center below its floor.
// Sidebar/details toggles are sticky preferences; auto-closed details reopens
// when the window widens again.

import { useCallback, useEffect, useState } from 'react'
import styles from './AppFrame.module.css'
import type { PointerEvent as ReactPointerEvent, ReactNode } from 'react'

const SIDEBAR_MIN = 240
const SIDEBAR_COLLAPSED = 56
const DETAILS_MIN = 300
const CENTER_MIN = 640
const GUTTER = 6 // resize handle between an expanded sidebar and the center

interface Columns {
  sidebar: number
  center: number
  details: number
  sidebarCollapsed: boolean
  detailsOpen: boolean
}

function computeColumns(width: number, prefs: Prefs): Columns {
  const available = Math.max(0, width)
  const wantDetails = prefs.detailsOpen

  // The gutter sits between the sidebar and center — it occupies its own
  // 6px grid track and so must be subtracted from the center budget. It is
  // only present when the sidebar is expanded (a collapsed rail has no
  // resize handle).
  const gutter = (p: boolean) => (p ? 0 : GUTTER)

  // Full sidebar + details + center.
  let sidebar = prefs.sidebarWidth
  let details = wantDetails ? prefs.detailsWidth : 0
  let center = available - sidebar - gutter(false) - details
  if (center >= CENTER_MIN) {
    return { sidebar, center, details, sidebarCollapsed: false, detailsOpen: details > 0 }
  }

  // Shrink details to its min.
  details = wantDetails ? DETAILS_MIN : 0
  center = available - sidebar - gutter(false) - details
  if (center >= CENTER_MIN) {
    return { sidebar, center, details, sidebarCollapsed: false, detailsOpen: details > 0 }
  }

  // Auto-close details.
  details = 0
  center = available - sidebar - gutter(false)
  if (center >= CENTER_MIN) {
    return { sidebar, center, details, sidebarCollapsed: false, detailsOpen: false }
  }

  // Collapse sidebar to rail (no gutter).
  sidebar = SIDEBAR_COLLAPSED
  center = available - sidebar - gutter(true)
  if (center >= CENTER_MIN) {
    return { sidebar, center, details, sidebarCollapsed: true, detailsOpen: false }
  }

  // Last resort: shrink center below floor; keep the sidebar rail.
  center = Math.max(0, available - sidebar - gutter(true))
  return { sidebar, center, details, sidebarCollapsed: true, detailsOpen: false }
}

interface Prefs {
  sidebarWidth: number
  detailsWidth: number
  detailsOpen: boolean
}

interface AppFrameProps {
  sidebar: ReactNode
  center: ReactNode
  details?: ReactNode
  /** Sticky prefs the parent owns so collapse state survives re-mounts. */
  prefs: Prefs
  onPrefsChange: (p: Prefs) => void
}

export function AppFrame({
  sidebar,
  center,
  details,
  prefs,
  onPrefsChange,
}: AppFrameProps): ReactNode {
  const [width, setWidth] = useState(
    typeof window !== 'undefined' ? window.innerWidth : 1280,
  )

  useEffect(() => {
    const onResize = () => setWidth(window.innerWidth)
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  // The grid template's column count MUST match the rendered child count —
  // a stray grid child (the gutter) lands in the wrong track and shoves the
  // center into a 0px column. Columns: sidebar | (gutter) | center | (details).
  const cols = computeColumns(width, prefs)
  const tracks: string[] = [`${cols.sidebar}px`]
  if (!cols.sidebarCollapsed) tracks.push(`${GUTTER}px`)
  tracks.push(`${cols.center}px`)
  if (cols.detailsOpen) tracks.push(`${cols.details}px`)
  const gridTemplate = tracks.join(' ')

  // Drag-to-resize the sidebar gutter. Simple pointer handlers — no library;
  // the column solver recomputes on each move.
  const startSidebarResize = useCallback(
    (e: ReactPointerEvent) => {
      e.preventDefault()
      const startX = e.clientX
      const startWidth = prefs.sidebarWidth
      const move = (ev: PointerEvent) => {
        const next = Math.min(
          SIDEBAR_MIN + 180, // generous max
          Math.max(SIDEBAR_MIN, startWidth + (ev.clientX - startX)),
        )
        onPrefsChange({ ...prefs, sidebarWidth: next })
      }
      const up = () => {
        window.removeEventListener('pointermove', move)
        window.removeEventListener('pointerup', up)
      }
      window.addEventListener('pointermove', move)
      window.addEventListener('pointerup', up)
    },
    [prefs, onPrefsChange],
  )

  return (
    <div className={styles.frame} style={{ gridTemplateColumns: gridTemplate }}>
      <aside
        className={`${styles.sidebar} ${
          cols.sidebarCollapsed ? styles.collapsed : ''
        }`}
      >
        {sidebar}
      </aside>
      {!cols.sidebarCollapsed && (
        <div
          className={styles.gutter}
          onPointerDown={startSidebarResize}
          role="separator"
          aria-orientation="vertical"
        />
      )}
      <main className={styles.center}>{center}</main>
      {cols.detailsOpen && details !== undefined && (
        <aside className={styles.details}>{details}</aside>
      )}
    </div>
  )
}
