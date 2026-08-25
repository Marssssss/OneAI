// Slash-command registry for the webUI composer (issue #39).
//
// Scope mirrors the TUI (`examples/cli` `SLASH_COMMANDS` + its dispatch tree
// in `tui/mod.rs`), narrowed to the commands the app-server JSON-RPC surface
// (or a client-side action) can actually back:
//
//   TUI command   web backing
//   /help         client-side note (this registry is the source of truth)
//   /new          session/create
//   /clear        session/clear
//   /compact      conversation/compact
//   /usage        opens the usage·trajectory details rail
//   /session      session/list + session/load (list | resume <id>)
//   /init         project/init (oneai | agents | claude)
//   /skills       the Skills modal
//   /domain       the DomainPack modal
//
// TUI commands with no app-server RPC are deliberately absent rather than
// surfaced as broken options (the Footprint Ladder rule applies to the palette
// too): /tools · /context · /wf · /tool · /skill · /quit. The old web-only
// scenario/settings/plan/trajectory entries were dropped for the same reason —
// scenario + settings live in the sidebar/menus, plan mode in the mode chip,
// and the trajectory rail opens via /usage (its TUI-aligned name).
//
// Two-level suggestions follow the TUI's issue-#30 model: while the first
// token is incomplete the popup lists top-level commands; once the command is
// fully typed + a space, its subcommands (if any) surface; past the
// subcommand the input is free-form (an id, flags, …) and the popup closes.

export type SlashCommand =
  | 'help'
  | 'new'
  | 'clear'
  | 'compact'
  | 'usage'
  | 'skills'
  | 'domain'
  | 'init'
  | 'session'

/** Functional groups (issue #39: commands classified + sorted by what they
 *  actually do). Rendered top-down in this order. */
export type CommandGroup = 'session' | 'view' | 'project' | 'extensions'

export const COMMAND_GROUPS: CommandGroup[] = [
  'session',
  'view',
  'project',
  'extensions',
]

export interface SlashSubCommand {
  name: string
  /** i18n key for the description. */
  descKey: string
  /** A free-form argument follows this subcommand (e.g. `resume <id>`) — the
   *  suggestion inserts a trailing space and closes the popup. */
  takesArg?: boolean
}

export interface SlashCommandDef {
  cmd: SlashCommand
  label: string
  /** i18n key for the description. */
  descKey: string
  group: CommandGroup
  /** Executable with no subcommand (`/init` defaults to ONEAI.md). Commands
   *  that are NOT executable bare (`/session`) act as folders: Enter/accept
   *  fills `<cmd> ` and opens the subcommand level instead of running. */
  executableBare: boolean
  subs?: SlashSubCommand[]
}

/** The registry — grouped, and sorted within each group by everyday
 *  usefulness (create → clear → compact → history). */
export const SLASH_COMMANDS: SlashCommandDef[] = [
  { cmd: 'new', label: '/new', descKey: 'command.new', group: 'session', executableBare: true },
  { cmd: 'clear', label: '/clear', descKey: 'command.clear', group: 'session', executableBare: true },
  { cmd: 'compact', label: '/compact', descKey: 'command.compact', group: 'session', executableBare: true },
  {
    cmd: 'session',
    label: '/session',
    descKey: 'command.session',
    group: 'session',
    executableBare: false,
    subs: [
      { name: 'list', descKey: 'command.session.list' },
      { name: 'resume', descKey: 'command.session.resume', takesArg: true },
    ],
  },
  { cmd: 'usage', label: '/usage', descKey: 'command.usage', group: 'view', executableBare: true },
  { cmd: 'help', label: '/help', descKey: 'command.help', group: 'view', executableBare: true },
  {
    cmd: 'init',
    label: '/init',
    descKey: 'command.init',
    group: 'project',
    executableBare: true,
    subs: [
      { name: 'oneai', descKey: 'command.init.oneai' },
      { name: 'agents', descKey: 'command.init.agents' },
      { name: 'claude', descKey: 'command.init.claude' },
    ],
  },
  { cmd: 'skills', label: '/skills', descKey: 'command.skills', group: 'extensions', executableBare: true },
  { cmd: 'domain', label: '/domain', descKey: 'command.domain', group: 'extensions', executableBare: true },
]

