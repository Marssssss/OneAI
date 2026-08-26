// JsonTree — a small collapsible JSON viewer (issue #40 follow-up). The API
// request/response bodies in the trajectory infer drill-in are large nested
// blobs; a flat <pre> is hard to scan, so we render them as a tree with
// per-node expand/collapse. Zero dependency — plain recursion + local state.

import { useState } from 'react'
import type { ReactNode } from 'react'
import styles from './JsonTree.module.css'

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function Primitive({
  name,
  value,
  depth,
}: {
  name: string | undefined
  value: unknown
  depth: number
}): ReactNode {
  const raw = typeof value === 'string' ? JSON.stringify(value) : String(value)
  const cls =
    value === null
      ? styles.null
      : typeof value === 'string'
        ? styles.string
        : typeof value === 'number'
          ? styles.number
          : styles.boolean
  return (
    <div className={styles.line} style={{ paddingLeft: depth * 16 }}>
      {name !== undefined && <span className={styles.key}>{name}</span>}
      {name !== undefined && <span className={styles.colon}>: </span>}
      <span className={cls}>{raw}</span>
    </div>
  )
}

function Branch({
  name,
  value,
  depth,
}: {
  name: string | undefined
  value: Record<string, unknown> | unknown[]
  depth: number
}): ReactNode {
  // The root branch (depth 0) is open so the top-level keys are visible;
  // nested containers stay collapsed until the user opens them.
  const [open, setOpen] = useState(depth < 1)
  const isArray = Array.isArray(value)
  const entries: Array<[string, unknown]> = isArray
    ? (value as unknown[]).map((v, i) => [String(i), v] as [string, unknown])
    : Object.entries(value as Record<string, unknown>)
  const summary = isArray
    ? `[${(value as unknown[]).length}]`
    : `{${Object.keys(value as Record<string, unknown>).length}}`
  return (
    <div>
      <div className={styles.branch} style={{ paddingLeft: depth * 16 }} onClick={() => setOpen((o) => !o)}>
        <span className={styles.toggle}>{open ? '▾' : '▸'}</span>
        {name !== undefined && <span className={styles.key}>{name}</span>}
        <span className={styles.summary}>{summary}</span>
      </div>
      {open && entries.map(([k, v]) => <JsonNode key={k} name={k} value={v} depth={depth + 1} />)}
    </div>
  )
}

function JsonNode({ name, value, depth }: { name: string | undefined; value: unknown; depth: number }): ReactNode {
  if (isRecord(value) || Array.isArray(value)) {
    return <Branch name={name} value={value as Record<string, unknown> | unknown[]} depth={depth} />
  }
  return <Primitive name={name} value={value} depth={depth} />
}

export function JsonTree({ value }: { value: unknown }): ReactNode {
  return (
    <div className={styles.root}>
      <JsonNode name={undefined} value={value} depth={0} />
    </div>
  )
}
