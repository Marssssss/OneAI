import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { StreamCoalescer } from './coalescer'

// The coalescer batches hot updates to ≤20fps (50ms windows) and flushes
// immediately on terminal/non-hot signals. These tests pin both policies.

describe('StreamCoalescer', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })
  afterEach(() => {
    vi.useRealTimers()
  })

  it('request() flushes at most once per ~50ms window', () => {
    const flush = vi.fn()
    const c = new StreamCoalescer(flush)

    // Fire 100 hot chunks within a single frame window.
    for (let i = 0; i < 100; i++) c.request()

    // Nothing flushed synchronously — the drain is deferred.
    expect(flush).not.toHaveBeenCalled()

    // Advance past the 50ms wait + the rAF tick. Exactly one coalesced flush.
    vi.advanceTimersByTime(60)
    expect(flush).toHaveBeenCalledTimes(1)

    // A second window after another burst.
    for (let i = 0; i < 50; i++) c.request()
    vi.advanceTimersByTime(60)
    expect(flush).toHaveBeenCalledTimes(2)
  })

  it('flushNow() flushes immediately and cancels a pending coalesced drain', () => {
    const flush = vi.fn()
    const c = new StreamCoalescer(flush)

    c.request() // schedules a coalesced drain
    expect(flush).not.toHaveBeenCalled()

    c.flushNow() // terminal signal — flush now, cancel the pending drain
    expect(flush).toHaveBeenCalledTimes(1)

    // Advancing time must NOT fire a second (coalesced) flush.
    vi.advanceTimersByTime(80)
    expect(flush).toHaveBeenCalledTimes(1)
  })

  it('flushNow() always flushes even if no hot request preceded it', () => {
    // Non-hot mutations (pushUserMessage / session_loaded) rely on flushNow
    // being unconditional — gating on a dirty flag would silently drop them.
    const flush = vi.fn()
    const c = new StreamCoalescer(flush)

    c.flushNow()
    expect(flush).toHaveBeenCalledTimes(1)
  })

  it('a flush error does not kill the coalescer', () => {
    const flush = vi.fn(() => {
      throw new Error('boom')
    })
    const c = new StreamCoalescer(flush)

    expect(() => c.flushNow()).not.toThrow()
    // A subsequent request still schedules without throwing.
    expect(() => c.request()).not.toThrow()
  })
})
