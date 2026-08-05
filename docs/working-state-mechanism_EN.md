# OneAI Working-State & Cross-Session Continuation Mechanism (Whitepaper)

> An event-sourced per-task file log + in-memory projection + per-step incremental persistence + cross-session index discovery working-state engine: a task's goal/steps/decisions/blockers land as an append-only JSONL event stream in `<root>/tasks/{task_id}.jsonl`; the hot path reads only the `LoopState` in-memory projection (zero file IO); a new session reads the lightweight `tasks.index.json` once to surface the previous session's unfinished work; behavior is declared by DomainPack layer 7 `MemoryProfile.working_state`.

> Version: corresponds to the 1.1.0 line of the codebase. This document is written based on a file-by-file review of the `crates/oneai-core`, `oneai-persistence`, `oneai-agent`, `oneai-app`, and `oneai-domain` source code; every mechanism is annotated with `file:line` for verification. The design rationale is in the sibling `docs/agent-working-state-and-cross-session-resume.md` (Chinese research reference) and `~/.claude/plans/vectorized-hopping-willow.md` (implementation plan).

---

## 0. One-Sentence Summary

OneAI's working-state management is an **"event-sourced per-task file log + in-memory projection + per-step incremental persistence + cross-session index discovery"** engine: a task's goal/steps/decisions/blockers are no longer spread across the session transcript, but instead land as an append-only JSONL event stream in `<root>/tasks/{task_id}.jsonl`. The hot path reads only the in-memory projection in `LoopState` each turn (zero file IO), and a new session starts up by reading the lightweight `tasks.index.json` once to surface unfinished work from last time. The entire behavior is declared by the DomainPack layer-7 `MemoryProfile.working_state`, enabled with a single `AppBuilder::working_state(root)`.

---

## 1. Architecture Overview: Layering and Data Flow

```
        ┌───────────────────────────────────────────────────────────┐
        │                  AgentLoop (oneai-agent)                  │
        │   Control-tool execution points: exit_plan_mode /         │
        │   task_update / decision                                 │
        └───────────────────┬───────────────────────────────────────┘
                            │ append_event (per-step incremental
                            │   persistence, §4)
                            ▼
        ┌───────────────────────────────────────────────────────────┐
        │   LoopState.working_state: Option<WorkingState>           │
        │   ← in-memory projection (projector derives once from    │
        │      events, then hot path reads only it)                 │
        └───────────────────┬───────────────────────────────────────┘
                            │ inject_pinned_blocks re-injected each
                            │   turn (zero IO)
                            ▼
        ┌───────────────────────────────────────────────────────────┐
        │   ContextAssembler pinned blocks: [Task Anchor] /        │
        │   [Plan & Progress] / [Decisions Made] / [Blockers] /    │
        │   [Unfinished Work From Previous Sessions] (first turn)  │
        └───────────────────────────────────────────────────────────┘
                            ▲
                            │ list_open_tasks (reads index.json,
                            │   first turn of a new session)
        ┌───────────────────┴───────────────────────────────────────┐
        │   FileWorkingStateStore (oneai-persistence)              │
        │   <root>/tasks/{task_id}.jsonl  ← append-only event log  │
        │   <root>/tasks.index.json      ← lightweight index        │
        │                                    (cross-session disc.) │
        │   projector / compaction / archive                       │
        └───────────────────────────────────────────────────────────┘
```

**Division of responsibilities across the four crates:**

| crate | role | key files |
|---|---|---|
| `oneai-core` | L0 types: `WorkingState` / `Step` / `Decision` / `Blocker` / `TaskEvent`; `WorkingStateStore` trait | `types.rs:914`, `traits.rs:386` |
| `oneai-persistence` | File backend: `FileWorkingStateStore` + projector + compaction + archive | `working_state_store.rs:36` |
| `oneai-agent` | Projection wiring: `LoopState.working_state` + control-tool event append + pinned-block rendering | `agent_loop.rs`, `context_assembler.rs` |
| `oneai-app` | Integration: `AppBuilder::working_state()` + new-session injection of `[Unfinished Work]` + resume rehydrate | `builder.rs`, `session.rs` |
| `oneai-domain` | Declarative policy: `MemoryProfile.working_state: WorkingStatePolicy` + `RefreshPolicy::OnResume` | `memory_profile.rs`, `context_source.rs:50` |

