// slashCommands — registry + two-level suggestion/parse logic (issue #39).
// Pure module, no DOM: guards the TUI-aligned scope, the grouped ordering,
// the issue-#30-style two-level filtering, and the dispatch parser.

import { describe, expect, it } from 'vitest'
import {
  COMMAND_GROUPS,
  SLASH_COMMANDS,
  getSuggestions,
  parseSlash,
} from './slashCommands'

const labels = (text: string): string[] =>
  getSuggestions(text).map((s) => s.display)

describe('registry (issue #39: TUI-aligned scope)', () => {
  it('exposes exactly the TUI-aligned command set (no scenario/settings/plan/trajectory)', () => {
    const set = SLASH_COMMANDS.map((c) => c.label).sort()
    expect(set).toEqual(
      [
        '/clear',
        '/compact',
        '/domain',
        '/help',
        '/init',
        '/new',
        '/session',
        '/skills',
        '/usage',
      ].sort(),
    )
  })

  it('is grouped + sorted by functional group, groups in COMMAND_GROUPS order', () => {
    const groupSeq = SLASH_COMMANDS.map((c) => c.group)
    let last = -1
    for (const g of groupSeq) {
      const idx = COMMAND_GROUPS.indexOf(g)
      expect(idx).toBeGreaterThanOrEqual(last)
      last = idx
    }
    // Every declared group is actually populated.
    for (const g of COMMAND_GROUPS) {
      expect(SLASH_COMMANDS.some((c) => c.group === g)).toBe(true)
    }
  })

  it('only /session and /init carry subcommands; /session is not executable bare', () => {
    const withSubs = SLASH_COMMANDS.filter((c) => c.subs !== undefined).map((c) => c.cmd)
    expect(withSubs.sort()).toEqual(['init', 'session'])
    const session = SLASH_COMMANDS.find((c) => c.cmd === 'session')
    expect(session?.executableBare).toBe(false)
    const init = SLASH_COMMANDS.find((c) => c.cmd === 'init')
    expect(init?.executableBare).toBe(true)
  })
})

describe('getSuggestions — top level', () => {
  it('lists every command (in group order) for a bare /', () => {
    expect(labels('/')).toEqual(SLASH_COMMANDS.map((c) => c.label))
  })

  it('prefix-filters top-level labels', () => {
    expect(labels('/cl')).toEqual(['/clear'])
    expect(labels('/se')).toEqual(['/session'])
    expect(labels('/z')).toEqual([])
  })

  it('ignores non-slash input and input with a space before level-2', () => {
    expect(labels('')).toEqual([])
    expect(labels('hello')).toEqual([])
  })

  it('marks leaf commands immediately executable, /session as a folder', () => {
    const clear = getSuggestions('/clear')[0]
    expect(clear.invocation).toEqual({ cmd: 'clear', sub: null, arg: null })
    expect(clear.insert).toBe('/clear')
    const session = getSuggestions('/session')[0]
    expect(session.invocation).toBeNull()
    expect(session.insert).toBe('/session ')
    const init = getSuggestions('/init')[0]
    expect(init.invocation).toEqual({ cmd: 'init', sub: null, arg: null })
    expect(init.insert).toBe('/init ')
  })
})

describe('getSuggestions — second level (issue #39 requirement 2)', () => {
  it('surfaces subcommands after `<cmd> ` (even with an empty partial)', () => {
    expect(labels('/session ')).toEqual(['/session list', '/session resume'])
    expect(labels('/init ')).toEqual(['/init oneai', '/init agents', '/init claude'])
  })

  it('filters subcommands by the partial token', () => {
    expect(labels('/session r')).toEqual(['/session resume'])
    expect(labels('/init a')).toEqual(['/init agents'])
    expect(labels('/session xyz')).toEqual([])
  })

  it('offers no popup for commands without subcommands', () => {
    expect(labels('/clear ')).toEqual([])
    expect(labels('/clear x')).toEqual([])
  })

  it('closes the popup in the free-form arg zone (after the subcommand)', () => {
    expect(labels('/session resume ')).toEqual([])
    expect(labels('/session resume abc')).toEqual([])
    expect(labels('/init oneai --force')).toEqual([])
  })

  it('takesArg subcommands accept into a trailing space without an invocation', () => {
    const resume = getSuggestions('/session resume')[0]
    expect(resume.invocation).toBeNull()
    expect(resume.insert).toBe('/session resume ')
    const list = getSuggestions('/session list')[0]
    expect(list.invocation).toEqual({ cmd: 'session', sub: 'list', arg: null })
    expect(list.insert).toBe('/session list ')
  })
})

describe('parseSlash — dispatch parsing', () => {
  it('parses bare commands', () => {
    expect(parseSlash('/help')).toEqual({
      kind: 'command',
      invocation: { cmd: 'help', sub: null, arg: null },
    })
  })

  it('parses subcommand + arg, collapsing whitespace', () => {
    expect(parseSlash('/session resume abc')).toEqual({
      kind: 'command',
      invocation: { cmd: 'session', sub: 'resume', arg: 'abc' },
    })
    expect(parseSlash('  /init   oneai   --force  ')).toEqual({
      kind: 'command',
      invocation: { cmd: 'init', sub: 'oneai', arg: '--force' },
    })
  })

  it('flags unknown commands (the composer shows a note, never sends them)', () => {
    expect(parseSlash('/foo')).toEqual({ kind: 'unknown', label: '/foo' })
    expect(parseSlash('/scenario')).toEqual({ kind: 'unknown', label: '/scenario' })
    expect(parseSlash('/settings')).toEqual({ kind: 'unknown', label: '/settings' })
  })

  it('returns null for non-slash input (a normal message)', () => {
    expect(parseSlash('hello')).toBeNull()
    expect(parseSlash('')).toBeNull()
  })
})
