// Theme resolution — extracted so it's unit-testable (App.tsx keeps the live
// `prefers-color-scheme` listener + persistence; this is just the initial
// read). Semantics: an explicit localStorage choice wins; otherwise the OS
// preference is used AND marked non-explicit so the app keeps following the
// OS until the user toggles.

export type Theme = 'light' | 'dark'

export interface InitialTheme {
  theme: Theme
  /** True when the user explicitly chose a theme (persisted). False when the
   * value is derived from the OS `prefers-color-scheme` — in that state the
   * app keeps following OS changes live. */
  explicit: boolean
}

const STORAGE_KEY = 'oneai-theme'

export function readInitialTheme(): InitialTheme {
  try {
    const stored = localStorage.getItem(STORAGE_KEY)
    if (stored === 'dark' || stored === 'light') return { theme: stored, explicit: true }
  } catch {
    /* ignore */
  }
  if (
    typeof window !== 'undefined' &&
    window.matchMedia?.('(prefers-color-scheme: dark)').matches
  ) {
    return { theme: 'dark', explicit: false }
  }
  return { theme: 'light', explicit: false }
}

export const THEME_STORAGE_KEY = STORAGE_KEY
