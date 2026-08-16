// ApprovalPanel — composer-takeover approval bar. Mirrors dsh's amber
// "Waiting for approval" bar. Handles all 5 InteractionRequest variants.
//
// Parallel approval queue (issue #20): the projection holds an approval
// queue; `currentApproval` is its head. When a second approval_request arrives
// before the first resolves it enqueues behind — the head is the only one the
// UI shows, and `promote_next` (in the store, on respond) advances it. The
// `queueDepth` badge surfaces "N more queued".
//
// Wire contract: InteractionRequest/Response are externally-tagged enums, no
// rename — `{ToolApproval: {approval}}` / `{Proceed: null}` / etc. We narrow
// by the single object key.

import { useState } from 'react'
import type { ReactNode } from 'react'
import type {
  ApprovalItem,
} from '../store/projection'
import type {
  ApprovalRequest,
  InteractionRequest,
  InteractionResponse,
} from '../rpc/types'
import { useLocale } from '../i18n'
import styles from './ApprovalPanel.module.css'

interface ApprovalPanelProps {
  current: ApprovalItem | null
  queueDepth: number
  onRespond: (requestId: string, response: InteractionResponse) => void
}

type RequestVariant =
  | { kind: 'tool'; approval: ApprovalRequest }
  | { kind: 'plan_review'; plan: string; steps: { id: string; description: string }[] }
  | { kind: 'plan_decision'; decision_id: string; question: string; context: string; options: { id: string; label: string; description: string; tradeoffs: string }[] }
  | { kind: 'network'; host: string; requested_by: string }
  | { kind: 'elicitation'; server: string; message: string; requested_schema: unknown }

function narrow(req: InteractionRequest): RequestVariant {
  const key = Object.keys(req)[0] as keyof InteractionRequest
  const payload = (req as Record<string, Record<string, unknown>>)[key] ?? {}
  switch (key) {
    case 'ToolApproval':
      return { kind: 'tool', approval: payload as unknown as ApprovalRequest }
    case 'PlanReview':
      return { kind: 'plan_review', ...(payload as unknown as { plan: string; steps: { id: string; description: string }[] }) }
    case 'PlanDecision':
      return { kind: 'plan_decision', ...(payload as unknown as { decision_id: string; question: string; context: string; options: { id: string; label: string; description: string; tradeoffs: string }[] }) }
    case 'NetworkApproval':
      return { kind: 'network', ...(payload as unknown as { host: string; requested_by: string }) }
    case 'McpElicitation':
      return { kind: 'elicitation', ...(payload as unknown as { server: string; message: string; requested_schema: unknown }) }
    default:
      // Unknown variant — the bus contract: ignore. Render a generic
      // proceed/abort so the turn isn't wedged.
      return { kind: 'network', host: '(unknown)', requested_by: '' }
  }
}

export function ApprovalPanel({
  current,
  queueDepth,
  onRespond,
}: ApprovalPanelProps): ReactNode {
  const { t } = useLocale()
  if (current === null) return null
  const variant = narrow(current.request)

  return (
    <div className={styles.bar}>
      <div className={styles.headline}>
        <span className={styles.icon}>⚠</span>
        <span className={styles.title}>{t('approval.waiting')}</span>
        {queueDepth > 0 && (
          <span className={styles.queue}>+{queueDepth} {t('approval.queued')}</span>
        )}
      </div>
      <div className={styles.body}>
        {variant.kind === 'tool' && (
          <ToolApprovalView approval={variant.approval} onRespond={(r) => onRespond(current.request_id, r)} respondLabel={t('approval.allow')} refuseLabel={t('approval.refuse')} />
        )}
        {variant.kind === 'plan_review' && (
          <PlanReviewView plan={variant.plan} steps={variant.steps} onRespond={(r) => onRespond(current.request_id, r)} acceptLabel={t('approval.accept')} reviseLabel={t('approval.revise')} revisePlaceholder={t('approval.revise.placeholder')} />
        )}
        {variant.kind === 'plan_decision' && (
          <PlanDecisionView decision_id={variant.decision_id} question={variant.question} context={variant.context} options={variant.options} onRespond={(r) => onRespond(current.request_id, r)} pickLabel={t('approval.pick')} />
        )}
        {variant.kind === 'network' && (
          <NetworkApprovalView host={variant.host} requested_by={variant.requested_by} onRespond={(r) => onRespond(current.request_id, r)} allowLabel={t('approval.allow.host')} denyLabel={t('approval.deny')} />
        )}
        {variant.kind === 'elicitation' && (
          <ElicitationView server={variant.server} message={variant.message} requested_schema={variant.requested_schema} onRespond={(r) => onRespond(current.request_id, r)} submitLabel={t('approval.submit')} declineLabel={t('approval.decline')} cancelLabel={t('approval.cancel')} />
        )}
      </div>
    </div>
  )
}

