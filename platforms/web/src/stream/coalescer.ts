// StreamCoalescer — batches "hot" visual updates to ≤20fps so per-token
// mutations don't flood React's reconciler, while flushing immediately on
// terminal signals (complete/error) and non-hot mutations (user message,
// session_loaded).
//
// Mirrors the macOS SwiftUI `StreamCoalescer` (~20fps, immediate flush on
// `.complete`/`.error`). The key invariant: the underlying mutable state is
// updated the moment a chunk arrives (so a synchronous read always sees the
// latest text), but the *notification* to React is coalesced — React only
// re-renders once per frame window at most.
//
// Two methods, two policies:
//  - `request()`  — hot path (stream_chunk/thinking). Dedupes within a
//    pending flush; schedules a drain on a ~20fps cadence landing in a rAF.
//  - `flushNow()` — terminal / non-hot path. Unconditionally flushes and
//    cancels any pending coalesced drain. MUST always flush (it's the path
//    pushUserMessage / turn_complete / session_loaded rely on) — gating it on
//    a `dirty` flag set only by `request()` would silently no-op them.
//
// This lives entirely in the frontend. The app-server never coalesces its
// outbound `event` stream — that would break the single-message bus semantics.

const FLUSH_INTERVAL_MS = 1000 / 20 // 20fps

export class StreamCoalescer {
  private scheduled = false
  private lastFlush = 0
  private flush: () => void

  constructor(flush: () => void) {
    this.flush = flush
  }

  /**
   * Mark a hot update pending and schedule a coalesced drain at ≤20fps. Safe to
   * call per token — only the first call after a flush schedules; subsequent
   * calls dedupe until the drain fires.
   */
  request(): void {
    if (this.scheduled) return
    this.scheduled = true
    const elapsed = Date.now() - this.lastFlush
    const wait = Math.max(0, FLUSH_INTERVAL_MS - elapsed)
    // Sleep `wait` ms, then drain on the next animation frame so the flush
    // lands in the browser's paint cycle rather than mid-task.
    setTimeout(() => {
      if (!this.scheduled) return // a flushNow cancelled it
      requestAnimationFrame(() => this.drain())
    }, wait)
  }

  /**
   * Flush immediately and unconditionally. Cancels any pending coalesced drain
   * so we don't double-flush. Used for terminal signals (turn_complete / error)
   * and non-hot mutations (pushUserMessage, session_created / session_loaded).
   */
  flushNow(): void {
    this.scheduled = false // cancel any pending coalesced drain
    this.lastFlush = Date.now()
    try {
      this.flush()
    } catch {
      /* a flush error must not kill the coalescer */
    }
  }

  private drain(): void {
    if (!this.scheduled) return // cancelled by an intervening flushNow
    this.scheduled = false
    this.lastFlush = Date.now()
    try {
      this.flush()
    } catch {
      /* a flush error must not kill the coalescer */
    }
  }
}
