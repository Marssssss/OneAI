import { describe, expect, it } from 'vitest'
import { SettingsStore } from './settingsStore'
import type { OneAiRpcClient } from '../rpc/client'
import type { AppConfigSnapshot, ProviderInfo } from '../rpc/types'

// Verifies SettingsStore's `thinking/set` RPC call: right method + params,
// optimistic patch of the snapshot's `thinking_effort`, server-result
// confirmation, and rollback on RPC failure.

interface FakeRpc {
  calls: { method: string; params: unknown }[]
  // Per-method canned results; `thinking/set` mutates `effort` so the
  // "confirm" path sees the new tier.
  effort: string
  failSet: boolean
  // Issue #37 — provider list + model-listing fixtures.
  providers: ProviderInfo[]
  models: string[]
  failModels: boolean
}

function fakeRpc(): FakeRpc & OneAiRpcClient {
  const self: FakeRpc & Partial<OneAiRpcClient> = {
    calls: [],
    effort: 'medium',
    failSet: false,
    providers: [],
    models: ['gpt-4o', 'gpt-4o-mini'],
    failModels: false,
  }
  self.onEvent = () => () => {}
  self.onStatus = () => () => {}
  self.getStatus = () => 'closed' as const
  self.call = (method: string, params: unknown) => {
    self.calls.push({ method, params })
    if (method === 'config/get') {
      const snap: AppConfigSnapshot = { plan_mode: false, thinking_effort: self.effort }
      return Promise.resolve(snap as never)
    }
    if (method === 'thinking/set') {
      if (self.failSet) return Promise.reject(new Error('offline'))
      // The server re-reads the persisted value; echo it back.
      self.effort = (params as { effort: string }).effort
      return Promise.resolve({ effort: self.effort } as never)
    }
    if (method === 'provider/list') {
      return Promise.resolve({ providers: self.providers } as never)
    }
    if (method === 'provider/set_active') {
      // Mirror the pool: mark the entry whose NAME matches active.
      const name = (params as { name: string }).name
      self.providers = self.providers.map((p) => ({ ...p, active: p.name === name }))
      return Promise.resolve({ ok: true, providers: self.providers } as never)
    }
    if (method === 'provider/models') {
      if (self.failModels) return Promise.reject(new Error('offline'))
      return Promise.resolve({ ok: true, models: self.models } as never)
    }
    // domainpack/list · skill/list · config/read → empties.
    if (method === 'skill/list') return Promise.resolve({ skills: [] } as never)
    return Promise.resolve(null as never)
  }
  return self as FakeRpc & OneAiRpcClient
}

describe('SettingsStore thinking effort RPC', () => {
  it('setThinkingEffort calls thinking/set with {effort}', async () => {
    const rpc = fakeRpc()
    const store = new SettingsStore(rpc)
    await store.refresh()
    await store.setThinkingEffort('high')
    const setCall = rpc.calls.find((c) => c.method === 'thinking/set')
    expect(setCall).toEqual({ method: 'thinking/set', params: { effort: 'high' } })
  })

  it('setThinkingEffort confirms the server-returned tier', async () => {
    const rpc = fakeRpc()
    const store = new SettingsStore(rpc)
    await store.refresh()
    expect(store.getSnapshot().config?.thinking_effort).toBe('medium')
    const ok = await store.setThinkingEffort('high')
    expect(ok).toBe(true)
    expect(store.getSnapshot().config?.thinking_effort).toBe('high')
  })

  it('setThinkingEffort rolls back on RPC failure', async () => {
    const rpc = fakeRpc()
    rpc.failSet = true
    const store = new SettingsStore(rpc)
    await store.refresh()
    const ok = await store.setThinkingEffort('high')
    expect(ok).toBe(false)
    // Rolled back to the prior persisted value (medium), not the optimistic
    // 'high' patch.
    expect(store.getSnapshot().config?.thinking_effort).toBe('medium')
    expect(store.getSnapshot().lastError).not.toBeNull()
  })
})

// Issue #37 — providers are addressed by their unique `name` (never `kind`):
// two entries may share a kind, and the pool's `set_active_by_name` matches
// on name. Also covers the `provider/models` RPC feeding the model dropdown.
describe('SettingsStore provider ops (issue #37)', () => {
  const TWO_OPENAI = (): ProviderInfo[] => [
    { name: 'openai-prod', kind: 'openai', model: 'gpt-4o', active: true },
    { name: 'openai-backup', kind: 'openai', model: 'gpt-4o-mini', active: false },
  ]

  it('providerSetActive addresses the entry by name, not kind', async () => {
    const rpc = fakeRpc()
    rpc.providers = TWO_OPENAI()
    const store = new SettingsStore(rpc)
    await store.refresh()

    const ok = await store.providerSetActive('openai-backup')
    expect(ok).toBe(true)

    // The RPC got the entry NAME (passing the kind "openai" was the #37 bug —
    // the pool answered "unknown provider: openai").
    const call = rpc.calls.find((c) => c.method === 'provider/set_active')
    expect(call?.params).toEqual({ name: 'openai-backup' })

    // Exactly the named entry flipped active — the same-kind sibling didn't.
    const byName = store.getSnapshot().providers
    expect(byName.find((p) => p.name === 'openai-backup')?.active).toBe(true)
    expect(byName.find((p) => p.name === 'openai-prod')?.active).toBe(false)
  })

  it('providerModels returns the fetched list', async () => {
    const rpc = fakeRpc()
    const store = new SettingsStore(rpc)
    const res = await store.providerModels({ kind: 'openai', api_key: 'sk' })
    expect(res.ok).toBe(true)
    expect(res.models).toEqual(['gpt-4o', 'gpt-4o-mini'])
    const call = rpc.calls.find((c) => c.method === 'provider/models')
    expect(call?.params).toEqual({ kind: 'openai', api_key: 'sk' })
  })

  it('providerModels surfaces failure as ok:false + error', async () => {
    const rpc = fakeRpc()
    rpc.failModels = true
    const store = new SettingsStore(rpc)
    const res = await store.providerModels({ kind: 'openai' })
    expect(res.ok).toBe(false)
    expect(res.models).toEqual([])
    expect(res.error).toContain('provider/models')
  })
})
