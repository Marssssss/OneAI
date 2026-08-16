// ToolCallNode — the disclosure card for one tool call. Mirrors dsh's
// `ToolCallTree` (flattened to one level for the single-agent path; recursive
// subcall trees are a W4 refinement). A node is pending until its matching
// `tool_result` lands, then done/errored.
//
// Clicking the header (or "show full →") selects this call for the details
// rail — the args/result full text is too long for the inline body.

import { memo, useState } from 'react'
import type { ReactNode } from 'react'
import type { ChatNode } from '../store/projection'
import { useLocale } from '../i18n'
import styles from './ToolCallNode.module.css'

interface ToolCallNodeProps {
  node: ChatNode
  selected: boolean
  onSelect: (nodeId: string) => void
}

export const ToolCallNode = memo(function ToolCallNode({
  node,
  selected,
  onSelect,
}: ToolCallNodeProps): ReactNode {
  const { t } = useLocale()
  const [open, setOpen] = useState(false)
  const state = node.toolState ?? 'pending'
  const statusLabel =
    state === 'pending' ? t('tool.pending') : state === 'error' ? t('tool.error') : t('tool.done')
  const output = node.toolOutput
  const preview =
    output !== undefined
      ? (output.content.length > 160 ? output.content.slice(0, 160) + '…' : output.content)
      : null
  const added = output?.added_tool_names

  return (
    <div
      className={`${styles.card} ${state === 'error' ? styles.error : ''} ${
        selected ? styles.selected : ''
      }`}
    >
      <button
        className={styles.header}
        onClick={() => onSelect(node.id)}
        aria-expanded={open}
        title={t('tool.inspect')}
      >
        <span className={`${styles.dot} ${styles[`dot_${state}`]}`} />
        <span className={styles.name}>{node.toolName ?? 'tool'}</span>
        <span className={styles.status}>{statusLabel}</span>
        <span
          className={styles.chevron}
          onClick={(e) => {
            e.stopPropagation()
            setOpen((v) => !v)
          }}
          role="button"
          aria-label="expand"
        >
          {open ? '▾' : '▸'}
        </span>
      </button>

      {open && (
        <div className={styles.body}>
          {node.toolArgs !== undefined && (
            <div className={styles.section}>
              <div className={styles.sectionLabel}>{t('tool.args')}</div>
              <pre className={styles.code}>{prettyJson(node.toolArgs)}</pre>
            </div>
          )}
          {output !== undefined && preview !== null && (
            <div className={styles.section}>
              <div className={styles.sectionLabel}>{t('tool.result')}</div>
              <pre className={styles.code}>{preview}</pre>
              {output.error !== undefined && output.error !== '' && (
                <pre className={`${styles.code} ${styles.errorText}`}>{output.error}</pre>
              )}
            </div>
          )}
          {added !== undefined && added.length > 0 && (
            <div className={styles.added}>＋ {t('tool.added')}: {added.join(', ')}</div>
          )}
        </div>
      )}
    </div>
  )
})

function prettyJson(v: unknown): string {
  if (typeof v === 'string') return v
  try {
    return JSON.stringify(v, null, 2)
  } catch {
    return String(v)
  }
}
