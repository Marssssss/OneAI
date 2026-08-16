# OneAI webUI (`platforms/web`)

OneAI's end-user-facing Web interface — a React SPA over the
`oneai app-server` JSON-RPC (WebSocket) protocol. W1–W3 are landed: single-
agent streaming chat, the full tool/approval/plan conversation domain, and
the scenario conversation entry (OneAI's differentiator). Design language
and architecture benchmarked against deepseek-harness's webUI (see
`docs/webui-refactor-design.md`).

## Stack

- React 19 + Vite 7 + TypeScript (strict, `verbatimModuleSyntax`)
- CSS Modules + `--oneai-*` design tokens (light/dark)
- `useSyncExternalStore` external store (no Redux/Zustand) — the projection
  store accumulates `EngineYield` events into chat nodes
- `mdast-util-from-markdown` + `micromark-extension-gfm` + `shiki` — block-level
  incremental markdown (keyed blocks reuse React subtrees; only the tail
  streaming block re-renders/re-highlights)
- **StreamCoalescer** (20fps) — batches per-token mutations so React never
  re-renders per token; terminal signals (turn_complete/error) flush
  immediately. Mirrors the macOS SwiftUI `ChatViewModel` coalescer.

## Layout

Three-column `AppFrame` (sidebar | center | details) with a concession
geometry solver: center never yields below 640px; details shrinks then
auto-closes; sidebar collapses to a 56px rail. Narrow screens auto-collapse
the sidebar.

## Wire contract

Verified against the Rust sources (`oneai-app-server`/`oneai-bus`):

- one WebSocket text frame = one JSON-RPC message (either direction);
- `turn/run` → `{content: ContentBlock[]}`; blocking-ack → `{turn_id, task}`;
- outbound: only the `event` notification, `params` = the whole `EngineYield`
  (`kind` snake_case tag). Unknown `kind`s are ignored (transparent growth).
- **scenario/\*** (synchronous CRUD): `scenario/list` `/get` `/upsert`
  `/delete` `/validate` — the shared scenario library; `scenario/validate` is
  the single authoritative validator every frontend's editor calls.
- **group/\*** (ack — results stream as `event`s): `group/start` (compiled
  `BusGroupScenario`), `group/open` (opener), `group/run` (user message),
  `group/set_order` (debrief narrowing).

## Scenario conversation entry (W3)

OneAI's differentiator dsh entirely lacks: multi-role group chat via
`BusScenario` (members + turn policy + topic fields + debrief + review loop).
The Web frontend makes it a first-class entry alongside the macOS/Windows/
VS Code/browser ports, sharing the same engine authority.

- **Sidebar Scenarios section**: local presets (`preset-*`, 5 bilingual) +
  server customs; pick → topic intake (when `topic_fields`) → group chat.
  `+/✎` open the React scenario editor.
- **Hero chips** on the empty state; **slash commands** `/scenario`
  `/new-scenario` `/edit-scenario`.
- **TopicIntake** collects values, compiled (`scenario/compile.ts`) into a
  `BusGroupScenario` — baking the visible topic background into each member's
  `system_prompt` per `visible_to`, dropping UI-only fields — then
  `group/start` + `group/open`|`group/run`.
- **GroupChatView**: speaker-tagged bubbles (name + color dot resolved from
  the active scenario's members); `speaker` on each fragment + `speaker_turn`
  round boundaries drive bubble switching via StreamCoalescer.
- **Debrief**: end-of-scenario button → `group/set_order` narrows to the
  debrief member → `group/run(summary_prompt)` → single-member summary.
- **ScenarioEditor**: React, full field set, live `scenario/validate` (300ms
  debounce) with localized inline errors; `scenario/upsert`/`delete`. Presets
  are read-only.

## Develop

Start the engine's ws transport in one terminal:

```bash
oneai app-server --listen ws://127.0.0.1:8787
# (provider via ONEAI_API_KEY / ONEAI_BASE_URL / ONEAI_MODEL env)
```

Then the SPA:

```bash
cd platforms/web
npm install
npm run dev      # http://localhost:5173 → connects to ws://127.0.0.1:8787
```

Override the app-server URL with `VITE_APP_SERVER_URL=ws://host:port`.

## Status

- **W1 (done)**: scaffold + RPC client + projection store + StreamCoalescer +
  AppFrame + conversation (streaming markdown) + sidebar (sessions) +
  theme/locale.
- **W2 (done)**: tool tree (paired args↔result) + approval panel (parallel
  queue, all 5 InteractionRequest variants) + plan mode + incremental
  markdown (mdast+shiki) + details rail.
- **W3 (done)**: scenario conversation entry — sidebar Scenarios section,
  hero chips, `/scenario` commands, ScenarioPicker + TopicIntake +
  ScenarioEditor (React, live `scenario/validate`), group chat with
  speaker-tagged bubbles, debrief. 5 bilingual presets + local/server merge.
  Verified: typecheck + build green (runtime smoke against a live
  `oneai app-server` pending).
- **W4 (next)**: capability panel (trajectory/StateGraph visualization) +
  settings modal + provider config RPC + attachments.
- **W5**: design-token pixel alignment + dark full-coverage + responsive +
  share the React scenario editor with the VS Code/browser extensions.
