// SettingsStore — the settings modal's external store over the W4 app-probe
// JSON-RPC methods (`config/get` · `provider/list` · `domainpack/list` ·
// `skill/list` + the skill lifecycle ops `skill/{pin|unpin|archive|restore}`).
//
// Pure consumers of the probe RPCs (no EngineYield streaming). Mirrors
// `ScenarioListStore`'s external-store pattern: `useSyncExternalStore`,
// `refresh()` pulls the four read-only snapshots, the skill ops do optimistic
// local mutation + a server round-trip then re-sync from the op result.

import { useSyncExternalStore } from 'react'
import type { OneAiRpcClient } from '../rpc/client'
import type {
  AppConfigSnapshot,
  ConfigFileView,
  DomainPackList,
  DomainPackOpResult,
  ProviderDetectParams,
  ProviderDetectResult,
  ProviderEntryDto,
  ProviderInfo,
  ProviderModelsParams,
  ProviderModelsResult,
  ProviderOpResult,
  ProviderSetModelParams,
  SkillInfo,
  SkillOpParams,
  SkillOpResult,
  ThinkingEffort,
  ThinkingEffortSetParams,
} from '../rpc/types'

/** Protocol display labels for wire `kind` strings (issue #41). The wire
 *  `kind` values are unchanged — these are display-only protocol names, not
 *  vendor names. */
export const PROVIDER_LABELS: Record<string, string> = {
  openai: 'OpenAI Completions',
  anthropic: 'Anthropic Messages',
  gemini: 'Gemini Protocol',
  ollama: 'Ollama Protocol',
}

/** Human protocol label for a `kind` string (falls back to the raw kind). */
export function providerKindLabel(kind: string): string {
  return PROVIDER_LABELS[kind] ?? kind
}

export interface SettingsSnapshot {
  config: AppConfigSnapshot | null
  providers: ProviderInfo[]
  domainPacks: DomainPackList | null
  skills: SkillInfo[]
  /** Raw config file (path + content) for the "open config file" affordance. */
  configFile: ConfigFileView | null
  /** Bumped on every refresh so callers can detect staleness. */
  version: number
  /** Last error surfaced by a config/provider read or op (NOT silently
   *  swallowed — surfaces the real RPC failure, e.g. "method not found:
   *  config/get" so a stale binary is diagnosable). */
  lastError: string | null
}

const EMPTY: SettingsSnapshot = {
  config: null,
  providers: [],
  domainPacks: null,
  skills: [],
  configFile: null,
  version: 0,
  lastError: null,
}

export class SettingsStore {
  private rpc: OneAiRpcClient
  private state: SettingsSnapshot = EMPTY
  private listeners = new Set<() => void>()

