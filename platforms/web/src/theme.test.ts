import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { readInitialTheme, THEME_STORAGE_KEY, type InitialTheme } from './theme'

// readInitialTheme: explicit localStorage choice wins (explicit=true, stops
// following OS); otherwise OS preference used and explicit=false so the app
// keeps following the OS until the user toggles. Tests stub localStorage +
// matchMedia directly (jsdom's localStorage is flaky in this env) so the
// behavior is exercised regardless of the host.

function makeStorage(): Storage {
  const map = new Map<string, string>()
  return {
    get length() {
      return map.size
    },
    clear: () => map.clear(),
    getItem: (k: string) => (map.has(k) ? (map.get(k) as string) : null),
    key: (i: number) => Array.from(map.keys())[i] ?? null,
    removeItem: (k: string) => {
      map.delete(k)
    },
    setItem: (k: string, v: string) => {
      map.set(k, v)
    },
  } as Storage
}

function setOsPrefersDark(dark: boolean | 'absent'): void {
  if (dark === 'absent') {
    // Simulate a host with no matchMedia at all.
    Object.defineProperty(window, 'matchMedia', {
      writable: true,
      configurable: true,
      value: undefined,
    })
    return
  }
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    configurable: true,
    value: (query: string) => ({
      matches: query.includes('dark') ? dark : !dark,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    }),
  })
}

describe('readInitialTheme', () => {
  beforeEach(() => {
    vi.stubGlobal('localStorage', makeStorage())
  })
  afterEach(() => {
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  it('returns an explicit stored choice and marks it explicit', () => {
    localStorage.setItem(THEME_STORAGE_KEY, 'dark')
    expect(readInitialTheme()).toEqual<InitialTheme>({ theme: 'dark', explicit: true })

    localStorage.setItem(THEME_STORAGE_KEY, 'light')
    expect(readInitialTheme()).toEqual<InitialTheme>({ theme: 'light', explicit: true })
  })

  it('falls back to the OS preference when nothing is stored, non-explicit', () => {
    setOsPrefersDark(true)
    expect(readInitialTheme()).toEqual<InitialTheme>({ theme: 'dark', explicit: false })

    setOsPrefersDark(false)
    expect(readInitialTheme()).toEqual<InitialTheme>({ theme: 'light', explicit: false })
  })

  it('defaults to light when nothing stored and no matchMedia', () => {
    setOsPrefersDark('absent')
    expect(readInitialTheme()).toEqual<InitialTheme>({ theme: 'light', explicit: false })
  })

  it('treats a corrupt stored value as no choice', () => {
    localStorage.setItem(THEME_STORAGE_KEY, 'mauve')
    setOsPrefersDark(true)
    expect(readInitialTheme()).toEqual<InitialTheme>({ theme: 'dark', explicit: false })
  })
})
