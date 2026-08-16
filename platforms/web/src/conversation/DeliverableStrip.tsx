// DeliverableStrip — the per-turn list of files the agent produced this turn
// (W4 A3). Surfaced under the final assistant bubble of a turn; the projection
// collects `ToolOutput.artifacts` from the turn's tool results at TurnComplete.
//
// A browser frontend can't read a host-filesystem path, so a plain-path artifact
// renders as a chip (path + size + copy-button); a `data:`/`file:` URI artifact
// (e.g. an image) opens in the Lightbox on click.

import { useState } from 'react'
import type { ReactNode } from 'react'
import type { Artifact } from '../rpc/types'
import styles from './DeliverableStrip.module.css'

interface DeliverableStripProps {
  artifacts: Artifact[]
  /** Called when the user clicks an image-class (data:/file:) artifact. */
  onOpenImage: (src: string, alt: string) => void
}

function formatSize(bytes?: number): string {
  if (bytes === undefined) return ''
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

function isImage(a: Artifact): boolean {
  return a.mime_type.startsWith('image/') || a.path.startsWith('data:image')
}

function imageUrl(a: Artifact): string {
  // A `data:` URI is directly displayable; a `file:` URI works in some
  // desktop webviews; a plain path is not.
  if (a.path.startsWith('data:') || a.path.startsWith('file:')) return a.path
  return ''
}

export function DeliverableStrip({ artifacts, onOpenImage }: DeliverableStripProps): ReactNode {
  const [copied, setCopied] = useState<string | null>(null)

  if (artifacts.length === 0) return null

  const copyPath = (path: string) => {
    void navigator.clipboard?.writeText(path).then(() => {
      setCopied(path)
      window.setTimeout(() => setCopied(null), 1200)
    })
  }

  return (
    <div className={styles.strip}>
      <span className={styles.label}>📎 Deliverables</span>
      <div className={styles.chips}>
        {artifacts.map((a, i) => {
          const url = imageUrl(a)
          const openable = isImage(a) && url.length > 0
          return (
            <span
              className={`${styles.chip} ${openable ? styles.chipOpenable : ''}`}
              key={`${a.path}-${i}`}
              role="button"
              tabIndex={0}
              onClick={() => {
                if (openable) onOpenImage(url, a.description)
              }}
              onKeyDown={(e) => {
                if (openable && (e.key === 'Enter' || e.key === ' ')) {
                  e.preventDefault()
                  onOpenImage(url, a.description)
                }
              }}
              title={a.description}
            >
              {openable ? (
                <img className={styles.thumb} src={url} alt={a.description} />
              ) : (
                <span className={styles.icon}>📄</span>
              )}
              <span className={styles.path}>{a.path}</span>
              {formatSize(a.size_bytes).length > 0 && (
                <span className={styles.size}>{formatSize(a.size_bytes)}</span>
              )}
              {!openable && (
                <button
                  className={styles.copyBtn}
                  onClick={(e) => {
                    e.stopPropagation()
                    copyPath(a.path)
                  }}
                  aria-label="copy path"
                >
                  {copied === a.path ? '✓' : '⧉'}
                </button>
              )}
            </span>
          )
        })}
      </div>
    </div>
  )
}
