import { describe, expect, it } from 'vitest'
import { ProjectionStore } from './projection'
import type { OneAiRpcClient } from '../rpc/client'

// §B5 — verifies ProjectionStore's host/* RPC methods call the right JSON-RPC
// method with the right params, return the `host/list` shape, and swallow
// network errors (the ApprovalPanel "Always" button must still proceed when
// the app-server is unreachable).

interface FakeRpc {
  calls: { method: string; params: unknown }[]
  nextResult: unknown
  fail: boolean
}

// `call` reads `self.nextResult` / `self.fail` off the same object the test
// mutates, so reassigning `rpc.nextResult` after construction is visible to
// `call` (Object.assign-based copies would break this).
function fakeRpc(): FakeRpc & OneAiRpcClient {
  const self: FakeRpc & Partial<OneAiRpcClient> = {
    calls: [],
    nextResult: { allowed: [], denied: [] },
    fail: false,
  }
  self.onEvent = () => () => {}
  self.onStatus = () => () => {}
  self.getStatus = () => 'closed' as const
  self.call = (method: string, params: unknown) => {
    self.calls.push({ method, params })
    if (self.fail) return Promise.reject(new Error('offline'))
    return Promise.resolve(self.nextResult as never)
  }
  return self as FakeRpc & OneAiRpcClient
}

describe('ProjectionStore host/* RPC (B5)', () => {
  it('admitHost calls host/allow with {host}', async () => {
    const rpc = fakeRpc()
    const store = new ProjectionStore(rpc)
    await store.admitHost('api.example.com')
    expect(rpc.calls).toEqual([{ method: 'host/allow', params: { host: 'api.example.com' } }])
  })

  it('denyHost calls host/deny', async () => {
    const rpc = fakeRpc()
    const store = new ProjectionStore(rpc)
    await store.denyHost('evil.example')
    expect(rpc.calls[0]).toEqual({ method: 'host/deny', params: { host: 'evil.example' } })
  })

  it('listHosts returns {allowed,denied} and calls host/list', async () => {
    const rpc = fakeRpc()
    rpc.nextResult = {
      allowed: [{ host: 'a.example', recorded_at_ms: 1000 }],
      denied: [{ host: 'b.example', recorded_at_ms: 2000 }],
    }
    const store = new ProjectionStore(rpc)
    const r = await store.listHosts()
    expect(rpc.calls[0].method).toBe('host/list')
    expect(r.allowed).toHaveLength(1)
    expect(r.allowed[0].host).toBe('a.example')
    expect(r.denied).toHaveLength(1)
    expect(r.denied[0].host).toBe('b.example')
  })

  it('removeHost / removeDeniedHost call the right method', async () => {
    const rpc = fakeRpc()
    const store = new ProjectionStore(rpc)
    await store.removeHost('a.example')
    await store.removeDeniedHost('b.example')
    expect(rpc.calls[0]).toEqual({ method: 'host/remove', params: { host: 'a.example' } })
    expect(rpc.calls[1]).toEqual({ method: 'host/remove-denied', params: { host: 'b.example' } })
  })

  it('admitHost swallows network errors (resolves undefined, no throw)', async () => {
    const rpc = fakeRpc()
    rpc.fail = true
    const store = new ProjectionStore(rpc)
    await expect(store.admitHost('a.example')).resolves.toBeUndefined()
  })

  it('listHosts returns empty on failure (no throw)', async () => {
    const rpc = fakeRpc()
    rpc.fail = true
    const store = new ProjectionStore(rpc)
    const r = await store.listHosts()
    expect(r).toEqual({ allowed: [], denied: [] })
  })
})
