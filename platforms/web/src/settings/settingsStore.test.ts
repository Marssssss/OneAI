import { describe, expect, it } from 'vitest'
import { SettingsStore } from './settingsStore'
import type { OneAiRpcClient } from '../rpc/client'
import type { AppConfigSnapshot } from '../rpc/types'

// Verifies SettingsStore's `thinking/set` RPC call: right method + params,
// optimistic patch of the snapshot's `thinking_effort`, server-result
// confirmation, and rollback on RPC failure.

interface FakeRpc {
  calls: { method: string; params: unknown }[]
  // Per-method canned results; `thinking/set` mutates `effort` so the
  // "confirm" path sees the new tier.
  effort: string
  failSet: boolean
}

function fakeRpc(): FakeRpc & OneAiRpcClient {
  const self: FakeRpc & Partial<OneAiRpcClient> = {
    calls: [],
    effort: 'medium',
    failSet: false,
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
    // provider/list · domainpack/list · skill/list · config/read → empties.
    if (method === 'provider/list') return Promise.resolve({ providers: [] } as never)
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