---

## 2. L0 Types (Event vs. Projection)

Working state is split into two layers: **events** are the source of truth (append-only, persisted to disk); the **projection** `WorkingState` is the runtime view derived from events (in-memory cache).

- `TaskEvent` (`types.rs:1161`) — one JSON per line: `{ id, task_id, session_id, parent_event_id?, event_type, payload, schema_version, ts }`. `parent_event_id` supports audit / fork chains.
- `TaskEventType` (`types.rs:1194`) — `TaskCreated / GoalRevised / StepAdded / StepStatusChanged / DecisionMade / BlockerRaised / BlockerResolved / NoteAdded / TaskPaused / TaskResumed / TaskCompleted / TaskArchived / Reconciliation / Snapshot`.
- `TaskEventPayload` (`types.rs:1230`) — the payload corresponding to each event type (goal/intent for a newly created task; description/order for a step; chosen/rationale for a decision; resolution for a blocker…).
- `WorkingState` (`types.rs:914`) — projection: `{ task_id, user_id, project, goal, intent, status, steps[], decisions[], blockers[], notes[], owner_session, created_at, updated_at }`.
- `TaskStatus` — `Active | Paused | Completed | Archived`; `StepStatus` — `Pending | InProgress | Completed | Failed`.
- `TaskBrief` (`types.rs:1121`) — a single record in index.json: `{ task_id, goal, status, open_step_count, last_event_ts }`; cross-session discovery reads only it.

`PlanStep` (`types.rs:844`) is not removed — it is still used internally by the workflow; on the working-state side `Step` is authoritative, and `PlanState` is demoted to a runtime projection of `Step`.

---

## 3. L1 Event Log: Append-Only File

Storage layout (per `WorkingStatePolicy.storage_root`):

- Coding scenario: `<project_dir>/.oneai/tasks/{task_id}.jsonl` + `.oneai/tasks.index.json` (in-repo, can be `git diff`-reviewed by hand; a git commit = free durability + a reconciliation source).
- Assistant scenario: `~/.oneai/working-state/{user}/{task_id}.jsonl` + `~/.oneai/working-state/{user}/index.json`.

Each `{task_id}.jsonl` is an append-only event stream, one event JSON per line. The **write path is the only one** — `append_event` (`traits.rs:411`): appends one line + incrementally updates the corresponding entry in `tasks.index.json`. The read path splits into two:

- **Hot path**: the `LoopState.working_state` in-memory cache, read every turn by `inject_pinned_blocks`, zero file IO.
- **Cold path / first read**: `derive_state` (`traits.rs:422`) rebuild — find the latest `Snapshot` event + replay the tail after it. Crash recovery / a new session's `continue` goes through it.

**Crash safety (§8.1)**: append-only → if the last line is a partial write (power loss / crash), on reload `read_events` (`working_state_store.rs:79`) skips the line that fails to deserialize, rather than aborting the whole log. Append-per-step = persisted → a crash loses at most the last step.

**index.json does not drift**: it is a derivative of the event log, and `append_event` updates it incrementally on every write; `list_open_tasks` reads only it (does not derive per task), so cross-session discovery is an O(1) file read + O(N) parsing of index entries, with zero per-task IO.

---

## 4. L2/L4 Per-Step Incremental Persistence (Replaces the Old auto_checkpoint)

Old path: `AgentLoop::auto_checkpoint` constructed an `AgentState` bound to `_agent_state` (the underscore = unused) and never called `save()`; `AppSession::save_checkpoint` stored an empty `GlobalState::new()`. Crash recovery did not actually exist — both of these stubs were REMOVED in P4.

New path: `LoopState` holds `working_state: Option<WorkingState>` + `task_id`. At the plan control-tool execution points (the control-tool branch in `agent_loop.rs`) it appends events + updates the in-memory projection:

- `exit_plan_mode` accepts the plan → `TaskCreated` (if a new task) + `StepAdded` for each step.
- `task_update` status change → `StepStatusChanged`. **Append-per-step = persisted**; a crash loses at most the last step.
- `request_plan_decision` once the approval is settled → `DecisionMade` (chosen + rationale + alternatives).
- Escalation / `stuck` (idle timeout) → `BlockerRaised`; recovery → `BlockerResolved`.
- After each append, `compact_if_needed` is triggered per policy.

