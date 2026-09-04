// SettingsRoot — the settings modal. Three sections (left rail nav):
// General / Models / Domain. Skills remains a standalone modal (`/skills`).
//
// Models is the deepseek-harness-style provider manager: shows the config
// file path (web can't reveal Finder — shows the path + a copy button + a
// read-only TOML preview), lists configured providers with active/delete/
// set-active, and an add-provider form. Adds/deletes/switches go through the
// `provider/add` · `/delete` · `/set_active` RPCs, which write to
// `~/.oneai/config.toml` AND mutate the live `ProviderPool` (a new provider is
// immediately switchable from the composer's ModelSelect — no restart).
//
// Domain is the live DomainPack switcher: a dropdown over `domainpack/list`,
// switching via `domainpack/switch` (hot-swap, no restart), plus the active
// permission profile for context.

import { useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import { Modal } from '../scenario/Modal'
import { useLocale } from '../i18n'
import type { HostListResult } from '../rpc/types'
import type { SettingsStore } from './settingsStore'
import { providerKindLabel, useSettings } from './settingsStore'
import styles from './SettingsRoot.module.css'

type Section = 'general' | 'models' | 'domain' | 'network'

/** §B5 — durable host allow/deny CRUD surface for the Settings modal. Backed
 * by the `host/*` JSON-RPC methods (the engine's `~/.oneai/oneai.db` durable
 * store, shared with the `NetworkProxy` hot path). `undefined` hides the
 * section (only relevant when host/* RPC is wired, which the app-server always
 * wires in production). */
export interface HostOps {
  list: () => Promise<HostListResult>
  remove: (host: string) => Promise<void>
  removeDenied: (host: string) => Promise<void>
}

interface SettingsRootProps {
  store: SettingsStore
  theme: 'light' | 'dark'
  locale: 'zh' | 'en'
  connection: 'connecting' | 'open' | 'closed' | 'error'
  onToggleTheme: () => void
  onToggleLocale: () => void
  hostOps?: HostOps
  onClose: () => void
}

export function SettingsRoot({
  store,
  theme,
  locale,
  connection,
  onToggleTheme,
  onToggleLocale,
  hostOps,
  onClose,
}: SettingsRootProps): ReactNode {
  const { t } = useLocale()
  const snap = useSettings(store)
  const [section, setSection] = useState<Section>('general')

  // Pull the read-only snapshots on open + whenever the modal re-opens.
  useEffect(() => {
    void store.refresh()
  }, [store])

  const sections: { id: Section; label: string }[] = [
    { id: 'general', label: t('settings.general') },
    { id: 'models', label: t('settings.models') },
    { id: 'domain', label: t('settings.domain') },
    ...(hostOps !== undefined ? [{ id: 'network' as const, label: t('settings.network') }] : []),
  ]

  return (
    <Modal title={t('settings.title')} width={920} onClose={onClose}>
      <div className={styles.layout}>
        <nav className={styles.rail}>
          {sections.map((s) => (
            <button
              key={s.id}
              className={`${styles.railItem} ${section === s.id ? styles.railItemActive : ''}`}
              onClick={() => setSection(s.id)}
            >
              {s.label}
            </button>
          ))}
        </nav>
        <div className={styles.content}>
          {section === 'general' && (
            <GeneralSection
              theme={theme}
              locale={locale}
              connection={connection}
              onToggleTheme={onToggleTheme}
              onToggleLocale={onToggleLocale}
            />
          )}
          {section === 'models' && <ModelsSection store={store} snap={snap} />}
          {section === 'domain' && <DomainSection store={store} snap={snap} />}
          {section === 'network' && hostOps !== undefined && (
            <NetworkSection hostOps={hostOps} />
          )}
        </div>
      </div>
    </Modal>
  )
}

function Row({ label, children }: { label: string; children: ReactNode }): ReactNode {
  return (
    <div className={styles.row}>
      <span className={styles.rowLabel}>{label}</span>
      <div className={styles.rowValue}>{children}</div>
    </div>
  )
}

function GeneralSection({
  theme,
  locale,
  connection,
  onToggleTheme,
  onToggleLocale,
}: {
  theme: 'light' | 'dark'
  locale: 'zh' | 'en'
  connection: 'connecting' | 'open' | 'closed' | 'error'
  onToggleTheme: () => void
  onToggleLocale: () => void
}): ReactNode {
  const { t } = useLocale()
  return (
    <div className={styles.stack}>
      <Row label={t('settings.theme')}>
        <button className={styles.toggle} onClick={onToggleTheme}>
          {theme === 'dark' ? '☾ ' + t('settings.dark') : '☀ ' + t('settings.light')}
        </button>
      </Row>
      <Row label={t('settings.language')}>
        <button className={styles.toggle} onClick={onToggleLocale}>
          {locale === 'zh' ? '中文' : 'English'}
        </button>
      </Row>
      <Row label={t('settings.thinkingEffort')}>
        <span className={styles.hint}>{t('settings.thinkingEffortMoved')}</span>
      </Row>
      <Row label={t('settings.connection')}>
        <span className={styles.value}>{t(`status.${connection}`)}</span>
      </Row>
    </div>
  )
}

/** The fixed protocol kinds the engine understands (issue #37 — a dropdown,
 *  not free text). `ollama` maps to `ProviderType::Local` server-side. These
 *  are wire values; the UI shows protocol labels via `providerKindLabel`. */
const PROVIDER_KINDS = ['openai', 'anthropic', 'gemini', 'ollama'] as const

/** A provider entry's editable fields (shared by the add + edit forms). */
interface ProviderDraft {
  name: string
  kind: string
  api_key: string
  base_url: string
  model: string
}

const EMPTY_DRAFT: ProviderDraft = { name: '', kind: '', api_key: '', base_url: '', model: '' }

function ModelsSection({
  store,
  snap,
}: {
  store: SettingsStore
  snap: ReturnType<typeof useSettings>
}): ReactNode {
  const { t } = useLocale()
  const [showAdd, setShowAdd] = useState(false)
  const [draft, setDraft] = useState<ProviderDraft>(EMPTY_DRAFT)
  const [showConfig, setShowConfig] = useState(false)
  // Issue #37 — model dropdown data: fetched from the endpoint via
  // `provider/models`; manual entry stays possible (the input keeps working
  // when the list is unavailable).
  const [modelOptions, setModelOptions] = useState<string[]>([])
  const [modelsFetching, setModelsFetching] = useState(false)
  const [modelsError, setModelsError] = useState<string | null>(null)
  // Issue #41 — auto-detect hint from `provider/detect` (when kind is Auto).
  const [detected, setDetected] = useState<string | null>(null)
  // Issue #41 — inline edit state (edit form keyed by the entry name).
  const [editing, setEditing] = useState<string | null>(null)
  const [editDraft, setEditDraft] = useState<ProviderDraft>(EMPTY_DRAFT)

  /** Pull the endpoint's model list for the given kind/key/url (the draft's
   *  fields). Unset fields inherit the engine env server-side. */
  const fetchModels = async (kind: string, apiKey: string, baseUrl: string) => {
    setModelsFetching(true)
    setModelsError(null)
    const res = await store.providerModels({
      kind: kind === '' ? undefined : kind,
      api_key: apiKey.trim().length > 0 ? apiKey.trim() : undefined,
      base_url: baseUrl.trim().length > 0 ? baseUrl.trim() : undefined,
    })
    setModelsFetching(false)
    if (res.ok) {
      setModelOptions(res.models)
    } else {
      setModelOptions([])
      setModelsError(res.error ?? 'provider/models failed')
    }
  }

  /** Auto-detect the protocol from the base URL (issue #41) when kind is Auto.
   *  Best-effort — a null result just clears the hint. */
  const runDetect = async (baseUrl: string) => {
    const url = baseUrl.trim()
    if (url.length === 0) {
      setDetected(null)
      return
    }
    const res = await store.providerDetect(url)
    setDetected(res !== null && res.kind !== '' ? res.label : null)
  }

  /** Protocol kind is form-wide state — switching it invalidates the fetched
   *  model list (different protocol ⇒ different catalog). */
  const changeKind = (kind: string) => {
    setDraft({ ...draft, kind, model: '' })
    setModelOptions([])
    setModelsError(null)
    if (kind !== '') setDetected(null)
  }

  const submit = async () => {
    if (draft.name.trim().length === 0) return
    const ok = await store.providerAdd({
      name: draft.name.trim(),
      kind: draft.kind === '' ? undefined : draft.kind,
      api_key: draft.api_key.trim().length > 0 ? draft.api_key.trim() : undefined,
      base_url: draft.base_url.trim().length > 0 ? draft.base_url.trim() : undefined,
      model: draft.model.trim().length > 0 ? draft.model.trim() : undefined,
    })
    if (ok) {
      setDraft(EMPTY_DRAFT)
      setModelOptions([])
      setModelsError(null)
      setDetected(null)
      setShowAdd(false)
    }
  }

  /** Open the inline edit form, pre-filled from a card (api_key left blank —
   *  cards don't echo the plaintext key; blank = keep stored key). */
  const startEdit = (p: { name: string; kind: string; model: string; base_url?: string }) => {
    setEditing(p.name)
    setEditDraft({
      name: p.name,
      kind: p.kind,
      api_key: '',
      base_url: p.base_url ?? '',
      model: p.model,
    })
    setModelOptions([])
    setModelsError(null)
    setDetected(null)
  }

  const submitEdit = async () => {
    if (editDraft.name.trim().length === 0) return
    const ok = await store.providerUpdate({
      name: editDraft.name.trim(),
      kind: editDraft.kind === '' ? undefined : editDraft.kind,
      api_key: editDraft.api_key.trim().length > 0 ? editDraft.api_key.trim() : undefined,
      base_url: editDraft.base_url.trim().length > 0 ? editDraft.base_url.trim() : undefined,
      model: editDraft.model.trim().length > 0 ? editDraft.model.trim() : undefined,
    })
    if (ok) {
      setEditing(null)
      setEditDraft(EMPTY_DRAFT)
      setModelOptions([])
      setModelsError(null)
    }
  }

  return (
    <div className={styles.stack}>
      {snap.lastError !== null && (
        <div className={styles.errorBanner}>{snap.lastError}</div>
      )}

      {/* Config file affordance — path + copy + read-only TOML preview. */}
      {snap.configFile !== null && (
        <div className={styles.configFile}>
          <div className={styles.configFileHead}>
            <span className={styles.configFileLabel}>{t('settings.configPath')}</span>
            <div className={styles.configFileActions}>
              <button
                className={styles.miniBtn}
                onClick={() => void navigator.clipboard?.writeText(snap.configFile?.path ?? '')}
              >
                {t('settings.copyPath')}
              </button>
              <button className={styles.miniBtn} onClick={() => setShowConfig((v) => !v)}>
                {showConfig ? t('settings.hide') : t('settings.show')}
              </button>
            </div>
          </div>
          <code className={styles.configPath}>{snap.configFile.path}</code>
          {showConfig && (
            <pre className={styles.configPreview}>{snap.configFile.content}</pre>
          )}
        </div>
      )}

      {/* Provider list. */}
      {snap.providers.length === 0 && (
        <div className={styles.empty}>{t('settings.noProvider')}</div>
      )}
      {snap.providers.map((p) =>
        editing === p.name ? (
          <div key={p.name} className={styles.addForm}>
            <div className={styles.addRow}>
              <input
                className={styles.input}
                placeholder={t('settings.fldName')}
                value={editDraft.name}
                onChange={(e) => setEditDraft({ ...editDraft, name: e.target.value })}
              />
              <select
                className={styles.select}
                aria-label={t('settings.fldKind')}
                value={editDraft.kind}
                onChange={(e) => {
                  setEditDraft({ ...editDraft, kind: e.target.value, model: '' })
                  setModelOptions([])
                  setModelsError(null)
                }}
              >
                <option value="">{t('settings.kindAuto')}</option>
                {PROVIDER_KINDS.map((k) => (
                  <option key={k} value={k}>
                    {providerKindLabel(k)}
                  </option>
                ))}
              </select>
            </div>
            <input
              className={styles.input}
              placeholder={t('settings.fldApiKeyKeep')}
              value={editDraft.api_key}
              onChange={(e) => setEditDraft({ ...editDraft, api_key: e.target.value })}
            />
            <input
              className={styles.input}
              placeholder={t('settings.fldBaseUrl')}
              value={editDraft.base_url}
              onChange={(e) => setEditDraft({ ...editDraft, base_url: e.target.value })}
            />
            <div className={styles.modelRow}>
              <input
                className={styles.input}
                placeholder={t('settings.fldModel')}
                value={editDraft.model}
                list="oneai-settings-edit-model-options"
                onChange={(e) => setEditDraft({ ...editDraft, model: e.target.value })}
              />
              <button
                className={styles.miniBtn}
                disabled={modelsFetching}
                onClick={() => void fetchModels(editDraft.kind, editDraft.api_key, editDraft.base_url)}
              >
                {modelsFetching ? t('settings.fetchingModels') : t('settings.fetchModels')}
              </button>
            </div>
            <datalist id="oneai-settings-edit-model-options">
              {modelOptions.map((m) => (
                <option key={m} value={m} />
              ))}
            </datalist>
            {modelsError !== null && <div className={styles.hint}>{modelsError}</div>}
            <div className={styles.skillActions}>
              <button className={styles.miniBtn} onClick={() => void submitEdit()}>
                {t('settings.save')}
              </button>
              <button className={styles.miniBtn} onClick={() => setEditing(null)}>
                {t('settings.cancel')}
              </button>
            </div>
          </div>
        ) : (
          <div key={p.name} className={`${styles.card} ${p.active ? styles.cardActive : ''}`}>
            <div className={styles.packHead}>
              {/* name is the entry's unique key (set_active/delete operate on it
                  — issue #37); kind shows its protocol label (issue #41). */}
              <code className={styles.code}>{p.name}</code>
              <span className={styles.providerKind}>{providerKindLabel(p.kind)}</span>
              {p.active && <span className={styles.activeBadge}>{t('settings.active')}</span>}
              <span className={styles.providerModel}>{p.model || t('settings.inherited')}</span>
            </div>
            {p.base_url !== undefined && p.base_url !== '' && (
              <div className={styles.packDesc}>{p.base_url}</div>
            )}
            <div className={styles.skillActions}>
              <button
                className={styles.miniBtn}
                onClick={() => void store.providerSetActive(p.name)}
                disabled={p.active}
              >
                {t('settings.setActive')}
              </button>
              <button className={styles.miniBtn} onClick={() => startEdit(p)}>
                {t('settings.edit')}
              </button>
              <button
                className={`${styles.miniBtn} ${styles.danger}`}
                onClick={() => void store.providerDelete(p.name)}
                disabled={snap.providers.length <= 1}
              >
                {t('settings.delete')}
              </button>
            </div>
          </div>
        ),
      )}

      {/* Add provider form. */}
      {showAdd ? (
        <div className={styles.addForm}>
          <div className={styles.addRow}>
            <input
              className={styles.input}
              placeholder={t('settings.fldName')}
              value={draft.name}
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            />
            {/* Protocol kind: Auto (from URL) by default, explicit override
                optional (issue #41). */}
            <select
              className={styles.select}
              aria-label={t('settings.fldKind')}
              value={draft.kind}
              onChange={(e) => changeKind(e.target.value)}
            >
              <option value="">{t('settings.kindAuto')}</option>
              {PROVIDER_KINDS.map((k) => (
                <option key={k} value={k}>
                  {providerKindLabel(k)}
                </option>
              ))}
            </select>
          </div>
          <input
            className={styles.input}
            placeholder={t('settings.fldApiKey')}
            value={draft.api_key}
            onChange={(e) => setDraft({ ...draft, api_key: e.target.value })}
          />
          <input
            className={styles.input}
            placeholder={t('settings.fldBaseUrl')}
            value={draft.base_url}
            onChange={(e) => setDraft({ ...draft, base_url: e.target.value })}
            onBlur={(e) => {
              if (draft.kind === '') void runDetect(e.target.value)
            }}
          />
          {detected !== null && <div className={styles.hint}>{detected}</div>}
          {/* Model name: pick from the endpoint's fetched catalog (datalist
              keeps manual entry as a fallback — issue #37). */}
          <div className={styles.modelRow}>
            <input
              className={styles.input}
              placeholder={t('settings.fldModel')}
              value={draft.model}
              list="oneai-settings-model-options"
              onChange={(e) => setDraft({ ...draft, model: e.target.value })}
            />
            <button
              className={styles.miniBtn}
              disabled={modelsFetching}
              onClick={() => void fetchModels(draft.kind, draft.api_key, draft.base_url)}
            >
              {modelsFetching ? t('settings.fetchingModels') : t('settings.fetchModels')}
            </button>
          </div>
          <datalist id="oneai-settings-model-options">
            {modelOptions.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
          {modelsError !== null && <div className={styles.hint}>{modelsError}</div>}
          <div className={styles.skillActions}>
            <button className={styles.miniBtn} onClick={() => void submit()}>
              {t('settings.save')}
            </button>
            <button className={styles.miniBtn} onClick={() => setShowAdd(false)}>
              {t('settings.cancel')}
            </button>
          </div>
        </div>
      ) : (
        <button className={styles.addToggle} onClick={() => setShowAdd(true)}>
          + {t('settings.addProvider')}
        </button>
      )}
    </div>
  )
}

function DomainSection({
  store,
  snap,
}: {
  store: SettingsStore
  snap: ReturnType<typeof useSettings>
}): ReactNode {
  const { t } = useLocale()
  const [switching, setSwitching] = useState<string | null>(null)
  const dp = snap.domainPacks

  const switchTo = async (name: string) => {
    if (name === dp?.active) return
    setSwitching(name)
    try {
      await store.domainpackSwitch(name)
    } finally {
      setSwitching(null)
    }
  }

  // A stale app-server (method-not-found on `domainpack/list`) or an offline
  // probe shows the real error rather than a generic banner.
  if (dp === null) {
    return (
      <div className={styles.empty}>
        {snap.lastError !== null ? snap.lastError : t('settings.offline')}
      </div>
    )
  }

  const active = dp.active ?? ''
  const current = dp.available.find((p) => p.name === active)

  return (
    <div className={styles.stack}>
      <Row label={t('settings.activeDomain')}>
        <select
          className={styles.select}
          aria-label={t('settings.activeDomain')}
          value={active}
          disabled={switching !== null}
          onChange={(e) => {
            if (e.target.value) void switchTo(e.target.value)
          }}
        >
          {dp.available.map((p) => (
            <option key={p.name} value={p.name}>
              {p.name}
            </option>
          ))}
        </select>
        {switching !== null && (
          <span className={styles.hint}>{t('settings.switching')}</span>
        )}
      </Row>
      {current?.description !== undefined && (
        <div className={styles.packDesc}>{current.description}</div>
      )}
      <Row label={t('settings.permissionProfile')}>
        <code className={styles.code}>
          {snap.config?.permission_profile ?? t('settings.none')}
        </code>
      </Row>
      <div className={styles.hint}>{t('settings.restartHintDomain')}</div>
    </div>
  )
}

// ── Network (§B5 durable host allow/deny list) ───────────────────────────────
// Lists the hosts the user admitted/denied cross-session + per-row revoke.
// A host admitted via the NetworkApproval panel's "Always" lands here; the
// engine `NetworkProxy` consults the same durable `~/.oneai/oneai.db` table on
// every CONNECT, so removing a row here makes the next connection re-prompt.

function NetworkSection({ hostOps }: { hostOps: HostOps }): ReactNode {
  const { t } = useLocale()
  const [list, setList] = useState<HostListResult>({ allowed: [], denied: [] })
  const [loaded, setLoaded] = useState(false)

  const refresh = async () => {
    const r = await hostOps.list()
    setList(r)
    setLoaded(true)
  }

  useEffect(() => {
    void refresh()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // Optimistic remove: drop the row locally, then fire the RPC. The list is
  // re-fetched on error so a failed delete resurfaces the row.
  const remove = async (host: string, denied: boolean) => {
    const prev = list
    setList((cur) =>
      denied
        ? { ...cur, denied: cur.denied.filter((e) => e.host !== host) }
        : { ...cur, allowed: cur.allowed.filter((e) => e.host !== host) },
    )
    try {
      if (denied) await hostOps.removeDenied(host)
      else await hostOps.remove(host)
    } catch {
      setList(prev) // restore on failure
    }
  }

  return (
    <div className={styles.stack}>
      <div className={styles.hint}>{t('settings.networkHint')}</div>

      <div className={styles.configFileLabel}>{t('settings.networkAllowed')}</div>
      {!loaded ? (
        <div className={styles.empty}>{t('settings.networkLoading')}</div>
      ) : list.allowed.length === 0 ? (
        <div className={styles.empty}>{t('settings.networkEmpty')}</div>
      ) : (
        list.allowed.map((e) => (
          <div key={`a-${e.host}`} className={styles.card}>
            <code className={styles.code}>{e.host}</code>
            <button className={`${styles.miniBtn} ${styles.danger}`} onClick={() => void remove(e.host, false)}>
              {t('settings.delete')}
            </button>
          </div>
        ))
      )}

      <div className={styles.configFileLabel}>{t('settings.networkDenied')}</div>
      {loaded && list.denied.length === 0 ? (
        <div className={styles.empty}>{t('settings.networkEmpty')}</div>
      ) : (
        list.denied.map((e) => (
          <div key={`d-${e.host}`} className={styles.card}>
            <code className={styles.code}>{e.host}</code>
            <button className={`${styles.miniBtn} ${styles.danger}`} onClick={() => void remove(e.host, true)}>
              {t('settings.delete')}
            </button>
          </div>
        ))
      )}
    </div>
  )
}
