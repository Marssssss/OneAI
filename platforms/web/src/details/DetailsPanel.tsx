// DetailsPanel — the right rail. W4 splits it into a two-tab container:
//   • Tool   — the full args/result of the tool node selected from the chat
//              stream (the inline ToolCallNode truncates to a preview; this
//              rail shows the untruncated content for inspection).
//   • Trajectory — a turn-aware event ledger + timing overview, pure-consumer
//              of the flowing EngineYield kinds the W1–W3 projection dropped.
//
// AppFrame conditionally renders this <aside> based on prefs.detailsOpen; App
// flips detailsOpen=true on selectTool (and switches to the Tool tab) and the
// close button clears selection (App may also flip detailsOpen=false).

import type { ReactNode } from 'react'
import type { ChatNode } from '../store/projection'
import type { TrajectoryEntry, UsageSnapshot, SubagentNode } from '../store/trajectory'
import { useLocale } from '../i18n'
import { TrajectoryView } from './TrajectoryView'
import styles from './DetailsPanel.module.css'

export type DetailsTab = 'tool' | 'trajectory'

interface DetailsPanelProps {
  node: ChatNode | null
  tab: DetailsTab
  onTabChange: (tab: DetailsTab) => void
  trajectory: TrajectoryEntry[]
  usage: UsageSnapshot
  subagents: SubagentNode[]
  turnTimings: { turnId: string; startedAt: number | null; endedAt: number | null; iterations: number }[]
  onClose: () => void
}

export function DetailsPanel({
  node,
  tab,
  onTabChange,
  trajectory,
  usage,
  subagents,
  turnTimings,
  onClose,
}: DetailsPanelProps): ReactNode {
  const { t } = useLocale()

  return (
    <div className={styles.root}>
      <header className={styles.header}>
        <div className={styles.tabs}>
          <button
            className={`${styles.tab} ${tab === 'tool' ? styles.tabActive : ''}`}
            onClick={() => onTabChange('tool')}
          >
            {t('details.toolTab')}
          </button>
          <button
            className={`${styles.tab} ${tab === 'trajectory' ? styles.tabActive : ''}`}
            onClick={() => onTabChange('trajectory')}
          >
            {t('details.trajectoryTab')}
          </button>
        </div>
        <button className={styles.close} onClick={onClose} title="close">
          ✕
        </button>
      </header>

      <div className={styles.scroll}>
        {tab === 'tool' ? (
          <ToolDetails node={node} />
        ) : (
          <TrajectoryView
            trajectory={trajectory}
            usage={usage}
            turnTimings={turnTimings}
            subagents={subagents}
          />
        )}
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