`WorkingStateStore` is the sole writer — `append_event` is the only persistence entry point, and the in-memory projection is held by the caller; the on-disk `Snapshot` is written only during compaction, by the projector, as a single `Snapshot` event (an event *inside* the log, not a parallel state *outside* the log → this eliminates §8.4 drift at the root: the snapshot and the events can never disagree).

---

## 5. L3 Pinned Projection (Retains the Re-injection Pattern)

`inject_pinned_blocks` (`context_assembler.rs`) retains the architecture of rebuilding every turn without writing back to the durable log; **the data source is switched from `Conversation::metadata` to the in-memory `WorkingState`**:

- `[Task Anchor]` ← `working_state.goal / intent`.
- `[Plan & Progress]` ← `working_state.steps`, rendered as ✅/🔄/⏳.
- `[Decisions Made]` ← `working_state.decisions` (chosen + rationale) — fills the "key decisions" gap.
- `[Blockers]` ← open blockers — fills the "blockers" gap.

Still ephemeral, rebuilt every turn, zero IO. The `original_task` of `from_conversation` (`agent_loop.rs`) is now taken from `WorkingState.goal` and is not overwritten by a new task arg (fixes the old bug: overwriting the goal with a new task arg caused resume to lose the original goal).

---

## 6. L5 Cross-Session Discovery (the User's Core Requirement)

A new session starts up (`AppBuilder::create_session`):

1. It reads `WorkingStateStore::list_open_tasks(user, project)` (a single index.json read, zero per-turn) → injects the `[Unfinished Work From Previous Sessions]` pinned block (first turn `EveryIteration`, then `OnChange`), listing unfinished tasks + progress summaries + open blockers, and asks the user whether to continue one of them.
2. User runs `tasks continue <id>` / platform calls `continue_task` → the new session binds `task_id=id`, `get_task(id)` derives once into `LoopState.working_state`, and the pinned blocks are projected from that task. **It does not read the old session's conversation** (§6.2 — the conversation is a transcript, not the source of working state).
3. `chat --resume <session_id>` (**by design, not yet implemented**): load the conversation (existing SQLite path) + take the `task_id` pointer from the conversation + `get_task(task_id)` to derive and rehydrate. The durable part is covered by the event log; the LoopState runtime fields (paradigm / token budget) are derived from the conversation + working state, not checkpointed separately. Currently `oneai session resume <id>` is print-only preview of the conversation history; live continuation uniformly goes through `tasks continue` (cross-session).

---

## 7. L7 Ground-Truth Reconciliation (§8.2)

`RefreshPolicy` (`context_source.rs:30`) gains an `OnResume` variant (`context_source.rs:50`). CodingPack's `GitReconciliationSource` (`builtin_sources.rs`) runs `git status` / `git log` / `git diff .oneai/` on resume/continue, and reconciles against the WorkingState's "current step / pinned files": on drift it appends a `Reconciliation` event + marks the pinned block stale, with code as the source of truth on conflict. Because working state lives in the in-repo `.oneai/`, `git diff` is naturally the reconciliation source. The assistant pack has no external ground truth and skips this.

---

## 8. L8 Scenario Policy (Folded into MemoryProfile, No New DomainPack Layer)

`MemoryProfile` (`memory_profile.rs`) gains a `working_state: WorkingStatePolicy` sub-structure:

| field | meaning |
|---|---|
| `storage_root` | `InRepo(".oneai")` / `HomeDir("~/.oneai")` |
| `persistence` | `StrictEventSourced` (the only option — principle-driven) |
| `checkpoint_granularity` | `EveryStep` / `CriticalNodes` / `OnTaskBoundary` |
| `ground_truth_reconciliation` | `Git` / `None` |
| `cross_session_surface` | `AutoInject` / `OnDemand` |
| `retention` | `ArchiveOnComplete` / `Keep` |
| `compaction` | `{ event_threshold, keep_recent, max_age_before_archive }` |
| `thickness` | `Thin` (can be re-derived from external sources) / `Thick` (no external GT) |

