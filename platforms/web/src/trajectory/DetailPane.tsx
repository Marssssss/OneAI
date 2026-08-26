// DetailPane — renders the drill-in details of a selected trajectory node,
// keyed by the node's `TrajectoryDetail.kind` (issue #40: what a node shows
// depends on its type — tool args/result only on tool nodes, context sections
// only on context nodes, …).

import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { TrajectoryEntry } from '../store/trajectory'
import styles from './DetailPane.module.css'

interface DetailPaneProps {
  entry: TrajectoryEntry | null
}

function Row({ label, children }: { label: string; children: ReactNode }): ReactNode {
  return (
    <div className={styles.row}>
      <span className={styles.label}>{label}</span>
      <div className={styles.value}>{children}</div>
    </div>
  )
}

function Mono({ text }: { text: string }): ReactNode {
  return text.length === 0 ? <span className={styles.muted}>—</span> : <pre className={styles.mono}>{text}</pre>
}

function jsonify(value: unknown): string {
  if (value === undefined || value === null) return ''
  if (typeof value === 'string') return value
  try {
    return JSON.stringify(value, null, 2)
  } catch {
    return String(value)
  }
}

export function DetailPane({ entry }: DetailPaneProps): ReactNode {
  const { t } = useLocale()
  if (entry === null) return <div className={styles.empty}>{t('trajectory.noEvents')}</div>

  const d = entry.detail
  if (!d) {
    return <div className={styles.plain}>{entry.title}</div>
  }

  const duration = d.kind === 'tool' ? d.durationMs : undefined

  switch (d.kind) {
    case 'iteration':
      return (
        <div>
          <Row label={t('trajectory.detail.iteration')}>#{d.iteration} · {d.paradigm}</Row>
          {d.usage !== undefined && (
            <Row label={t('trajectory.detail.tokens')}>
              {d.usage.prompt_tokens} / {d.usage.completion_tokens}
              {d.usage.cache_read_tokens > 0 ? ` (cache ${d.usage.cache_read_tokens})` : ''}
            </Row>
          )}
          {d.thinking.length > 0 && <Row label={t('trajectory.detail.thinking')}><Mono text={d.thinking} /></Row>}
          {d.inference.length > 0 && <Row label={t('trajectory.detail.reasoning')}><Mono text={d.inference} /></Row>}
        </div>
      )
    case 'context':
      return (
        <div>
          <Row label={t('trajectory.detail.iteration')}>#{d.iteration}</Row>
          <div className={styles.sections}>
            {d.sections.map((s, i) => (
              <details key={i} className={styles.section}>
                <summary>
                  <span className={styles.sectionLabel}>{s.label}</span>
                  <span className={styles.sectionTokens}>~{s.tokens} tok</span>
                </summary>
                <Mono text={s.content} />
              </details>
            ))}
          </div>
        </div>
      )
    case 'tool':
      return (
        <div>
          <Row label={t('trajectory.detail.duration')}>
            {duration !== undefined ? `${duration}ms` : '—'}
          </Row>
          <Row label={t('trajectory.detail.ok')}>{d.ok === true ? t('trajectory.detail.ok') : d.ok === false ? t('trajectory.detail.err') : '—'}</Row>
          <Row label={t('trajectory.detail.toolCmd')}><Mono text={jsonify(d.args)} /></Row>
          {d.result !== undefined && <Row label={t('trajectory.detail.toolResult')}><Mono text={d.result} /></Row>}
        </div>
      )
    case 'delegate':
      return (
        <div>
          <Row label="task">{d.task}</Row>
          {d.dependsOn.length > 0 && (
            <Row label={t('trajectory.detail.dependsOn')}>
              <div className={styles.chips}>{d.dependsOn.map((x) => <span key={x} className={styles.chip}>{x}</span>)}</div>
            </Row>
          )}
        </div>
      )
    case 'delegate_progress':
      return (
        <div>
          <Row label="task">{d.taskId}</Row>
          <Row label="event"><Mono text={jsonify(d.event)} /></Row>
        </div>
      )
    case 'delegate_complete':
      return (
        <div>
          <Row label={t('trajectory.detail.summary')}>{d.summary.summary}</Row>
          {d.summary.keyFindings.length > 0 && (
            <Row label={t('trajectory.detail.keyFindings')}>
              <ul className={styles.list}>{d.summary.keyFindings.map((f, i) => <li key={i}>{f}</li>)}</ul>
            </Row>
          )}
          <Row label={t('trajectory.detail.tokens')}>{d.summary.tokensUsed}</Row>
        </div>
      )
    case 'plan':
      return (
        <div>
          <Row label={t('trajectory.detail.plan')}>{d.steps} step{d.steps === 1 ? '' : 's'}</Row>
          <Mono text={jsonify(d.plan)} />
        </div>
      )
    case 'paradigm':
      return <Row label={t('trajectory.detail.paradigm')}>{d.from} → {d.to}</Row>
    case 'approval':
      return (
        <div>
          <Row label="request">{d.requestId}</Row>
          <Mono text={jsonify(d.request)} />
        </div>
      )
    case 'working_state':
      return <Mono text={jsonify(d.event)} />
    case 'context_accounting':
      return (
        <div>
          <Row label={t('trajectory.detail.tokens')}>{d.accounting.total_tokens} (of {d.accounting.context_window_size})</Row>
          <Mono text={jsonify(d.accounting)} />
        </div>
      )
    case 'tools_added':
      return (
        <div>
          <Row label="tools"><div className={styles.chips}>{d.names.map((x) => <span key={x} className={styles.chip}>{x}</span>)}</div></Row>
        </div>
      )
    case 'interrupted':
      return <Row label="reason">{d.reason} ({d.point})</Row>
    case 'reflection':
      return <Row label={t('trajectory.detail.summary')}>{d.summary}</Row>
    case 'error':
      return <Row label={t('trajectory.detail.err')}>{d.message}</Row>
    case 'turn':
      return <Row label="task">{d.task}</Row>
    case 'turn_complete':
      return <div className={styles.muted}>—</div>
    default:
      return <div className={styles.plain}>{entry.title}</div>
  }
}
