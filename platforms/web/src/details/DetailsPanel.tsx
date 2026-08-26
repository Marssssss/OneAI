// DetailsPanel — the right rail, now a single-purpose tool inspector. The
// trajectory was promoted to a resident center-column view (issue #40) — see
// `trajectory/TrajectoryExplorer.tsx`; this rail keeps the full args/result of
// the tool node selected from the chat stream.

import type { ReactNode } from 'react'
import type { ChatNode } from '../store/projection'
import { useLocale } from '../i18n'
import styles from './DetailsPanel.module.css'

interface DetailsPanelProps {
  node: ChatNode | null
  onClose: () => void
}

export function DetailsPanel({ node, onClose }: DetailsPanelProps): ReactNode {
  const { t } = useLocale()

  return (
    <div className={styles.root}>
      <header className={styles.header}>
        <span className={styles.title}>{t('details.toolTab')}</span>
        <button className={styles.close} onClick={onClose} title="close">
          ✕
        </button>
      </header>

      <div className={styles.scroll}>
        <ToolDetails node={node} />
      </div>
    </div>
  )
}

function ToolDetails({ node }: { node: ChatNode | null }): ReactNode {
  const { t } = useLocale()

  if (node === null || node.kind !== 'tool') {
    return <div className={styles.empty}>{t('details.empty')}</div>
  }

  const output = node.toolOutput
  return (
    <>
      <section className={styles.section}>
        <div className={styles.sectionLabel}>{t('details.tool')}</div>
        <code className={styles.toolName}>{node.toolName ?? 'tool'}</code>
        <span className={`${styles.badge} ${styles[`badge_${node.toolState ?? 'pending'}`]}`}>
          {node.toolState ?? 'pending'}
        </span>
      </section>

      <section className={styles.section}>
        <div className={styles.sectionLabel}>{t('details.args')}</div>
        <pre className={styles.code}>{prettyJson(node.toolArgs)}</pre>
      </section>

      {output !== undefined && (
        <section className={styles.section}>
          <div className={styles.sectionLabel}>{t('details.result')}</div>
          <pre className={styles.code}>{output.content}</pre>
          {output.error !== undefined && output.error !== '' && (
            <pre className={`${styles.code} ${styles.errorText}`}>{output.error}</pre>
          )}
          {output.added_tool_names !== undefined && output.added_tool_names.length > 0 && (
            <div className={styles.added}>＋ {output.added_tool_names.join(', ')}</div>
          )}
        </section>
      )}
    </>
  )
}

function prettyJson(v: unknown): string {
  if (typeof v === 'string') return v
  try {
    return JSON.stringify(v, null, 2)
  } catch {
    return String(v)
  }
}
