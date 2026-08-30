// ToolCallNode — the disclosure card for one tool call. Mirrors dsh's
// `ToolCallTree` collapsed-header pattern: the header is one line showing
// the operation summary ("Bash: cargo test …"), collapsed by default; clicking
// expands an inline body with the full command (args) + execution output.
//
// A small ⤢ inspect affordance selects this call for the details rail (full
// unbounded text), preserving the W2 power-user path.
//
// A node is pending until its matching `tool_result` lands, then done/errored.

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
  const state = node.toolState ?? 'executing'
  const statusLabel =
    state === 'assembling'
      ? t('tool.assembling')
      : state === 'executing'
        ? t('tool.executing')
        : state === 'error'
          ? t('tool.error')
          : t('tool.done')
  const output = node.toolOutput
  const summary = summarizeCall(node.toolName, node.toolArgs)
  const [copied, setCopied] = useState<string | null>(null)
  const copyPath = (path: string) => {
    void navigator.clipboard?.writeText(path).then(() => {
      setCopied(path)
      window.setTimeout(() => setCopied(null), 1200)
    })
  }

  const added = output?.added_tool_names
  const artifacts = output?.artifacts

  return (
    <div
      className={`${styles.card} ${state === 'error' ? styles.error : ''} ${
        selected ? styles.selected : ''
      } ${open ? styles.cardOpen : ''}`}
    >
      <div className={styles.header} role="button" tabIndex={0} aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span className={`${styles.dot} ${styles[`dot_${state}`]}`} />
        <span className={styles.name}>{node.toolName ?? 'tool'}</span>
        {summary.length > 0 && <span className={styles.summary}>{summary}</span>}
        <span className={styles.status}>{statusLabel}</span>
        <button
          className={styles.inspectBtn}
          onClick={(e) => {
            e.stopPropagation()
            onSelect(node.id)
          }}
          title={t('tool.inspect')}
          aria-label={t('tool.inspect')}
        >
          ⤢
        </button>
        <span className={styles.chevron} aria-hidden>
          {open ? '▾' : '▸'}
        </span>
      </div>

      {open && (
        <div className={styles.body}>
          {node.toolArgs !== undefined && (
            <div className={styles.section}>
              <div className={styles.sectionLabel}>{t('tool.command')}</div>
              <pre className={styles.code}>{prettyArgs(node.toolArgs)}</pre>
            </div>
          )}
          {output !== undefined && (
            <div className={styles.section}>
              <div className={styles.sectionLabel}>{t('tool.result')}</div>
              {output.content.length > 0 && (
                <pre className={styles.code}>{output.content}</pre>
              )}
              {output.error !== undefined && output.error !== '' && (
                <pre className={`${styles.code} ${styles.errorText}`}>{output.error}</pre>
              )}
            </div>
          )}
          {artifacts !== undefined && artifacts.length > 0 && (
            <div className={styles.section}>
              <div className={styles.sectionLabel}>📎 {t('tool.artifacts')}</div>
              <div className={styles.artifacts}>
                {artifacts.map((a, i) => (
                  <div className={styles.artifact} key={`${a.path}-${i}`} title={a.description}>
                    <span className={styles.artifactIcon}>📄</span>
                    <span className={styles.artifactPath}>{a.path}</span>
                    <button
                      className={styles.copyBtn}
                      onClick={(e) => {
                        e.stopPropagation()
                        copyPath(a.path)
                      }}
                      aria-label="copy path"
                    >
                      {copied === a.path ? '✓' : '⧉'}
                    </button>
                  </div>
                ))}
              </div>
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

/** Derive a one-line operation summary from the tool name + args — the
 * collapsed-header label (e.g. "cargo test --package oneai-agent"). Returns ''
 * when nothing actionable can be inferred (the bare tool name stays). */
function summarizeCall(name: string | undefined, args: unknown): string {
  const n = (name ?? '').toLowerCase()
  const a = args
  if (typeof a === 'string') return truncate(stripShellNoise(a))
  if (a === null || typeof a !== 'object') return ''
  const obj = a as Record<string, unknown>
  // Shell/bash: the command is the headline.
  if (n.includes('bash') || n.includes('shell') || n.includes('exec')) {
    const cmd = pickStr(obj, ['command', 'cmd', 'script'])
    if (cmd !== null) return truncate(stripShellNoise(cmd))
  }
  // File tools: the path is the headline.
  if (n.includes('write') || n.includes('read') || n.includes('edit') || n.includes('patch') || n.includes('file')) {
    const path = pickStr(obj, ['path', 'file_path', 'filename', 'file'])
    if (path !== null) return truncate(path)
  }
  // Web tools: query/url.
  if (n.includes('web') || n.includes('fetch') || n.includes('search')) {
    const q = pickStr(obj, ['query', 'url', 'uri'])
    if (q !== null) return truncate(q)
  }
  // Generic: surface the first scalar field so the card isn't bare.
  for (const v of Object.values(obj)) {
    if (typeof v === 'string' && v.length > 0) return truncate(v)
  }
  return ''
}

function pickStr(obj: Record<string, unknown>, keys: string[]): string | null {
  for (const k of keys) {
    const v = obj[k]
    if (typeof v === 'string' && v.length > 0) return v
  }
  return null
}

/** Drop leading "cd … &&" / env prefixes so the headline shows the actual
 * action, not a chain of setup commands. */
function stripShellNoise(cmd: string): string {
  let s = cmd.trim()
  // Drop a leading `cd <dir> &&` (common in generated tool calls).
  s = s.replace(/^cd\s+\S+(\s+\S+)*\s*&&\s*/i, '')
  return s
}

function truncate(s: string, max = 80): string {
  const flat = s.replace(/\s+/g, ' ').trim()
  return flat.length > max ? flat.slice(0, max - 1) + '…' : flat
}

function prettyArgs(v: unknown): string {
  if (typeof v === 'string') return v
  try {
    return JSON.stringify(v, null, 2)
  } catch {
    return String(v)
  }
}
