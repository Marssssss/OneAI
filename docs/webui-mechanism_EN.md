# OneAI WebUI Mechanism

> `platforms/web` — OneAI's browser frontend: a React SPA that connects over a bare WebSocket to the `oneai app-server` JSON-RPC 2.0 protocol layer, sharing that single frontend protocol with the IDE plugins / desktop (Swift/C#) / mobile clients. One engine process feeds all such frontends; the WebUI is the "browser-only" one. This documents the post-W1–W5 refactor final shape (architecture, protocol wiring, projection, streaming throttle, geometry, scenarios, tests, build).

## 1. Overview (what it is)

The WebUI is a Vite + React 19 + TypeScript SPA (`platforms/web/`). It is not produced by a standalone dev server — `vite.config.ts` serves the SPA directly in the `serve` phase; once the browser opens it, it connects a bare WebSocket to `oneai app-server --listen ws://127.0.0.1:8787` (overridable via `VITE_APP_SERVER_URL`). No HTTP routing proxy: ws and vite's 5173 are two independent hosts the browser reaches separately.

It **does not replicate dsh's Cordis/slots plugin-loading system** — OneAI has no JS-plugin requirement, so a build-time static bundle suffices; route-level lazy import + lazy shiki loads carve domains. Design tokens go through the `--oneai-*` CSS variable layer; theme switching is a single attribute flip.

## 2. Layering (key)

- **L4 engine** (unchanged, `oneai-agent`+`oneai-app`): `AgentLoop` + `BusObserver` + `BusInteractionGate`, unaware whether the peer is web or TUI.
- **L3 bus** (unchanged, `oneai-bus`): `Directive`/`EngineYield` newline-JSON + `InProcessBus`. app-server adapts it to JSON-RPC.
- **L2 app-server** (`oneai-app-server`): JSON-RPC 2.0 adapter — inbound `turn/run`→`UserMessage` etc.; outbound wraps `bus.subscribe_yields()` into a single `event` notification (`params` = the full `EngineYield`, tagged by `kind`). The WebUI is just a ws client; it does not see L2/L3 internals.
- **L1 WebUI** (this doc): `OneAiRpcClient` (ws JSON-RPC) → `ProjectionStore` (yield→UI-state projection) → React (`useSyncExternalStore` binding).

## 3. Process topology

```
browser ──ws──> oneai app-server --listen ws://127.0.0.1:8787 ──bus──> AgentLoop
            (JSON-RPC 2.0: turn/run·session/*·scenario/*·approval/respond …)
            <──event── (EngineYield: turn_start·stream_chunk·tool_result·turn_complete …)
```

A single app-server process can concurrently feed TUI (in-process), IDE (stdio), web (ws), desktop (ipc) frontends; the WebUI is the ws one. `oneai serve` (newline-JSON passthrough sidecar) is the escape hatch, coexisting with `app-server` on a separate socket. The WebUI defaults to `app-server`.

## 4. Protocol wiring (OneAiRpcClient)

`src/rpc/client.ts`. A single `OneAiRpcClient`:

- **call<P,R>(method, params)**: sends `{jsonrpc:'2.0', id, method, params}`, `id` monotonically incrementing; awaits the matching-`id` response (`result`/`error`). `turn/run`'s response is `{turn_id}` (returns on TurnStart) — streamed chunks are subsequent `event` notifications, not in the response.
- **notify(method, params)**: fire-and-forget notification (no id).
- **onEvent(fn)** / **onStatus(fn)**: register the `event` notification callback and the connection-status callback.
- **inbound `handleMessage`**: parses JSON frames — an `id`-bearing frame is a response (resolves pending); an id-less frame is a notification (`event`/`session_created` …) → dispatched to `EventListener`.

The type contract is in `src/rpc/types.ts`: `EngineYield` is the TS mirror of `#[serde(tag="kind", rename_all="snake_case")]` + `#[non_exhaustive]` — the frontend only enumerates the variants it currently consumes and **MUST ignore unknown `kind`s** (matches the Rust contract; new variants are transparent).

## 5. ProjectionStore — yield→UI-state projection

`src/store/projection.ts` is a one-way projector: `consume(y: EngineYield)` folds each yield into a `WorkingState`, then `emit()` builds an immutable snapshot (`ProjectionSnapshot`) to notify React. **Hot variants (`stream_chunk`/`thinking`) go through the coalescer for deferred notification; everything else** (`turn_start`/`tool_result`/`turn_complete`/`approval_request`/`session_*` …) **notifies immediately** (`emitNow()`).