/** A parsed slash dispatch — what `App.handleSlash` executes. */
export interface SlashInvocation {
  cmd: SlashCommand
  /** Subcommand token, if typed (e.g. `list` / `resume` / `oneai`). */
  sub: string | null
  /** Free-form remainder after the subcommand, trimmed (may contain flags
   *  like `--force`). null when absent. */
  arg: string | null
}

export type SlashParse =
  | { kind: 'command'; invocation: SlashInvocation }
  | { kind: 'unknown'; label: string }

/** One popup entry — either a top-level command or a subcommand line. */
export interface SlashSuggestion {
  /** Full input line accepted into the composer (may end with a space that
   *  opens the next level / makes room for a free-form arg). */
  insert: string
  /** Display label (no trailing space), e.g. `/session resume`. */
  display: string
  descKey: string
  group: CommandGroup
  /** What Enter/click executes immediately. null ⇒ accepting only fills the
   *  input (a folder command, or a subcommand awaiting its free-form arg). */
  invocation: SlashInvocation | null
}

/** Two-level suggestion filter — the web mirror of the TUI's
 *  `get_command_suggestions` (issue #30). Returns [] whenever the popup must
 *  be hidden (non-slash input, no matches, or the free-form arg zone). */
export function getSuggestions(text: string): SlashSuggestion[] {
  if (!text.startsWith('/')) return []
  const spaceIdx = text.indexOf(' ')

  if (spaceIdx === -1) {
    // Still completing the command name — prefix-match top-level labels,
    // preserving the grouped/sorted registry order.
    return SLASH_COMMANDS.filter((c) => c.label.startsWith(text)).map((c) => ({
      // Commands with subcommands accept into `<cmd> ` (opens the second
      // level); leaf commands accept as-is.
      insert: c.subs === undefined ? c.label : `${c.label} `,
      display: c.label,
      descKey: c.descKey,
      group: c.group,
      invocation: c.executableBare ? { cmd: c.cmd, sub: null, arg: null } : null,
    }))
  }

  const cmdLabel = text.slice(0, spaceIdx)
  const def = SLASH_COMMANDS.find((c) => c.label === cmdLabel)
  if (def === undefined || def.subs === undefined) return []
  const subPartial = text.slice(spaceIdx + 1)
  // Past the subcommand token the input is free-form (id/flags/…) — no popup.
  if (subPartial.includes(' ')) return []
  return def.subs
    .filter((s) => s.name.startsWith(subPartial))
    .map((s) => ({
      // Trailing space mirrors the TUI accept flow: room for the next token,
      // and (for takesArg subs) the popup closes into the free-form zone.
      insert: `${def.label} ${s.name} `,
      display: `${def.label} ${s.name}`,
      descKey: s.descKey,
      group: def.group,
      invocation: s.takesArg === true
        ? null
        : { cmd: def.cmd, sub: s.name, arg: null },
    }))
}

/** Parse a fully-typed slash line for dispatch on Enter/click.
 *
 * `/session resume abc` → `{cmd:'session', sub:'resume', arg:'abc'}`;
 * `/init oneai --force` → `{cmd:'init', sub:'oneai', arg:'--force'}`;
 * unknown `/foo` → `{kind:'unknown'}` (the composer shows a note instead of
 * sending the raw text to the model — TUI parity). */
export function parseSlash(raw: string): SlashParse | null {
  const trimmed = raw.trim()
  if (!trimmed.startsWith('/')) return null
  const tokens = trimmed.split(/\s+/)
  const label = tokens[0]
  const def = SLASH_COMMANDS.find((c) => c.label === label)
  if (def === undefined) return { kind: 'unknown', label }
  const sub = tokens.length > 1 ? tokens[1] : null
  const arg = tokens.length > 2 ? tokens.slice(2).join(' ').trim() : null
  return {
    kind: 'command',
    invocation: { cmd: def.cmd, sub, arg: arg !== null && arg.length > 0 ? arg : null },
  }
}
