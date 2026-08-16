// Lightbox — a portal overlay for viewing an image at full size. Triggered
// by clicking a user-attachment thumbnail, an assistant markdown image, or an
// image-class deliverable artifact (W4). Esc / backdrop-click closes.

import { useEffect } from 'react'
import type { ReactNode } from 'react'
import { createPortal } from 'react-dom'
import styles from './Lightbox.module.css'

interface LightboxProps {
  /** `data:` URL or any displayable image URL. */
  src: string
  alt: string
  onClose: () => void
}

export function Lightbox({ src, alt, onClose }: LightboxProps): ReactNode {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  if (typeof document === 'undefined') return null
  return createPortal(
    <div
      className={styles.backdrop}
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label={alt}
    >
      <img
        className={styles.image}
        src={src}
        alt={alt}
        onClick={(e) => e.stopPropagation()}
      />
      <button className={styles.close} onClick={onClose} aria-label="close">
        ✕
      </button>
    </div>,
    document.body,
  )
}
