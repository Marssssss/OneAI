// WorkspaceDropdown — a popover (NOT a modal) anchored under the composer's
// workspace chip, deepseek-harness style. Lists known workspaces (click to
// bind to a fresh session) + an "添加工作区" button that opens the NATIVE OS
// folder picker (the sidecar shells out to `osascript choose folder` /
// `zenity` / `kdialog` and returns the real absolute path — browsers can't
// get a host path, so the local backend shows the dialog). No file upload;
// the agent operates on the real folder.

import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import { workspaceStore, useWorkspace } from './workspaceStore'
import styles from './WorkspaceDropdown.module.css'

interface WorkspaceDropdownProps {
  onClose: () => void
  /** Select an existing workspace path (binds to a fresh session). Empty
   *  string ⇒ "no workspace" (the app-global cwd). */
  onSelect: (path: string) => void
  /** Open the native OS folder picker (handled in App via the
   *  `dialog/pick_directory` RPC); on confirm the returned path is upserted
   *  and bound to a fresh session. */
  onAddWorkspace: () => void
}

export function WorkspaceDropdown({ onClose, onSelect, onAddWorkspace }: WorkspaceDropdownProps): ReactNode {
  const { t } = useLocale()
  const snap = useWorkspace()

  return (
    <>
      {/* Transparent click-away backdrop (popover, not a full modal). */}
      <div className={styles.backdrop} onPointerDown={onClose} />
      <div className={styles.panel} role="menu" onPointerDown={(e) => e.stopPropagation()}>
        <div className={styles.subhead}>{t('workspace.known')}</div>
        {snap.list.length === 0 && (
          <div className={styles.empty}>{t('workspace.none')}</div>
        )}
        <div className={styles.list}>
          {snap.list.map((w) => (
            <button
              key={w.path}
              className={`${styles.row} ${snap.current === w.path ? styles.rowActive : ''}`}
              onClick={() => {
                onClose()
                onSelect(w.path)
              }}
              title={w.path}
            >
              <span className={styles.icon}>📂</span>
              <span className={styles.alias}>{w.alias}</span>
              {snap.current === w.path && <span className={styles.check}>✓</span>}
              <span
                className={styles.remove}
                role="button"
                tabIndex={0}
                onClick={(e) => {
                  e.stopPropagation()
                  workspaceStore.remove(w.path)
                }}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.stopPropagation()
                    e.preventDefault()
                    workspaceStore.remove(w.path)
                  }
                }}
                title={t('settings.delete')}
                aria-label={t('settings.delete')}
              >
                ✕
              </span>
            </button>
          ))}
        </div>

        <button className={styles.addBtn} onClick={onAddWorkspace}>
          + {t('workspace.add')}
        </button>
        <button className={styles.noneBtn} onClick={() => { onClose(); onSelect('') }}>
          {t('workspace.noWorkspace')}
        </button>
      </div>
    </>
  )
}
