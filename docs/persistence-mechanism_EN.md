# OneAI Persistence Mechanism

> SQLite (sessions / LTM / usage) + a file event log (working state / cross-session continuation) — two complementary persistence paths.

## Responsibility

Keep two kinds of agent state alive across time scales: ① conversation history / long-term facts / usage records — relational, via SQLite; ② a task's working state (goal / steps / decisions / blockers) — an append-only event log, via files. The latter is session-independent and resumable across sessions.

## SQLite session store

`SqliteSessionStore` persists sessions / LTM / usage; `AppSession` auto-saves after every run. `oneai session list / resume <id> / delete / info`, `oneai memory search/list --user`.

> `oneai session resume <id>` is currently **print-only preview** (shows conversation history, does not run the agent loop, does not rehydrate working state). Live continuation goes through `tasks continue <id>`.

## File event log (working state)

A task's working state is not spread across the session transcript but persisted as a **per-task append-only event log** (source of truth), independent of any session. A new session reads a lightweight index once and surfaces last time's unfinished work. Full mechanism in [Working-State mechanism](working-state-mechanism_EN.md).

## Key types & files

| Item | Location |
|---|---|
| `SqliteSessionStore` (sessions / LTM / usage) | `crates/oneai-persistence/src/sqlite_store.rs` |
| `FileWorkingStateStore` (event log) | `crates/oneai-persistence/src/working_state_store.rs` |
| `SqliteUsageTracker` | `crates/oneai-persistence/src/usage_tracker.rs` |
| Studio checkpoint backend | `crates/oneai-persistence/src/checkpoint.rs`, `state.rs` |

## Related CLI

[`session list/resume/delete/info/decay/export-hf`](cli-reference_EN.md#persistent-sessions-sqlite), [`tasks list/show/continue/archive`](cli-reference_EN.md#working-state-cross-session-task-continuation), [`memory search/list`](cli-reference_EN.md#memory-cross-session-persistent-facts), [`usage report/session/export`](cli-reference_EN.md#usage-records-token-only-no-usd).

## Further reading

- [Working-State mechanism](working-state-mechanism_EN.md)
- Memory persistence — see [Memory mechanism](memory-mechanism_EN.md)
