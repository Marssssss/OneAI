// DetailPane — renders the drill-in details of a selected trajectory node,
// keyed by the node's `TrajectoryDetail.kind` (issue #40: what a node shows
// depends on its type — tool args/result only on tool nodes, context sections
// only on context nodes, the API request/response on infer nodes, …).

import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { InferenceMessage, InferenceSnapshot } from '../rpc/types'
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

function formatMs(ms: number | undefined): string {
  return ms !== undefined && ms >= 0 ? `${ms}ms` : '—'
}

/** A compact, readable rendering of one inference message (request/response). */
function renderMessage(m: InferenceMessage): string {
  const body = m.content
    .map((b) => {
      switch (b.type) {
        case 'text':
        case 'thinking':
          return b.text
        case 'tool_call':
          return `⟦tool_call ${b.name}⟧ ${b.args}`
        case 'tool_result':
          return `⟦tool_result⟧ ${b.content}`
        case 'image':
          return `⟦image ${b.mime_type}⟧`
        case 'file':
          return `⟦file ${b.uri}⟧`
        default:
          return ''
      }
    })
    .filter((s) => s.length > 0)
    .join('\n')
  return body.length > 0 ? `[${m.role}] ${body}` : `[${m.role}]`
}

function renderMessages(msgs: InferenceMessage[]): string {
  return msgs.map(renderMessage).join('\n\n')
}

/** Render the API request/response drill-in for an infer node. */
function InferenceDetail({ snap }: { snap: InferenceSnapshot }): ReactNode {
  const { t } = useLocale()
  const u = snap.response.usage
  const cacheHit =
    u.prompt_tokens > 0 ? Math.round((u.cache_read_tokens / u.prompt_tokens) * 100) : 0
  return (
    <>
      <Row label={t('trajectory.detail.model')}>{snap.model}</Row>
      <Row label={t('trajectory.detail.params')}>
        <div className={styles.params}>
          <span className={styles.chip}>temp {snap.temperature ?? '—'}</span>
          <span className={styles.chip}>top_p {snap.top_p ?? '—'}</span>
          <span className={styles.chip}>max_tokens {snap.max_tokens ?? '—'}</span>
          <span className={styles.chip}>thinking {snap.thinking_budget ?? '—'}</span>
        </div>
      </Row>
      <Row label={t('trajectory.detail.requestMeta')}>
        {snap.message_count} msg · {snap.tool_names.length} tools
        {snap.tool_names.length > 0 && (
          <div className={styles.chips}>{snap.tool_names.map((n) => <span key={n} className={styles.chip}>{n}</span>)}</div>
        )}
      </Row>
      <Row label={t('trajectory.detail.responseUsage')}>
        in {u.prompt_tokens} / out {u.completion_tokens}
        {u.cache_read_tokens > 0 ? ` · cache ${u.cache_read_tokens} (${cacheHit}%)` : ''}
      </Row>
      <div className={styles.sections}>
        <details className={styles.section} open>
          <summary>
            <span className={styles.sectionLabel}>{t('trajectory.detail.apiRequest')}</span>
          </summary>
          <Mono text={renderMessages(snap.request_messages)} />
        </details>
        <details className={styles.section}>
          <summary>
            <span className={styles.sectionLabel}>{t('trajectory.detail.apiResponse')}</span>
          </summary>
          <Mono text={renderMessage(snap.response.message)} />
        </details>
      </div>
    </>
  )
}

export function DetailPane({ entry }: DetailPaneProps): ReactNode {
  const { t } = useLocale()
  if (entry === null) return <div className={styles.empty}>{t('trajectory.noEvents')}</div>

  const d = entry.detail
  if (!d) {
    return <div className={styles.plain}>{entry.title}</div>
  }

  switch (d.kind) {
    case 'iteration':
      return (
        <div>
          <Row label={t('trajectory.detail.infer')}>#{d.iteration} · {d.paradigm}</Row>
          {d.durationMs !== undefined && (
            <Row label={t('trajectory.detail.duration')}>{formatMs(d.durationMs)}</Row>
          )}
          {d.usage !== undefined && (
            <Row label={t('trajectory.detail.tokens')}>
              {d.usage.prompt_tokens} / {d.usage.completion_tokens}
              {d.usage.cache_read_tokens > 0 ? ` (cache ${d.usage.cache_read_tokens})` : ''}
            </Row>
          )}
          {d.thinking.length > 0 && <Row label={t('trajectory.detail.thinking')}><Mono text={d.thinking} /></Row>}
          {d.inference.length > 0 && <Row label={t('trajectory.detail.reasoning')}><Mono text={d.inference} /></Row>}
          {d.inferenceDetail !== undefined && <InferenceDetail snap={d.inferenceDetail} />}
        </div>
      )
    case 'context': {
      const total = d.sections.reduce((a, s) => a + s.tokens, 0)
      const changed = d.sections.filter((s) => s.changed).length
      return (
        <div>
          <Row label={t('trajectory.detail.iteration')}>#{d.iteration}</Row>
          {d.durationMs !== undefined && (
            <Row label={t('trajectory.detail.duration')}>{formatMs(d.durationMs)}</Row>
          )}
          <Row label={t('trajectory.detail.contextStats')}>
            {d.sections.length} sections · ~{total} tok · {changed} changed
          </Row>
          <div className={styles.sections}>
            {d.sections.map((s, i) => (
              <details key={i} className={styles.section}>
                <summary>
                  <span className={styles.sectionLabel}>
                    {s.changed ? '• ' : ''}{s.label}
                  </span>
                  <span className={styles.sectionTokens}>~{s.tokens} tok</span>
                </summary>
                <Mono text={s.content} />
              </details>
            ))}
          </div>
        </div>
      )
    }
    case 'tool':
      return (
        <div>
          <Row label={t('trajectory.detail.duration')}>
            {d.durationMs !== undefined ? `${d.durationMs}ms` : '—'}
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
    default:
      return <div className={styles.plain}>{entry.title}</div>
  }
}
