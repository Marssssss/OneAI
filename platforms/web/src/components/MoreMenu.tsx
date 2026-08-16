// MoreMenu — a "⋯" button that opens a small dropdown of actions. Reused by
// the session rows (rename / archive / delete) and the scenario rows
// (edit / view). Closes on outside click / Esc / item pick.

import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import styles from './MoreMenu.module.css'

export interface MoreMenuItem {
  id: string
  label: string
  /** Optional danger styling (delete). */
  danger?: boolean
  disabled?: boolean
}

interface MoreMenuProps {
  items: MoreMenuItem[]
  onPick: (id: string) => void
  ariaLabel?: string
}

export function MoreMenu({ items, onPick, ariaLabel }: MoreMenuProps): ReactNode {
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    const onDown = (e: PointerEvent) => {
      if (ref.current !== null && !ref.current.contains(e.target as Node)) {
        setOpen(false)
      }
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    window.addEventListener('pointerdown', onDown)
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('pointerdown', onDown)
      window.removeEventListener('keydown', onKey)
    }
  }, [open])

  return (
    <div className={styles.wrap} ref={ref}>
      <button
        className={styles.btn}
        onClick={(e) => {
          e.stopPropagation()
          setOpen((o) => !o)
        }}
        aria-label={ariaLabel ?? 'more'}
      >
        ⋯
      </button>
      {open && (
        <div className={styles.menu} onPointerDown={(e) => e.stopPropagation()}>
          {items.map((it) => (
            <button
              key={it.id}
              className={`${styles.item} ${it.danger === true ? styles.danger : ''}`}
              disabled={it.disabled}
              onClick={() => {
                setOpen(false)
                onPick(it.id)
              }}
            >
              {it.label}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}
