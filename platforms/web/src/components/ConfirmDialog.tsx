// ConfirmDialog — a small in-page confirmation modal (replaces the jarring
// browser `window.confirm`). Reuses the scenario `Modal` surface + the
// rename-modal's button styles so it matches the rest of the app.

import type { ReactNode } from 'react'
import { Modal } from '../scenario/Modal'
import renameStyles from '../scenario/SessionRenameModal.module.css'
import styles from './ConfirmDialog.module.css'

interface ConfirmDialogProps {
  title: string
  /** Body text shown under the title. */
  message: string
  confirmLabel: string
  cancelLabel: string
  /** Red destructive styling for the confirm button (e.g. delete). */
  danger?: boolean
  onConfirm: () => void
  onClose: () => void
}

export function ConfirmDialog({
  title,
  message,
  confirmLabel,
  cancelLabel,
  danger = false,
  onConfirm,
  onClose,
}: ConfirmDialogProps): ReactNode {
  return (
    <Modal
      title={title}
      onClose={onClose}
      width={420}
      footer={
        <>
          <button className={renameStyles.secondary} onClick={onClose}>
            {cancelLabel}
          </button>
          <button
            className={`${renameStyles.primary} ${danger ? styles.danger : ''}`}
            onClick={onConfirm}
          >
            {confirmLabel}
          </button>
        </>
      }
    >
      <p className={styles.message}>{message}</p>
    </Modal>
  )
}
