// Test setup — registers @testing-library/jest-dom matchers (toBeInTheDocument,
// toHaveAttribute, …). The StreamCoalescer now falls back to setTimeout when
// rAF is absent (jsdom), so no rAF polyfill is needed here.
import '@testing-library/jest-dom/vitest'

// jsdom in this Vitest config exposes `localStorage` as a bare object without
// the Storage interface (getItem/setItem/clear are undefined), so any code
// touching localStorage (the workspace store) silently no-ops.
// Install a minimal in-memory Storage so those code paths are testable.
const store = new Map<string, string>()
const localStorageMock: Storage = {
  get length() {
    return store.size
  },
  clear: () => store.clear(),
  getItem: (k: string) => (store.has(k) ? store.get(k)! : null),
  setItem: (k: string, v: string) => {
    store.set(k, String(v))
  },
  removeItem: (k: string) => {
    store.delete(k)
  },
  key: (i: number) => Array.from(store.keys())[i] ?? null,
}
;(globalThis as unknown as { localStorage: Storage }).localStorage = localStorageMock