// ── ToolApproval ──────────────────────────────────────────────────────────────

function ToolApprovalView({
  approval,
  onRespond,
  respondLabel,
  refuseLabel,
}: {
  approval: ApprovalRequest
  onRespond: (r: InteractionResponse) => void
  respondLabel: string
  refuseLabel: string
}): ReactNode {
  return (
    <>
      <div className={styles.attribution}>
        <code className={styles.toolName}>{approval.tool_name}</code>
        {approval.justification && (
          <span className={styles.just}>{approval.justification}</span>
        )}
      </div>
      <pre className={styles.code}>{prettyJson(approval.args)}</pre>
      <div className={styles.actions}>
        <button className={styles.allowBtn} onClick={() => onRespond({ Proceed: null })}>
          {respondLabel}
        </button>
        <button className={styles.refuseBtn} onClick={() => onRespond({ Abort: { reason: 'user refused' } })}>
          {refuseLabel}
        </button>
      </div>
    </>
  )
}

// ── PlanReview ─────────────────────────────────────────────────────────────────

function PlanReviewView({
  plan,
  steps,
  onRespond,
  acceptLabel,
  reviseLabel,
  revisePlaceholder,
}: {
  plan: string
  steps: { id: string; description: string }[]
  onRespond: (r: InteractionResponse) => void
  acceptLabel: string
  reviseLabel: string
  revisePlaceholder: string
}): ReactNode {
  const [feedback, setFeedback] = useState('')
  return (
    <>
      <pre className={styles.code}>{plan}</pre>
      {steps.length > 0 && (
        <ol className={styles.steps}>
          {steps.map((s) => (
            <li key={s.id}>{s.description}</li>
          ))}
        </ol>
      )}
      <textarea
        className={styles.feedback}
        placeholder={revisePlaceholder}
        value={feedback}
        onChange={(e) => setFeedback(e.target.value)}
        rows={2}
      />
      <div className={styles.actions}>
        <button className={styles.allowBtn} onClick={() => onRespond({ Proceed: null })}>
          {acceptLabel}
        </button>
        <button
          className={styles.refuseBtn}
          onClick={() => onRespond({ Revise: { feedback: feedback.trim() || 'please revise' } })}
        >
          {reviseLabel}
        </button>
      </div>
    </>
  )
}

// ── PlanDecision ───────────────────────────────────────────────────────────────

function PlanDecisionView({
  question,
  context,
  options,
  onRespond,
  pickLabel,
}: {
  decision_id: string
  question: string
  context: string
  options: { id: string; label: string; description: string; tradeoffs: string }[]
  onRespond: (r: InteractionResponse) => void
  pickLabel: string
}): ReactNode {
  return (
    <>
      <div className={styles.question}>{question}</div>
      {context && <div className={styles.context}>{context}</div>}
      <div className={styles.options}>
        {options.map((o) => (
          <button key={o.id} className={styles.option} onClick={() => onRespond({ Choose: { option_id: o.id } })}>
            <span className={styles.optionLabel}>{o.label}</span>
            <span className={styles.optionDesc}>{o.description}</span>
            {o.tradeoffs && <span className={styles.optionTrade}>{o.tradeoffs}</span>}
          </button>
        ))}
      </div>
      <div className={styles.actions}>
        <span className={styles.hint}>{pickLabel}</span>
      </div>
    </>
  )
}

// ── NetworkApproval ────────────────────────────────────────────────────────────
// Allow = Proceed (the engine proxy records the host in its session allow-list
// on Proceed, so subsequent same-host attempts don't re-prompt — issue #28
// stage 6). There is no `host/*` RPC, so a persistent cross-session "always"
// is not wire-supported; that's a W4 gap.

function NetworkApprovalView({
  host,
  requested_by,
  onRespond,
  allowLabel,
  denyLabel,
}: {
  host: string
  requested_by: string
  onRespond: (r: InteractionResponse) => void
  allowLabel: string
  denyLabel: string
}): ReactNode {
  return (
    <>
      <div className={styles.attribution}>
        <code className={styles.toolName}>{host}</code>
        {requested_by && <span className={styles.just}>← {requested_by}</span>}
      </div>
      <div className={styles.actions}>
        <button className={styles.allowBtn} onClick={() => onRespond({ Proceed: null })} title={t_networkHint()}>
          {allowLabel}
        </button>
        <button className={styles.refuseBtn} onClick={() => onRespond({ Abort: { reason: `denied host ${host}` } })}>
          {denyLabel}
        </button>
      </div>
    </>
  )
}

