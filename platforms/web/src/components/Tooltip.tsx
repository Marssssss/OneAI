// Tooltip — a lightweight CSS-driven hover tooltip. Replaces the native
// `title` attribute on small UI affordances (the mode chip, footer buttons)
// because the browser's title delay is ~1s and feels sluggish, and the
// label disappears the instant the pointer leaves with no control. This
// surfaces in ~120ms via a CSS transition-delay and tracks the theme.
//
// Pure CSS: the wrapper is `position: relative`; `.tip` is absolutely
// placed, hidden (opacity 0, pointer-events none), and fades in on
// `:hover`/`:focus-within` after a short delay. No JS timer, no portal —
// the label is `aria-hidden` (the button's own accessible name still
// labels it for SRs). Position defaults to top; `side` picks the side.

import type { ReactNode } from 'react'
import styles from './Tooltip.module.css'

interface TooltipProps {
  label: string
  side?: 'top' | 'bottom' | 'left' | 'right'
  children: ReactNode
}

export function Tooltip({ label, side = 'top', children }: TooltipProps): ReactNode {
  return (
    <span className={`${styles.wrap} ${styles[`side_${side}`]}`}>
      {children}
      <span className={styles.tip} role="tooltip" aria-hidden>
        {label}
      </span>
    </span>
  )
}
