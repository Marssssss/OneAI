// SessionRenameModal — a small modal to override a session's display title
// (web-local localStorage; the engine keeps its auto-derived title). Clears
// the override when the field is left empty (reverts to the auto title).

import { useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import { Modal } from './Modal'
import modalStyles from './Modal.module.css'
import styles from './SessionRenameModal.module.css'

interface SessionRenameModalProps {
  currentTitle: string
  onSubmit: (title: string) => void
  onClose: () => void
}

export function SessionRenameModal({
  currentTitle,
  onSubmit,
  onClose,
}: SessionRenameModalProps): ReactNode {
  const { t } = useLocale()
  const [value, setValue] = useState(currentTitle)

  useEffect(() => {
    setValue(currentTitle)
  }, [currentTitle])

  return (
    <Modal
      title={t('session.renameTitle')}
      onClose={onClose}
      width={420}
      footer={
        <>
          <button className={`${modalStyles.btn} ${modalStyles.btnSecondary}`} onClick={onClose}>
            {t('scenario.cancel')}
          </button>
          <button className={`${modalStyles.btn} ${modalStyles.btnPrimary}`} onClick={() => onSubmit(value)}>
            {t('scenario.save')}
          </button>
        </>
      }
    >
      <input
        className={styles.input}
        type="text"
        autoFocus
        value={value}
        placeholder={t('session.renamePlaceholder')}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault()
            onSubmit(value)
          }
        }}
      />
    </Modal>
  )
}
