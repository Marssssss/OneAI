// Modal — a fixed overlay + centered card. The app's first modal surface
// (W1/W2 had none). Rendered above the AppFrame grid via `position: fixed`.
// Esc closes; click on the backdrop closes; the card stops propagation.

import { useEffect } from 'react'
import type { ReactNode } from 'react'
import styles from './Modal.module.css'

interface ModalProps {
  title: string
  /** Width clamp; defaults to the medium scenario-editor width. */
  width?: number
  onClose: () => void
  children: ReactNode
  /** Optional footer (action buttons). */
  footer?: ReactNode
}

export function Modal({
  title,
  width,
  onClose,
  children,
  footer,
}: ModalProps): ReactNode {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  return (
    <div
      className={styles.overlay}
      onPointerDown={(e) => {
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div className={styles.card} style={width !== undefined ? { width } : undefined}>
        <header className={styles.header}>
          <span className={styles.title}>{title}</span>
          <button className={styles.close} onClick={onClose} aria-label="close">
            ✕
          </button>
        </header>
        <div className={styles.body}>{children}</div>
        {footer !== undefined && <footer className={styles.footer}>{footer}</footer>}
      </div>
    </div>
  )
}