Two presets: CodingPack = `InRepo + EveryStep + Git + AutoInject + ArchiveOnComplete + Thin + compaction{200, 50, 30d}`; Assistant pack = `HomeDir + OnTaskBoundary + None + AutoInject + Keep + Thick + compaction{500, 100, 90d}`.

---

## 9. L9 Event-Log Compaction (Bounded Growth)

- **Threshold trigger** (`compact_if_needed`, `traits.rs:427`): when a single task's event count exceeds `event_threshold` → the projector folds everything outside `[first .. after the latest Snapshot]` into a single `Snapshot` event (payload = the full WorkingState JSON from `derive_state` at that time), keeping the most recent `keep_recent` raw events. `derive_state` = find the latest `Snapshot` + replay the tail after it.
- **Complete/archive trigger** (`archive_task`, `traits.rs:430`): task → `Completed`/`Archived` → the entire `{task_id}.jsonl` is gzipped into `{task_id}.archive.jsonl.gz`, the index is marked archived, and a single summary is retained (goal + completion time + final step summary) for historical traceability.
- **Time trigger**: once Archived and past `max_age_before_archive` → delete the `.archive.jsonl.gz` (keeping the index summary).

The `Snapshot` is an **event inside the log**, not a parallel state outside the log → the snapshot and the events can never disagree (§8.4 drift is eliminated at the root).

---

## 10. L10 CLI / API

- `oneai tasks list` / `show <id>` / `continue <id>` / `archive <id>` (`examples/cli/src/cmd_tasks.rs`, reads the index/file).
- `oneai chat --resume <id>` (a referenced-but-nonexistent command).
- `oneai run` with no task automatically surfaces `[Unfinished Work]`.
- UniFFI / platform layers expose `list_open_tasks` / `continue_task`.

---

## 11. Comparison with Old Patches (REMOVED in P4)

| Old patch (the lesion) | New mechanism |
|---|---|
| `Conversation::metadata["task_anchor"]/["plan_state"]` as the working-state source | Stores only the `task_id` pointer; pinned blocks read the in-memory `WorkingState` |
| `AgentLoop::auto_checkpoint` (no-op stub, bound to `_agent_state`, never saves) | REMOVED; replaced by per-step append to the event log |
| `AppSession::save_checkpoint` (empty `GlobalState::new()`) | REMOVED; durability is covered by the event log |
| `ProgressiveCheckpointManager` / `CheckpointBackend` / `AutoSavePolicy` (SQLite checkpoint infra) | The entire `progressive_checkpoint.rs` was REMOVED; working state uses the file event log and does not reuse this set |
| `CoreMemory::pinned: RwLock<Vec<String>>` (process memory, not persistent) | Folded into a `MemoryFact.pinned: bool` column (`#[serde(default)]` + a SQLite `pinned` column + migration); the pin state is serialized with the fact and survives restarts |
| `from_conversation` overwriting `original_task` with a new task arg | Rehydrated from `WorkingState.goal`, not overwritten |
| `cmd_session::cmd_session_resume` print-only + pointing at a phantom command | `tasks continue <id>` is real continuation (implemented, cross-session binding of task_id + derive); `chat --resume` same-session real continuation is **planned but not implemented**; `session resume` remains print-only preview |

> Note: `FilePersistence` / `StatePersistence` trait / `AgentState` / `CheckpointInfo` (`checkpoint.rs`) are **retained** — they are the backend for the Studio Web UI checkpoint browser (list/load/browse already-saved state files) and are unrelated to the progressive checkpoint manager.

---

## 12. Verification Points