  constructor(rpc: OneAiRpcClient) {
    this.rpc = rpc
  }

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn)
    return () => this.listeners.delete(fn)
  }
  getSnapshot = (): SettingsSnapshot => this.state

  private set(patch: Partial<SettingsSnapshot>): void {
    this.state = { ...this.state, ...patch, version: this.state.version + 1 }
    for (const l of this.listeners) l()
  }

  /** Pull the read-only snapshots in parallel. Each is independent — a
   *  failure (e.g. a stale binary returning method-not-found) is captured
   *  into `lastError` rather than silently swallowed, so the UI can show the
   *  real cause (e.g. "method not found: config/get → rebuild app-server"). */
  async refresh(): Promise<void> {
    const [config, providers, domainPacks, skills, configFile] = await Promise.all([
      this.rpc.call<unknown, AppConfigSnapshot>('config/get', {}).catch((e) => {
        this.set({ lastError: errMsg(e, 'config/get') })
        return null
      }),
      this.rpc
        .call<unknown, { providers: ProviderInfo[] }>('provider/list', {})
        .then((r) => r.providers ?? [])
        .catch((e) => {
          this.set({ lastError: errMsg(e, 'provider/list') })
          return [] as ProviderInfo[]
        }),
      this.rpc.call<unknown, DomainPackList>('domainpack/list', {}).catch(() => null),
      this.rpc
        .call<unknown, { skills: SkillInfo[] }>('skill/list', {})
        .then((r) => r.skills ?? [])
        .catch(() => [] as SkillInfo[]),
      this.rpc.call<unknown, ConfigFileView>('config/read', {}).catch(() => null),
    ])
    this.set({ config, providers, domainPacks, skills, configFile })
  }

  /** Hot-swap the active DomainPack by name — takes effect on the next turn and
   *  persists to config.toml (launch default). Updates the local list from the
   *  op result (mirror of `providerSetActive`). */
  async domainpackSwitch(name: string): Promise<boolean> {
    try {
      const res = await this.rpc.call<{ name: string }, DomainPackOpResult>(
        'domainpack/switch',
        { name },
      )
      if (res.ok && res.available !== undefined) {
        this.set({
          domainPacks: { active: res.active ?? name, available: res.available },
          config: this.state.config
            ? { ...this.state.config, domain_pack: res.active ?? name }
            : this.state.config,
          lastError: null,
        })
        return true
      }
      this.set({ lastError: res.error ?? 'domain switch failed' })
      return false
    } catch (e) {
      this.set({ lastError: errMsg(e, 'domainpack/switch') })
      return false
    }
  }

  /** Add a provider — writes to config.toml + adds it live (immediately
   *  switchable). Updates the local list from the op result. */
  async providerAdd(entry: ProviderEntryDto): Promise<boolean> {
    try {
      const res = await this.rpc.call<{ entry: ProviderEntryDto }, ProviderOpResult>(
        'provider/add',
        { entry },
      )
      if (res.ok && res.providers !== undefined) {
        this.set({ providers: res.providers, lastError: null })
        return true
      }
      this.set({ lastError: res.error ?? 'provider add failed' })
      return false
    } catch (e) {
      this.set({ lastError: errMsg(e, 'provider/add') })
      return false
    }
  }

  /** Delete a provider — writes to config.toml + removes it live. */
  async providerDelete(name: string): Promise<boolean> {
    try {
      const res = await this.rpc.call<SkillOpParams, ProviderOpResult>('provider/delete', {
        name,
      })
      if (res.ok && res.providers !== undefined) {
        this.set({ providers: res.providers, lastError: null })
        return true
      }
      this.set({ lastError: res.error ?? 'provider delete failed' })
      return false
    } catch (e) {
      this.set({ lastError: errMsg(e, 'provider/delete') })
      return false
    }
  }

  /** Update a provider entry by name — writes to config.toml + rebuilds the
   *  live pool entry (preserving priority/active). `api_key` left undefined
   *  retains the stored key (cards don't echo the plaintext key). */
  async providerUpdate(entry: ProviderEntryDto): Promise<boolean> {
    try {
      const res = await this.rpc.call<{ entry: ProviderEntryDto }, ProviderOpResult>(
        'provider/update',
        { entry },
      )
      if (res.ok && res.providers !== undefined) {
        this.set({ providers: res.providers, lastError: null })
        return true
      }
      this.set({ lastError: res.error ?? 'provider update failed' })
      return false
    } catch (e) {
      this.set({ lastError: errMsg(e, 'provider/update') })
      return false
    }
  }

  /** Set the model of one provider entry (issue #41 — switch model under the
   *  same provider). Persisted + hot-swapped; the server returns the post-op
   *  provider list so the active-model display updates. */
  async providerSetModel(name: string, model: string): Promise<boolean> {
    try {
      const res = await this.rpc.call<ProviderSetModelParams, ProviderOpResult>(
        'provider/set_model',
        { name, model },
      )
      if (res.ok && res.providers !== undefined) {
        this.set({ providers: res.providers, lastError: null })
        return true
      }
      this.set({ lastError: res.error ?? 'provider set-model failed' })
      return false
    } catch (e) {
      this.set({ lastError: errMsg(e, 'provider/set_model') })
      return false
    }
  }

  /** Auto-detect the protocol + normalized base URL from a bare base URL
   *  (issue #41). No API key required. Returns null on any failure (the UI
   *  just shows no detection hint). */
  async providerDetect(baseUrl: string): Promise<ProviderDetectResult | null> {
    try {
      return await this.rpc.call<ProviderDetectParams, ProviderDetectResult>(
        'provider/detect',
        { base_url: baseUrl },
      )
    } catch {
      return null
    }
  }

  /** Live-switch the active provider (atomic pool active_index). Optimistically
   *  flips the active marker; the server result confirms. Addressed by the
   *  entry's unique `name`, never its `kind` (issue #37 — two entries may
   *  share a kind, and the pool matches on name). */
  async providerSetActive(name: string): Promise<boolean> {
    this.set({
      providers: this.state.providers.map((p) => ({ ...p, active: p.name === name })),
      lastError: null,
    })
    try {
      const res = await this.rpc.call<SkillOpParams, ProviderOpResult>(
        'provider/set_active',
        { name },
      )
      if (res.ok && res.providers !== undefined) {
        this.set({ providers: res.providers, lastError: null })
        return true
      }
      this.set({ lastError: res.error ?? 'provider set-active failed' })
      await this.refresh()
      return false
    } catch (e) {
      this.set({ lastError: errMsg(e, 'provider/set_active') })
      await this.refresh()
      return false
    }
  }

  /** Fetch the models an endpoint serves (issue #37 — feeds the add-provider
   *  form's model dropdown). Returns the list, or `{models:[], error}` on any
   *  failure (the UI keeps manual entry available). */
  async providerModels(params: ProviderModelsParams): Promise<ProviderModelsResult> {
    try {
      return await this.rpc.call<ProviderModelsParams, ProviderModelsResult>(
        'provider/models',
        params,
      )
    } catch (e) {
      return { ok: false, models: [], error: errMsg(e, 'provider/models') }
    }
  }

  /** Persist a new thinking-effort tier (web UI "思考程度" toggle). Hot-
   *  swaps immediately — the next turn's main agent + new sub-agents read the
   *  new value. Optimistically patches the snapshot's `thinking_effort` so the
   *  control reacts instantly; the server's `{effort}` result confirms (and
   *  re-syncs on failure). */
  async setThinkingEffort(effort: ThinkingEffort): Promise<boolean> {
    const prev = this.state.config?.thinking_effort
    if (this.state.config) {
      this.set({
        config: { ...this.state.config, thinking_effort: effort },
        lastError: null,
      })
    }
    try {
      const res = await this.rpc.call<ThinkingEffortSetParams, { effort: ThinkingEffort }>(
        'thinking/set',
        { effort },
      )
      if (this.state.config) {
        this.set({ config: { ...this.state.config, thinking_effort: res.effort }, lastError: null })
      }
      return true
    } catch (e) {
      // Roll back the optimistic patch to the server's persisted value.
      if (this.state.config) {
        this.set({
          config: { ...this.state.config, thinking_effort: prev },
          lastError: errMsg(e, 'thinking/set'),
        })
      }
      return false
    }
  }

  /** Run a skill lifecycle op (pin/unpin/archive/restore). Optimistically
   *  applies the op's returned `SkillInfo` to the local list; on failure
   *  surfaces the error. The server is the authority — a refresh re-syncs. */
  async skillOp(
    method: 'skill/pin' | 'skill/unpin' | 'skill/archive' | 'skill/restore',
    name: string,
  ): Promise<void> {
    // Optimistic: optimistically hide/move the row based on the op kind so the
    // UI reacts instantly; the server response confirms.
    if (method === 'skill/archive') {
      this.set({
        skills: this.state.skills.filter((s) => s.name !== name),
        lastError: null,
      })
    } else {
      this.set({
        skills: this.state.skills.map((s) =>
          s.name === name
            ? { ...s, pinned: method === 'skill/pin' ? true : method === 'skill/unpin' ? false : s.pinned }
            : s,
        ),
        lastError: null,
      })
    }
    try {
      const res = await this.rpc.call<SkillOpParams, SkillOpResult>(method, { name })
      if (res.ok && res.skill !== undefined) {
        // Replace the row with the post-op state (or re-add if archived→active).
        const next = this.state.skills.filter((s) => s.name !== name)
        this.set({ skills: [...next, res.skill] })
      } else if (!res.ok) {
        this.set({ lastError: res.error ?? 'skill op failed' })
        // Roll back by re-listing.
        const r = await this.rpc
          .call<unknown, { skills: SkillInfo[] }>('skill/list', {})
          .then((x) => x.skills ?? [])
          .catch(() => this.state.skills)
        this.set({ skills: r })
      }
    } catch (e) {
      this.set({ lastError: e instanceof Error ? e.message : String(e) })
      await this.refresh()
    }
  }
}

export function useSettings(store: SettingsStore): SettingsSnapshot {
  return useSyncExternalStore(store.subscribe, store.getSnapshot, () => EMPTY)
}

/** Render an RPC error as `"<method>: <message>"` so the UI shows WHICH call
 *  failed (e.g. a stale app-server binary → "method not found: config/get"). */
function errMsg(e: unknown, method: string): string {
  const msg = e instanceof Error ? e.message : String(e)
  return `${method}: ${msg}`
}
