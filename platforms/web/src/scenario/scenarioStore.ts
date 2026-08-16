// ScenarioListStore — the sidebar's scenario library, an external store over
// the synchronous `scenario/*` JSON-RPC methods. Mirrors macOS `AgentStore`
// merge rule: `scenario/list` returns the server store; we drop the server's
// `preset-*` seeds (they're a minimal starter; the per-frontend local preset
// set is richer and locale-bound) and prepend OUR local presets so the
// sidebar shows localPresets + serverCustoms. Customs (non-`preset-` ids) are
// the shared library — edits go to the server via `scenario/upsert`/`delete`.

import { useSyncExternalStore } from 'react'
import type { OneAiRpcClient } from '../rpc/client'
import type {
  BusScenario,
  ScenarioError,
  ScenarioUpsertResult,
  ScenarioValidateResult,
} from '../rpc/types'
import { presetsFor } from './presets'

export interface ScenarioEntry {
  scenario: BusScenario
  /** `preset-*` ids are local-only (read-only in the editor). */
  isPreset: boolean
}

export class ScenarioListStore {
  private rpc: OneAiRpcClient
  private entries: ScenarioEntry[] = []
  private listeners = new Set<() => void>()

  constructor(rpc: OneAiRpcClient) {
    this.rpc = rpc
  }

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn)
    return () => this.listeners.delete(fn)
  }
  getSnapshot = (): ScenarioEntry[] => this.entries

  /** Refresh from the server. Local presets (locale-bound) are prepended;
   *  the server's `preset-*` seeds are dropped so the local set wins. */
  async refresh(locale: 'zh' | 'en'): Promise<void> {
    let customs: BusScenario[] = []
    try {
      const res = await this.rpc.call<unknown, { scenarios: BusScenario[] }>(
        'scenario/list',
        {},
      )
      customs = (res.scenarios ?? []).filter(
        (s) => !s.id.startsWith('preset-'),
      )
    } catch {
      /* offline — keep the last list */
    }
    const presets = presetsFor(locale).map((s) => ({
      scenario: s,
      isPreset: true,
    }))
    const customEntries = customs.map((s) => ({ scenario: s, isPreset: false }))
    this.entries = [...presets, ...customEntries]
    for (const l of this.listeners) l()
  }

  /** Validate a scenario (live editor feedback). Returns the raw errors. */
  async validate(
    scenario: BusScenario,
  ): Promise<ScenarioError[]> {
    try {
      const res = await this.rpc.call<{ scenario: BusScenario }, ScenarioValidateResult>(
        'scenario/validate',
        { scenario },
      )
      return res.errors ?? []
    } catch {
      return []
    }
  }

  /** Upsert (the server re-validates; on `ok:false` returns the errors).
   *  Presets are local-only — caller must guard against upserting them. */
  async upsert(
    scenario: BusScenario,
  ): Promise<{ ok: true; id: string } | { ok: false; errors: ScenarioError[] }> {
    try {
      const res = await this.rpc.call<{ scenario: BusScenario }, ScenarioUpsertResult>(
        'scenario/upsert',
        { scenario },
      )
      if (res.ok && res.id !== undefined) return { ok: true, id: res.id }
      return { ok: false, errors: res.errors ?? [] }
    } catch (e) {
      return { ok: false, errors: [{ field: '', code: 'invalid', message: e instanceof Error ? e.message : String(e) }] }
    }
  }

  /** Delete by id (no-op server-side if absent). */
  async delete(id: string): Promise<void> {
    try {
      await this.rpc.call<{ id: string }, { ok: boolean }>('scenario/delete', {
        id,
      })
    } catch {
      /* ignore */
    }
  }
}

export function useScenarioList(store: ScenarioListStore): ScenarioEntry[] {
  return useSyncExternalStore(store.subscribe, store.getSnapshot, () => [])
}