1. **Unit**: `FileWorkingStateStore` appends N events → `derive_state` == the in-memory cache; after compaction derive is consistent; a partial-write corrupted last line is ignored; the index does not drift from the file.
2. **Performance**: every turn `inject_pinned_blocks` reads the in-memory WorkingState (<μs level, zero file IO); append-event latency (ms level, single-line append).
3. **Same-session resume** (`chat --resume`, **not yet implemented**; currently `session resume` is print-only preview): a long task runs to the middle and is killed → restart `chat --resume` → goal/steps/decisions/blockers rehydrate from the JSONL, pinned blocks are correct, `original_task` is not overwritten.
4. **Cross-session (core)**: session A creates a task, runs to step 3 without finishing → exits → a new session B is created (new session_id, does not read A's conversation) → the first turn shows `[Unfinished Work]` containing A's task (read from index.json) → `tasks continue <A_task_id>` → B binds A's task_id, derives A's working_state into LoopState, the pinned projection shows step-3 progress, and does not repeat completed steps.
5. **Reconciliation**: under CodingPack, if git is changed externally between sessions → on continue the pinned block is marked stale + a `Reconciliation` event is recorded.
6. **Bounded growth**: a single task's appends exceed `event_threshold` → old events are folded into a snapshot, the log shrinks; task completes → `.archive.jsonl.gz` is generated, the index is marked archived.
7. **Pin persistence**: pin a fact in core memory → after restart the SQLite `pinned` column is still set, and `enforce_budget` does not evict it (`sqlite_store.rs::pinned_flag_survives_sqlite_roundtrip`).

---

## 13. Design Trade-offs and Industry Benchmarking

- **Files, not a DB, for storage**: working state is structured per-task JSONL. Rationale: human-readable and editable (manually fix a stuck task); git-versionable (the coding scenario gets free durability + diff-as-reconciliation-source); append-only partial-write tolerance; no schema-migration pain; zero dependencies; matches the "document" substrate nature of working state. Claude Code (`~/.claude/projects/{cwd}/{id}.jsonl`), RALPH (TASKS.md + git), and agent-session-resume (handoff.md) are all file-based. A DB's indexed-query advantage only shows up for cross-session large-scale search / multi-agent coordination, which is outside OneAI's local single-user target scope.
- **The hot path uses an in-memory cache, the file is only a durable mirror**: working state is derived once at session startup → cached into `LoopState`; lifecycle events append + update the in-memory cache; pinned re-injection reads the in-memory cache every turn, zero IO. This is Claude Code's model (todos in memory, the JSONL is a mirror), avoiding the performance problem of querying the store every turn.
- **Principle-driven + projection-rebuildable**: the append-only event log is the source of truth; the in-memory working state is a projection derived from events and can be rebuilt at any time. The read path goes through the in-memory cache (no replay every time); after a crash it is rebuilt from the events.

Reference sources: Claude Code session storage (JSONL append-only), the TASKS.md pattern (RALPH / agent-session-resume), reference doc §7.1 state derived from events, §8.1/8.2/8.4 failure modes, §10.2-10.3 working-state file startup injection. The full research is in `docs/agent-working-state-and-cross-session-resume.md` (Chinese).

---

## Dependencies

| Direction | Who | What |
|---|---|---|
| Upstream | `oneai-core` | `TaskEvent`/`TaskEventType`/`WorkingState`/`TaskId` shared types |
| Upstream | `oneai-persistence` | `FileWorkingStateStore` (append-only JSONL + `tasks.index.json` + compaction) |
| Upstream | `oneai-domain` | `MemoryProfile.working_state` policy (folds into layer 7, no new DomainPack layer) |
| Downstream | `oneai-agent` | `LoopState.working_state` in-memory projection (zero-IO hot read) + `OnResume` reconciliation + cadence hydrate |
| Downstream | `oneai-app` | `AppSession` startup surfaces `[Unfinished Work From Previous Sessions]` + `tasks continue <id>` binds |
| Cross-cutting | DomainPack layer 7 | `MemoryProfile.working_state` declares the persistence policy |

---

## Further reading

- [memory-mechanism](memory-mechanism_EN.md) — the memory path (facts/recall), a separate persistence path from working-state
- [persistence-mechanism](persistence-mechanism_EN.md) — SQLite (sessions/LTM) + file event log, two paths
- [domain-pack-mechanism](domain-pack-mechanism_EN.md) — layer 7 `MemoryProfile.working_state` declaration
- [multi-agent-mechanism](multi-agent-mechanism_EN.md) — `LoopState` projection consumption in the AgentLoop
- Research reference: `docs/agent-working-state-and-cross-session-resume.md`
- Source: `crates/oneai-persistence/src/` (file event log) + `crates/oneai-agent/src/loop_state.rs` (projection)
- [CLAUDE.md — Working state & cross-session resume](../CLAUDE.md)
