// ConfirmDialog — a small in-page confirmation modal (replaces the jarring
// browser `window.confirm`). Uses the shared `Modal` surface + button
// vocabulary so every dialog reads as one family.

import type { ReactNode } from 'react'
import { Modal } from '../scenario/Modal'
import modalStyles from '../scenario/Modal.module.css'
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
          <button className={`${modalStyles.btn} ${modalStyles.btnSecondary}`} onClick={onClose}>
            {cancelLabel}
          </button>
          <button
            className={`${modalStyles.btn} ${danger ? modalStyles.btnDanger : modalStyles.btnPrimary}`}
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