- `subscribe`/`getSnapshot`: the `useSyncExternalStore` contract. The `snapshot.nodes` React receives is a shallow-copied new array; node objects are replaced (not mutated) per edit, so memoized row components skip unchanged rows via diff.
- `attach()`: in a `useEffect`, wires `rpc.onEvent` to `consume` — not in the constructor. Under StrictMode the effect simulates an unmount-then-remount; a subscription made in the constructor is never re-established (the `useMemo` singleton's constructor doesn't re-run) → events stop. Driving it from the effect makes subscribe/unsubscribe symmetric.
- Projections: the `ChatNode` tree (user/assistant/system · text/thinking/tool/plan/error · streaming/done/error) + `approvalQueue` (parallel approval queue; head is `currentApproval`) + `trajectory` (turn-level event ledger) + `working` (goal/steps/decisions/blockers) + `subagents` + `usage` + `turnTimings`.

## 6. StreamCoalescer — 20fps frontend throttle

`src/stream/coalescer.ts`. The app-server **does not coalesce its outbound side** — single-message bus semantics are preserved. Throttling lives in the frontend:

- `request()` (hot path): dedupes + drains after a ~50ms (20fps) window on rAF. The underlying mutable state is updated **immediately** (a synchronous read always sees the latest text); only the notification to React is coalesced.
- `flushNow()` (terminal/non-hot path): unconditionally flushes immediately and cancels a pending coalesced drain. `pushUserMessage`/`turn_complete`/`session_loaded` rely on this path — it **must not** be gated on a `dirty` flag, or they'd be silently dropped.
- Falls back to `setTimeout(fn,0)` when rAF is unavailable (jsdom/SSR-safe) — a production hardening that is also test-friendly.

## 7. AppFrame — concession geometry (desktop) + overlay drawer (mobile)

`src/layout/AppFrame.tsx` + `.module.css`. Mirrors dsh's `AppFrame` + `computeColumns`:

- **Desktop**: a 3-column grid `sidebar | gutter | center | details`; column widths come from the JS solver `computeColumns(width, prefs)`, and the column count MUST match the rendered child count (a stray gutter shoves center into a 0px column). Concession order: collapse details to its min→0 (auto-close), then collapse sidebar to a 56px rail, and only as a last resort let center drop below its 640 floor. Dragging the gutter changes `sidebarWidth` (sticky prefs, survives re-mounts).
- **Mobile** (`width ≤ --oneai-mobile-breakpoint:900px`): abandons the grid for a single column + top bar (hamburger + title + quick theme toggle) + sidebar as a `position:fixed` overlay drawer (`transform: translateX` slide-in) + scrim. Details is hidden (a secondary panel). Drawer state is controlled by App, so picking a session/scenario auto-closes it; an effect closes it when leaving the mobile width to avoid a stale overlay. Esc / scrim-click closes. Touch targets use `--oneai-touch-target:44px` (enlarged only inside the mobile breakpoint; desktop density is untouched).

## 8. Scenario conversations + group-chat speaker routing

The scenario editor `src/scenario/` (`ScenarioEditor`, React version; `scenario/validate`/`upsert`/`delete` through `ScenarioListStore`, the single transport-agnostic source of truth) + 5 bilingual presets. `compile.ts` compiles a `BusScenario`+topic values into the engine `BusGroupScenario`: it appends a topic-background block into each member's `system_prompt` per `visible_to`, and the title is "scenario·topic-values".

