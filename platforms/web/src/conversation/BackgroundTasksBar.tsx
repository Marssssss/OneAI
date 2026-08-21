// BackgroundTasksBar — a collapsible strip of in-flight background sub-agents
// at the top of the chat column. Unlike the trajectory tab's SubagentTree
// (turn-scoped), this reads the cross-turn `backgroundTasks` slice, so a
// sub-agent launched in turn A and completing in turn B stays visible the
// whole time. Collapsible: defaults to a compact one-line summary (count +
// active/done breakdown) so it never occludes the conversation; click to
// expand the per-task cards. Auto-expands while any task is active so the
// user sees live progress, collapses to summary when all are done. Pure
// consumer of the projection snapshot.

import { useState } from 'react'
import type { ReactNode } from 'react'
import type { BackgroundTaskNode } from '../store/trajectory'
import { useLocale } from '../i18n'
import subStyles from '../details/SubagentTree.module.css'

interface BackgroundTasksBarProps {
  tasks: BackgroundTaskNode[]
}

function kindLabel(k: BackgroundTaskNode['agentKind']): string {
  return typeof k === 'string' ? k : `Custom:${k.Custom}`
}

export function BackgroundTasksBar({ tasks }: BackgroundTasksBarProps): ReactNode | null {
  const { t } = useLocale()
  const [expanded, setExpanded] = useState(false)
  if (tasks.length === 0) return null

  const active = tasks.filter((s) => s.status === 'active').length
  const done = tasks.filter((s) => s.status === 'done').length
  const failed = tasks.filter((s) => s.status === 'failed').length
  // Auto-expand while work is in flight; collapse to the summary once all
  // settled (the user can still re-expand).
  const open = expanded || active > 0

  const summary = [
    `${active} ${t('subagent.active')}`,
    done > 0 ? `${done} ${t('subagent.done')}` : null,
    failed > 0 ? `${failed} failed` : null,
  ]
    .filter(Boolean)
    .join(' · ')

  return (
    <section className={subStyles.root} aria-label={t('subagent.title')}>
      <button
        className={subStyles.label}
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={open}
        style={{
          cursor: 'pointer',
          background: 'none',
          border: 'none',
          padding: 0,
          font: 'inherit',
          color: 'inherit',
          display: 'flex',
          alignItems: 'center',
          gap: 6,
          width: '100%',
        }}
      >
        <span>{open ? '▾' : '▸'}</span>
        <span>{t('subagent.title')}</span>
        <span>· {summary}</span>
      </button>
      {open && (
        <ul className={subStyles.list}>
          {tasks.map((s) => {
            const dot = s.status === 'active' ? subStyles.dot_active : subStyles.dot_done
            return (
              <li key={s.taskId} className={subStyles.item}>
                <span className={`${subStyles.dot} ${dot}`} aria-hidden />
                <div className={subStyles.body}>
                  <div className={subStyles.head}>
                    <span className={subStyles.kind}>{kindLabel(s.agentKind)}</span>
                    <span className={subStyles.status}>
                      {s.status === 'active'
                        ? s.iteration
                          ? `${t('subagent.active')} · iter ${s.iteration}`
                          : t('subagent.active')
                        : s.status === 'done'
                          ? t('subagent.done')
                          : 'failed'}
                    </span>
                  </div>
                  <div className={subStyles.task} title={s.task}>
                    {s.task}
                    {s.lastTool ? ` · ${s.lastTool}` : ''}
                  </div>
                  {s.summary && s.summary.summary.length > 0 && (
                    <div className={subStyles.summary}>
                      <div className={subStyles.summaryText}>{s.summary.summary}</div>
                    </div>
                  )}
                </div>
              </li>
            )
          })}
        </ul>
      )}
    </section>
  )
}