// Localized hint helper without re-rendering the parent: read from the locale
// store lazily (good enough for a tooltip).
function t_networkHint(): string {
  return 'Allow admits this host for the session (no persistent allowlist RPC yet)'
}

// ── McpElicitation (schema-driven form) ───────────────────────────────────────

interface SchemaField {
  key: string
  type: 'string' | 'boolean' | 'number' | 'integer'
  label: string
  enumValues?: string[]
  required: boolean
  default?: unknown
}

function parseSchema(schema: unknown): { fields: SchemaField[]; requiredSet: Set<string> } {
  const fields: SchemaField[] = []
  const s = schema as {
    type?: string
    properties?: Record<string, { type?: string; enum?: unknown[]; description?: string; default?: unknown }>
    required?: string[]
  } | null
  if (s === null || typeof s !== 'object' || s.properties === undefined) {
    return { fields, requiredSet: new Set() }
  }
  const required = new Set(s.required ?? [])
  for (const [key, def] of Object.entries(s.properties)) {
    const ft = def.type === 'boolean' ? 'boolean' : def.type === 'number' || def.type === 'integer' ? 'number' : 'string'
    fields.push({
      key,
      type: ft,
      label: def.description ?? key,
      enumValues: Array.isArray(def.enum) ? (def.enum as string[]) : undefined,
      required: required.has(key),
      default: def.default,
    })
  }
  return { fields, requiredSet: required }
}

function ElicitationView({
  server,
  message,
  requested_schema,
  onRespond,
  submitLabel,
  declineLabel,
  cancelLabel,
}: {
  server: string
  message: string
  requested_schema: unknown
  onRespond: (r: InteractionResponse) => void
  submitLabel: string
  declineLabel: string
  cancelLabel: string
}): ReactNode {
  const { fields } = parseSchema(requested_schema)
  const [values, setValues] = useState<Record<string, unknown>>(() => {
    const init: Record<string, unknown> = {}
    for (const f of fields) {
      if (f.default !== undefined) init[f.key] = f.default
      else if (f.type === 'boolean') init[f.key] = false
      else init[f.key] = ''
    }
    return init
  })

  const canSubmit = fields.every((f) => !f.required || (values[f.key] !== '' && values[f.key] !== undefined && values[f.key] !== null))

  const submit = () => {
    onRespond({ ElicitationReply: { action: 'accept', data: { ...values } } })
  }

  return (
    <>
      <div className={styles.attribution}>
        <code className={styles.toolName}>{server}</code>
      </div>
      <div className={styles.message}>{message}</div>
      <div className={styles.form}>
        {fields.map((f) => (
          <label key={f.key} className={styles.field}>
            <span className={styles.fieldLabel}>
              {f.label}
              {f.required && <span className={styles.req}>*</span>}
            </span>
            {f.enumValues !== undefined ? (
              <select
                className={styles.input}
                value={String(values[f.key] ?? '')}
                onChange={(e) => setValues((v) => ({ ...v, [f.key]: e.target.value }))}
              >
                <option value="">—</option>
                {f.enumValues.map((opt) => (
                  <option key={opt} value={opt}>{opt}</option>
                ))}
              </select>
            ) : f.type === 'boolean' ? (
              <input
                type="checkbox"
                checked={Boolean(values[f.key])}
                onChange={(e) => setValues((v) => ({ ...v, [f.key]: e.target.checked }))}
              />
            ) : (
              <input
                className={styles.input}
                type={f.type === 'number' ? 'number' : 'text'}
                value={String(values[f.key] ?? '')}
                onChange={(e) =>
                  setValues((v) => ({
                    ...v,
                    [f.key]: f.type === 'number' ? Number(e.target.value) : e.target.value,
                  }))
                }
              />
            )}
          </label>
        ))}
      </div>
      <div className={styles.actions}>
        <button className={styles.allowBtn} onClick={submit} disabled={!canSubmit}>
          {submitLabel}
        </button>
        <button className={styles.refuseBtn} onClick={() => onRespond({ ElicitationReply: { action: 'decline' } })}>
          {declineLabel}
        </button>
        <button className={styles.neutralBtn} onClick={() => onRespond({ ElicitationReply: { action: 'cancel' } })}>
          {cancelLabel}
        </button>
      </div>
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
