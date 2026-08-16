// ScenarioEditor — the React scenario editor (W3). Replaces the vanilla
// `platforms/shared/scenario-editor.js` for the Web frontend while keeping
// the same transport-agnostic contract: it talks to `scenario/validate` /
// `scenario/upsert` / `scenario/delete` through a `ScenarioListStore` (which
// holds the `rpc.call` surface), never touching ws/stdio directly. Long-term
// the VS Code / browser extensions can share this React version; for now the
// vanilla copy stays for them.
//
// Field set mirrors `BusScenario`: name/icon/members/turn_policy/script_order/
// moderator_id/opener_agent_id/opener_line/topic_fields/debrief/review_loop/
// locale. Live-validates on a 300ms debounce via `scenario/validate` (the
// single authoritative validator — no client-side mirror to drift), renders
// localized errors inline. Presets (`preset-*` ids) are read-only.

import { useEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type {
  BusLocale,
  BusScenario,
  BusScenarioMember,
  ScenarioError,
} from '../rpc/types'
import { localizeErrors } from './errors'
import type { ScenarioListStore } from './scenarioStore'
import { Modal } from './Modal'
import styles from './ScenarioEditor.module.css'

interface ScenarioEditorProps {
  scenario: BusScenario | null
  store: ScenarioListStore
  onSaved: (id: string) => void
  onDeleted: (id: string) => void
  onClose: () => void
}

let blankSeq = 0
// Default speaker color for a freshly added member. Persisted as scenario
// data (a real hex — the `<input type=color>` requires it), so it is NOT a
// theme token; theme-awareness for colorless speakers lives in ChatView via
// `--oneai-speaker-fallback`.
const DEFAULT_SPEAKER_COLOR = '#4D6BFE'
function blankScenario(): BusScenario {
  const t = Date.now()
  blankSeq += 1
  return {
    id: `sc-${t}-${blankSeq}`,
    name: '',
    icon: '◆',
    members: [
      {
        id: `m${t}-${blankSeq}`,
        name: '',
        role: '',
        system_prompt: '',
        kind: 'openai',
        model: '',
        color: DEFAULT_SPEAKER_COLOR,
      },
    ],
    turn_policy: 'roundrobin',
    locale: 'zh',
  }
}

const POLICIES = ['scripted', 'moderator', 'roundrobin'] as const

export function ScenarioEditor({
  scenario,
  store,
  onSaved,
  onDeleted,
  onClose,
}: ScenarioEditorProps): ReactNode {
  const { t, locale } = useLocale()
  const [draft, setDraft] = useState<BusScenario>(() =>
    scenario !== null
      ? (JSON.parse(JSON.stringify(scenario)) as BusScenario)
      : blankScenario(),
  )
  const isPreset = draft.id.startsWith('preset-')
  const [errors, setErrors] = useState<ScenarioError[]>([])
  const [saving, setSaving] = useState(false)
  const [saveErr, setSaveErr] = useState<string | null>(null)
  const validateTimer = useRef<ReturnType<typeof setTimeout> | null>(null)

  // Live validate (debounced) — the engine is the single source of truth.
  useEffect(() => {
    if (validateTimer.current !== null) clearTimeout(validateTimer.current)
    validateTimer.current = setTimeout(async () => {
      const errs = await store.validate(draft)
      setErrors(errs)
    }, 300)
    return () => {
      if (validateTimer.current !== null) clearTimeout(validateTimer.current)
    }
  }, [draft, store])

  // Errors grouped by field dot-path for inline rendering.
  const errMap = useMemo(() => {
    const m = new Map<string, string[]>()
    for (const e of errors) {
      const list = m.get(e.field) ?? []
      list.push(localizeError(e))
      m.set(e.field, list)
    }
    return m
    // locale in deps so a locale flip re-localizes existing errors.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [errors, locale])

  function localizeError(e: ScenarioError): string {
    return localizeErrors([e], locale)[0].message
  }

  const patch = (p: Partial<BusScenario>): void => {
    setDraft((d) => ({ ...d, ...p }))
  }
  const patchMember = (idx: number, p: Partial<BusScenarioMember>): void => {
    setDraft((d) => ({
      ...d,
      members: d.members.map((m, i) => (i === idx ? { ...m, ...p } : m)),
    }))
  }
  const addMember = (): void => {
    const id = `m${Date.now()}-${draft.members.length}`
    setDraft((d) => ({
      ...d,
      members: [
        ...d.members,
        {
          id,
          name: '',
          role: '',
          system_prompt: '',
          kind: 'openai',
          model: '',
          color: DEFAULT_SPEAKER_COLOR,
        },
      ],
    }))
  }
  const removeMember = (idx: number): void => {
    setDraft((d) => ({ ...d, members: d.members.filter((_, i) => i !== idx) }))
  }

  const memberIds = draft.members.map((m) => m.id)

  const save = async (): Promise<void> => {
    setSaving(true)
    setSaveErr(null)
    const res = await store.upsert(draft)
    setSaving(false)
    if (res.ok) {
      onSaved(res.id)
    } else {
      setSaveErr(
        res.errors
          .map((e) => localizeError(e))
          .join('; '),
      )
      setErrors(res.errors)
    }
  }

  const del = async (): Promise<void> => {
    if (isPreset) return
    if (!window.confirm(t('scenario.confirmDelete'))) return
    await store.delete(draft.id)
    onDeleted(draft.id)
  }

  return (
    <Modal
      title={
        isPreset
          ? `${t('scenario.view')} · ${draft.name || draft.id}`
          : scenario !== null
            ? `${t('scenario.edit')} · ${draft.name || draft.id}`
            : t('scenario.new')
      }
      onClose={onClose}
      width={720}
      footer={
        <>
          {!isPreset && (
            <button className={styles.danger} onClick={del} disabled={saving}>
              {t('scenario.delete')}
            </button>
          )}
          <button className={styles.secondary} onClick={onClose}>
            {t('scenario.cancel')}
          </button>
          {!isPreset && (
            <button className={styles.primary} onClick={save} disabled={saving}>
              {saving ? t('scenario.saving') : t('scenario.save')}
            </button>
          )}
        </>
      }
    >
      <div className={styles.form}>
        {saveErr !== null && <div className={styles.saveErr}>{saveErr}</div>}
        {isPreset && <div className={styles.presetNote}>{t('scenario.presetNote')}</div>}

        <div className={styles.row2}>
          <Field label={t('scenario.fld.name')} error={errMap.get('name')}>
            <input
              className={styles.input}
              type="text"
              value={draft.name}
              disabled={isPreset}
              onChange={(e) => patch({ name: e.target.value })}
            />
          </Field>
          <Field label={t('scenario.fld.icon')}>
            <input
              className={styles.input}
              type="text"
              value={draft.icon ?? ''}
              disabled={isPreset}
              onChange={(e) => patch({ icon: e.target.value })}
            />
          </Field>
          <Field label={t('scenario.fld.locale')}>
            <select
              className={styles.input}
              value={draft.locale ?? 'zh'}
              disabled={isPreset}
              onChange={(e) => patch({ locale: e.target.value as BusLocale })}
            >
              <option value="zh">zh</option>
              <option value="en">en</option>
            </select>
          </Field>
        </div>

        <SectionLabel>{t('scenario.section.cast')}</SectionLabel>
        {draft.members.map((m, i) => (
          <div className={styles.member} key={m.id}>
            <div className={styles.row2}>
              <Field
                label={t('scenario.fld.mName')}
                error={errMap.get(`members.${i}.name`)}
              >
                <input
                  className={styles.input}
                  type="text"
                  value={m.name}
                  disabled={isPreset}
                  onChange={(e) => patchMember(i, { name: e.target.value })}
                />
              </Field>
              <Field label={t('scenario.fld.mRole')}>
                <input
                  className={styles.input}
                  type="text"
                  value={m.role ?? ''}
                  disabled={isPreset}
                  onChange={(e) => patchMember(i, { role: e.target.value })}
                />
              </Field>
              <Field label={t('scenario.fld.mColor')}>
                <input
                  className={styles.color}
                  type="color"
                  value={m.color ?? DEFAULT_SPEAKER_COLOR}
                  disabled={isPreset}
                  onChange={(e) => patchMember(i, { color: e.target.value })}
                />
              </Field>
            </div>
            <Field
              label={t('scenario.fld.mPrompt')}
              error={errMap.get(`members.${i}.system_prompt`)}
            >
              <textarea
                className={styles.textarea}
                rows={3}
                value={m.system_prompt}
                disabled={isPreset}
                onChange={(e) => patchMember(i, { system_prompt: e.target.value })}
              />
            </Field>
            <div className={styles.row2}>
              <Field label={t('scenario.fld.mId')}>
                <input className={styles.inputMono} type="text" value={m.id} readOnly />
              </Field>
              <Field label={t('scenario.fld.mKind')}>
                <input
                  className={styles.input}
                  type="text"
                  value={m.kind ?? 'openai'}
                  disabled={isPreset}
                  onChange={(e) => patchMember(i, { kind: e.target.value })}
                />
              </Field>
              <Field label={t('scenario.fld.mModel')}>
                <input
                  className={styles.input}
                  type="text"
                  value={m.model ?? ''}
                  disabled={isPreset}
                  placeholder="inherit"
                  onChange={(e) => patchMember(i, { model: e.target.value })}
                />
              </Field>
              {!isPreset && (
                <button className={styles.miniBtn} onClick={() => removeMember(i)}>
                  {t('scenario.remove')}
                </button>
              )}
            </div>
          </div>
        ))}
        {!isPreset && (
          <button className={styles.addBtn} onClick={addMember}>
            {t('scenario.addMember')}
          </button>
        )}

        <SectionLabel>{t('scenario.section.flow')}</SectionLabel>
        <div className={styles.row2}>
          <Field label={t('scenario.fld.policy')} error={errMap.get('turn_policy')}>
            <select
              className={styles.input}
              value={draft.turn_policy}
              disabled={isPreset}
              onChange={(e) => patch({ turn_policy: e.target.value })}
            >
              {POLICIES.map((p) => (
                <option key={p} value={p}>
                  {p}
                </option>
              ))}
            </select>
          </Field>
          <Field
            label={t('scenario.fld.moderator')}
            error={errMap.get('moderator_id')}
          >
            <IdSelect
              ids={memberIds}
              value={draft.moderator_id}
              disabled={isPreset}
              allowNone
              onChange={(v) => patch({ moderator_id: v })}
            />
          </Field>
        </div>
        <div className={styles.row2}>
          <Field
            label={t('scenario.fld.opener')}
            error={errMap.get('opener_agent_id')}
          >
            <IdSelect
              ids={memberIds}
              value={draft.opener_agent_id}
              disabled={isPreset}
              allowNone
              onChange={(v) => patch({ opener_agent_id: v })}
            />
          </Field>
          <Field label={t('scenario.fld.openerLine')}>
            <input
              className={styles.input}
              type="text"
              value={draft.opener_line ?? ''}
              disabled={isPreset}
              onChange={(e) => patch({ opener_line: e.target.value })}
            />
          </Field>
        </div>
        <Field
          label={t('scenario.fld.scriptOrder')}
          error={errMap.get('script_order')}
          hint={t('scenario.fld.scriptOrderHint')}
        >
          <input
            className={styles.input}
            type="text"
            value={(draft.script_order ?? []).join(', ')}
            disabled={isPreset}
            onChange={(e) =>
              patch({
                script_order: parseIds(e.target.value),
              })
            }
          />
        </Field>

        <SectionLabel>{t('scenario.section.topic')}</SectionLabel>
        {(draft.topic_fields ?? []).map((f, fi) => (
          <div className={styles.member} key={f.id}>
            <div className={styles.row2}>
              <Field label={t('scenario.fld.tfLabel')}>
                <input
                  className={styles.input}
                  type="text"
                  value={f.label}
                  disabled={isPreset}
                  onChange={(e) =>
                    patchTopic(setDraft, fi, { label: e.target.value })
                  }
                />
              </Field>
              <Field label={t('scenario.fld.tfPlaceholder')}>
                <input
                  className={styles.input}
                  type="text"
                  value={f.placeholder ?? ''}
                  disabled={isPreset}
                  onChange={(e) =>
                    patchTopic(setDraft, fi, { placeholder: e.target.value })
                  }
                />
              </Field>
            </div>
            <div className={styles.row2}>
              <Field
                label={t('scenario.fld.tfId')}
                error={errMap.get(`topic_fields.${fi}.visible_to`)}
              >
                <input className={styles.inputMono} type="text" value={f.id} readOnly />
              </Field>
              <Field
                label={t('scenario.fld.tfVisibleTo')}
                hint={t('scenario.fld.tfVisibleToHint')}
              >
                <input
                  className={styles.input}
                  type="text"
                  value={(f.visible_to ?? []).join(', ')}
                  disabled={isPreset}
                  onChange={(e) =>
                    patchTopic(setDraft, fi, {
                      visible_to: parseIds(e.target.value),
                    })
                  }
                />
              </Field>
            </div>
          </div>
        ))}

        <SectionLabel>{t('scenario.section.debrief')}</SectionLabel>
        <Field
          label={t('scenario.fld.debriefMember')}
          error={errMap.get('debrief.debrief_member_id')}
        >
          <IdSelect
            ids={memberIds}
            value={draft.debrief?.debrief_member_id}
            disabled={isPreset}
            allowNone
            onChange={(v) =>
              patch({
                debrief:
                  v === undefined
                    ? undefined
                    : {
                        button_label: draft.debrief?.button_label ?? '',
                        summary_prompt: draft.debrief?.summary_prompt ?? '',
                        debrief_member_id: v,
                      },
              })
            }
          />
        </Field>
        {draft.debrief !== undefined && (
          <>
            <Field label={t('scenario.fld.debriefButton')}>
              <input
                className={styles.input}
                type="text"
                value={draft.debrief.button_label}
                disabled={isPreset}
                onChange={(e) =>
                  patch({
                    debrief: { ...draft.debrief!, button_label: e.target.value },
                  })
                }
              />
            </Field>
            <Field label={t('scenario.fld.debriefSummary')}>
              <textarea
                className={styles.textarea}
                rows={2}
                value={draft.debrief.summary_prompt}
                disabled={isPreset}
                onChange={(e) =>
                  patch({
                    debrief: { ...draft.debrief!, summary_prompt: e.target.value },
                  })
                }
              />
            </Field>
          </>
        )}

        <SectionLabel>{t('scenario.section.review')}</SectionLabel>
        <div className={styles.row2}>
          <Field
            label={t('scenario.fld.reviewer')}
            error={errMap.get('review_loop.reviewer_id')}
          >
            <IdSelect
              ids={memberIds}
              value={draft.review_loop?.reviewer_id}
              disabled={isPreset}
              allowNone
              onChange={(v) =>
                patch({
                  review_loop:
                    v === undefined
                      ? undefined
                      : {
                          reviewer_id: v,
                          approve_marker: draft.review_loop?.approve_marker ?? '',
                          max_rounds: draft.review_loop?.max_rounds ?? 1,
                        },
                })
              }
            />
          </Field>
          <Field label={t('scenario.fld.marker')}>
            <input
              className={styles.input}
              type="text"
              value={draft.review_loop?.approve_marker ?? ''}
              disabled={isPreset || draft.review_loop === undefined}
              onChange={(e) =>
                patch({
                  review_loop:
                    draft.review_loop === undefined
                      ? undefined
                      : { ...draft.review_loop, approve_marker: e.target.value },
                })
              }
            />
          </Field>
          <Field
            label={t('scenario.fld.maxRounds')}
            error={errMap.get('review_loop.max_rounds')}
          >
            <input
              className={styles.input}
              type="number"
              min={1}
              value={draft.review_loop?.max_rounds ?? 1}
              disabled={isPreset || draft.review_loop === undefined}
              onChange={(e) =>
                patch({
                  review_loop:
                    draft.review_loop === undefined
                      ? undefined
                      : {
                          ...draft.review_loop,
                          max_rounds: Math.max(1, Number(e.target.value) || 1),
                        },
                })
              }
            />
          </Field>
        </div>
      </div>
    </Modal>
  )
}

function patchTopic(
  setDraft: (fn: (d: BusScenario) => BusScenario) => void,
  idx: number,
  p: Partial<{ id: string; label: string; placeholder: string | undefined; visible_to: string[] | undefined }>,
): void {
  setDraft((d) => ({
    ...d,
    topic_fields: (d.topic_fields ?? []).map((f, i) =>
      i === idx ? { ...f, ...p } : f,
    ),
  }))
}

function parseIds(s: string): string[] | undefined {
  const ids = s
    .split(',')
    .map((x) => x.trim())
    .filter((x) => x.length > 0)
  return ids.length > 0 ? ids : undefined
}

function Field({
  label,
  error,
  hint,
  children,
}: {
  label: string
  error?: string[]
  hint?: string
  children: ReactNode
}): ReactNode {
  return (
    <label className={styles.field}>
      <span className={styles.label}>{label}</span>
      {children}
      {hint !== undefined && <span className={styles.hint}>{hint}</span>}
      {error !== undefined &&
        error.map((m, i) => (
          <span key={i} className={styles.err}>
            {m}
          </span>
        ))}
    </label>
  )
}

function SectionLabel({ children }: { children: ReactNode }): ReactNode {
  return <div className={styles.sectionLabel}>{children}</div>
}

function IdSelect({
  ids,
  value,
  allowNone,
  disabled,
  onChange,
}: {
  ids: string[]
  value?: string
  allowNone?: boolean
  disabled?: boolean
  onChange: (v: string | undefined) => void
}): ReactNode {
  return (
    <select
      className={styles.input}
      value={value ?? ''}
      disabled={disabled}
      onChange={(e) => onChange(e.target.value === '' ? undefined : e.target.value)}
    >
      {allowNone && <option value="">—</option>}
      {ids.map((id) => (
        <option key={id} value={id}>
          {id}
        </option>
      ))}
    </select>
  )
}