Group flow: `group/start`→`group/open` (opener speaks first)→`group/run` (each round)→`speaker_turn` (round boundary; finalizes the previous speaker's node)→`stream_chunk` carrying the `speaker` id (the frontend routes bubbles by member name/color)→`turn_complete`. The Debrief button appears when the scenario has a `debrief` configured and not yet triggered.

> **W5 trade-off**: the VS Code / browser extensions were **not** migrated to the shared React version this round; they still use vanilla `platforms/shared/scenario-editor.js` (synced by `sync.sh`). The design-doc risk table itself says "long-term unify to React"; this round avoids webview CSP/bundle risk.

## 9. Settings / probe RPC / provider pooling

`SettingsRoot` (General/Models/Permissions) + `SkillsModal` + `DomainPackModal` go through `SettingsStore`→`config/get`·`provider/list`·`domainpack/list`·`skill/list`·`config/read` (the `AppProbe` trait threads through `serve_all`). Full live provider management: `provider/add`·`delete`·`set_active` mutate `ProviderPool`'s `Arc<RwLock<Vec>>` (snapshot mode — clone out, drop the guard before await); active_index switches live without rebuilding App. Architecture red line: DomainPack hot-switch needs a `--domain` restart (`App.domain_pack` is an immutable Arc; no pooling bypass exists).

## 10. Attachments / deliverables / message feedback

- **Attachments** (W4): Composer drag-drop/paste/📎→base64 image `ContentBlock`→`turn/run`'s `content` flows straight to the engine.
- **Deliverables** (W4): `ToolOutput.artifacts` (populated by `write_file`/`apply_patch`) → at `turn_complete` the turn's tool artifacts are aggregated → attached to the turn's last assistant text node (DeliverableStrip).
- **Message feedback** (W4): `FeedbackStore` trait + SQLite `message_feedback` table; `feedback/submit`·`feedback/list` synchronous CRUD (mirrors `session/list`, not bus-routed). The feedback key is the node's runtime `turn_id`; **feedback is open only for fresh model output in the current conversation** — replayed history-session nodes have `turnId === null` → no 👍/👎 shown (by design: feedback is a reaction to a fresh output, not a retroactive mark on stored history).

## 11. Dark tokens + responsiveness

`src/theme/tokens.css`: `--oneai-*` in two sets (`:root` light / `body[data-oneai-theme='dark']` dark redefinition). Components **use only tokens, never hardcoded colors**. `readInitialTheme` (`src/theme.ts`, unit-testable): an explicit localStorage choice wins (`explicit=true`, stops following); otherwise the OS `prefers-color-scheme` is used and `explicit=false` — App's `matchMedia` change listener keeps following the OS until the user manually toggles (explicit-choice-wins semantics). The mobile breakpoint `--oneai-mobile-breakpoint` + touch target `--oneai-touch-target` are in §7.

## 12. Test matrix

Two layers, both in `platforms/web/`, via `npm run test` / `npm run e2e`:

- **vitest** (logic + components, jsdom): `src/stream/coalescer.test.ts` (20fps/flushNow/dirty), `src/scenario/compile.test.ts` (topic compilation/visible_to), `src/store/projection.test.ts` (feed yield sequences → assert ChatNode tree/approvalQueue/trajectory/deliverables, driving `consume(y)` directly with fake timers + `advanceTimersToNextFrame` to drain the rAF), `src/theme.test.ts` (readInitialTheme's three branches). `ProjectionStore.consume` is therefore public as a test seam.
- **Playwright e2e** (mock-ws end-to-end): `playwright.config.ts`'s `globalSetup` starts a `ws` server (`e2e/mock-server.ts`, :8788) speaking the same JSON-RPC, replaying scripted yields (`e2e/fixtures/scripts.ts`) instead of running an engine — deterministic, Rust-free. `webServer` runs `vite` (`VITE_APP_SERVER_URL` points at the mock). Cases: chat (streamed reply), approval (tool pause→respond→resume), theme (body attribute flip), responsive (375×812 hamburger + drawer). `fullyParallel:false` + `workers:1` (a single shared mock server).

## 13. Build

`npm run build` = `tsc --noEmit && vite build` (`dist/`). `npm run typecheck` is tsc alone. shiki language bundles are lazy-loaded; first paint loads only the conversation domain. The bundle is large (full shiki languages) — known, can be optimized later via `manualChunks`. npm devDeps do not pass the cargo supply-chain gate (`Cargo.lock`/`deny.toml` are unaffected); `npm audit` must be 0 (upgrading vitest to 4.x cleared the old esbuild transitive chain).

## 13.5. One-command launch (`oneai web`)

Mirrors deepseek-harness's `npx @deepseek-ai/dsh web`: **one command** lifts the engine + webUI + browser, no source, no extra processes. The engine process serves the SPA static dist **and** the `/ws` JSON-RPC upgrade on one port — `oneai-app-server`'s `serve_web` (feature `http`, axum 0.8 + `tower-http` `fs`) mounts `ServeDir` (with `index.html` SPA fallback) + `WebSocketUpgrade` on one router, and `serve_ws_axum` bridges the axum WebSocket to the existing `serve_connection` `mpsc<String>` seam (reuses all JSON-RPC handling, zero duplication).

- **Run**: `npx oneai-cli web` (npm package bundles the dist + fetches the platform binary on install) or `oneai web` (global/cargo install). Default `http://127.0.0.1:8787`, auto-opens the browser (`--no-open` skips; `--port/--host/--dist/--domain/--model/--user` configurable).
- **Self-contained ws URL**: when `VITE_APP_SERVER_URL` is unset, `App.tsx` derives it from the page origin `${wss?}://${host}/ws`, so the same built dist works on any host:port with no per-port rebuild; dev (`npm run dev` on 5173) can still override via env to point at a standalone app-server.
- **Dist source**: the web dist is platform-independent JS bundled into the `oneai-cli` npm tarball (`npm publish`'s `prepublishOnly` runs `platforms/npm/scripts/build-web.sh` to build + stage into `web-dist/`, gitignored); the launcher injects `ONEAI_WEB_DIST` to the package's dist. Cargo/binary users use `--dist` or auto-detect (`./platforms/web/dist` / `~/.oneai/web-dist`).
- **Reuse**: the CLI's `build_engine_server` (extracted from `cmd_app_server`, builds app+pool+stores+probe+pump) is shared by `cmd_web` and `cmd_app_server` — one engine-build path. `serve_web` builds its own `Dispatcher`+`subscribe_yields` (like `serve_all`).

## 13.6. Trajectory view (issue #40)

A resident center-column surface beside the conversation. The header-right button toggles the center between **chat** (reads "轨迹 / Trajectory") and the **trajectory timeline** (reads "对话 / Chat"); `/usage` switches to it too (it supersedes the old details-rail trajectory tab). The composer stays mounted so the user can keep sending while watching the timeline live-append.

- **Data**: `ProjectionStore` folds the trajectory-relevant `EngineYield`s into a `trajectory` ledger — per-iteration reasoning (the `infer` node, accumulated from `stream_chunk`/`thinking` and flushed into each `iteration_start` node, with the `inference` event attaching the API request/response + latency), `context_assembled` sections (hash-deduped against a per-key cache; the node sits **left of** its infer node — context is the input to inference), tool call↔result pairing (result + duration backfilled by `call_id`), approval requests (positioned left of the tool they gate), delegate DAG edges (`depends_on`), working-state snapshots, `interrupted`/`reflection`/`approval`/`error` markers.
- **Timeline model**: `trajectory/timeline.ts` folds the flat ledger into swim-lanes (lane 0 = main agent, one lane per delegated `task_id`) + fork/join/depends edges, positioned by wall-clock time. `turn_start`/`turn_complete` are **not nodes** — they fold into `turns` lane markers (start/end dashed lines). The SVG canvas pans (pointer drag) and zooms (wheel, cursor-anchored) and **defaults to fit-to-width** (the whole process visible in one screen, nodes enlarged to ~10px radius); "回到最新 / Back to latest" re-follows the latest node.
- **Detail pane**: clicking a node renders its type-specific detail (`DetailPane`) — context sections (with total-token/section/changed counts and `context:` source labels), reasoning/thinking + tokens + latency + the **API request/response** (sampling params, raw request messages, model response, usage/cache-hit), tool args/result + duration, plan checklist, delegate summary/key-findings, approval request, etc.
- **History replay**: the engine persists a whitelisted subset of yields per session (`FileSessionEventStore`, `<root>/events/{id}.jsonl`, tap-injected `ts`); `session/load` triggers `session/trajectory` and the projection replays those events through a trajectory-only path (chat nodes stay sourced from the message transcript).

## 14. Further reading

- `docs/webui-refactor-design.md` (+`_EN`) — the W1–W5 phased migration path and trade-off table.
- `docs/app-server-mechanism.md` (+`_EN`) — L2 JSON-RPC schema, Dispatcher, multi-transport.
- `docs/bus-mechanism.md` (+`_EN`) — L3 `Directive`/`EngineYield` semantics.
- `docs/cross-platform-mechanism.md` (+`_EN`) — the four native-frontend families and their relation to app-server (web is one of them).
