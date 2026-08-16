# OneAI WebUI Refactor Design (benchmarking deepseek-harness)

> This document is the refactor design for OneAI's end-user-facing "complete Web UI".
> Baseline: study how deepseek-harness (henceforth "dsh") implements its webUI, benchmark against OneAI's
> existing engine capabilities, split into "directly doable / missing-should-add / missing-not-advised",
> and fill in the **scenario-based conversation entry** that OneAI's engine already has but its Web frontend
> lacks — in the same UI style as dsh — to form OneAI's own WebUI.

---

## 0. At-a-glance

| Dimension | Conclusion |
|---|---|
| Benchmark target | dsh `packages/client` (browser half) + `packages/host` (host half); **not** dsh `website/` (VitePress docs site) |
| OneAI protocol base | `crates/oneai-app-server` (JSON-RPC 2.0 over stdio/ipc/ws/native-messaging, `ws` default-on) — the webUI integration surface, **ready** |
| OneAI existing web frontends | `platforms/browser` (extension popup) + `platforms/vscode` (webview) — both **simplified**, not standalone web apps |
| OneAI debug tool page | `crates/oneai-studio` (StateGraph/checkpoint/trace, **not via app-server**, single-agent, no scenarios) — reusable as a "trace viz component", **not a base** |
| dsh has, OneAI engine lacks | multi-session parallel preset composition format (`agent.cordis.yml`), Cordis slot system, DeepSeek onboarding |
| OneAI has, dsh entirely lacks | **scenario-based multi-role group chat** (`GroupChatSession` + `BusScenario` + `scenario/*` + `group/*` full stack) — OneAI's differentiator; the Web must make it a first-class entry |
| Recommended stack | React 18 + Vite + TypeScript + CSS Modules + design tokens (`--oneai-*`), state via `useSyncExternalStore` external store, connect to app-server WebSocket |
| First deliverable | New `platforms/web/` SPA, reuse app-server ws transport + the single-source-of-truth approach of `platforms/shared/scenario-editor.js` |

---

## 1. dsh webUI design analysis

### 1.1 Stack
React 18.2 + Vite 6, client-only SPA. `apps/web` is not a standalone dev server — `rejectStandaloneServe()` throws in `serve` unless `dsh web` injects `window.__DSH_BOOT__`. State: external store (Cordis `ctx` singletons + `HostObservable` projection snapshots + `useSyncExternalStoreWithSelector` via `bindSnapshotSelector()`). Styling: CSS Modules + `--dsw-static-*` tokens, light/dark. Markdown: incremental React renderer over micromark/mdast; single shiki highlighter with sync JS regex engine (no oniguruma WASM), lazy-loads non-core languages.

### 1.2 Layout & shell
Three-column grid `sidebar | center | details` (`AppFrame`), inline `gridTemplateColumns`, drag-resizable. Pure-function `computeColumns` concession solver: center never below `CENTER_MIN=640`; shrink details to 300 then auto-close; sidebar never yields (collapses to 56px rail); `<1024` auto-collapse. `AppWebEntry` two-phase boot (parse manifest → loading page → prefetch `immediately` plugins → Cordis Loader → per-node entry → full-active sweep → flip `settled`). Shell-self-sufficiency rule: loading page depends on no plugin. Slot system (`ui-slots`): `SlotMap` declaration-merge, single `register`, dispatch single/keyed/list/chain with shadowing + entry-boundary crash reporting.

### 1.3 Conversation `ui-conversation`
`ConversationRoot` resident skeleton. `ChatView` stable keyed parent list + pagination + bottom-follow (`FOLLOW_THRESHOLD=24`); each `ChatNodeSeat` subscribes to one Node key so Assistant delta / Tool lifecycle only re-renders its own row (no remount). Node dispatch via `conversation-nodes/` registered to `conversation.chat.node`. `AssistantMarkdown` renders by block: text→MarkdownText, reasoning→ReasoningRow (collapsed by default), image→ImageGallery, tool-call→grouped into tool rows, default→JsonBlock. `ui-tool`: `ToolCallTree` recursive root/subcall + `ToolDetails` in details rail + built-in toolview atomic cards (bash/read/file-mutation/search/web/todo/ask-question) + card models (diff/read/search/terminal/web). Streaming: `streaming` flag threaded to `MarkdownText`, throttled visual update, turn-tail loading dots. Approval: `ApprovalPanel` composer-takeover (amber bar + reason + command + refuse/allow). Attachments/deliverables/user-questions/message-feedback/input-trigger/permission-presets/model-selection all embedded in stream or composer seat.

### 1.4 Sidebar `ui-sidebar`
`SidebarRoot` owns only column geometry: logo row (doubles as New Session / collapse toggle) → New Session → `sidebar.workspaces` slot (occupant `ui-workspace`: multi-level tree / search / grouping / state dots) → footer + settings trigger. Collapse animation freezes content inline then slides to 56px rail; scrollbar hides 2s after pointer leave.

### 1.5 Settings + schema-form
`ui-settings-general` `SettingsRoot`: centered modal (1080×700) + left section nav rail (models/agent-presets/plugins/default). Close via button/mask/Esc. `schema-form` is **not a renderer** — it's a schema/draft model layer (`rehydrateSchema`/`setPath`/`validateDraft`); editors render their own controls.

### 1.6 Capability panels
Most capabilities are **not** standalone side panels — they embed in the conversation stream or composer seat: plan (composer chip), goal (composer-top dock bar), skill (toolview row + disclosure), subagent (session-header catalog), workflow-run (chat Node + member disclosure), jobs (session-header list), trajectory (**a tab in conversation view**, pure-consumer), details (selected tool args/result).

### 1.7 Workspace / directory-picker
`ui-workspace`: empty-state + sidebar slot; adding a workspace = pick one host dir. `ui-directory-picker-browse`: in-app **Miller two-pane** browser. `ui-directory-picker-native`: renderless OS chooser.

### 1.8 Connection & runtime
client↔host via `packages/client/connection` (fixture or HTTP/WS, shared `IApiClient`, `ctx.connection`). Host: `webserver` (`node:http` + route/upgrade registries + index transform taps, serves no files) + `frontend-static` (SPA dist fallback, injects `window.__DSH_BOOT__`) + `apiproxy` (JSON-RPC over HTTP/WS, per-domain files). SDK `packages/sdk/{protocol,client,server}`.

### 1.9 Key conclusion — dsh's "scenario conversation entry"

**dsh has NO multi-role group-chat / scenario-preset concept. Its `preset` is per-session single-agent composition, not multi-agent scenario chat.**

Evidence: `packages/preset/README.md` defines a preset as one `agent.cordis.yml` (tools+prompt+persona for one agent). Shipped presets (`standard/code/minimal/cordis`) are all single-agent coding-assistant configs. `ui-agent-preset` is single-preset select/manage only — no "pick a scenario → enter multi-role conversation" entry. Global grep for `group.chat|speaker|debrief|multiParticipant` hits only test fixtures or `turnOrder` (single-agent turn ordering). dsh's "multi-agent" = one process running multiple **independent** sessions (each its own preset), NOT multiple roles taking turns within one session.

**Implication**: OneAI's scenario group chat is a differentiator dsh entirely lacks. Making the "scenario conversation entry" a first-class Web entry is the core of what distinguishes OneAI's webUI from dsh's.

### 1.10 Design language
`--dsw-static-*` tokens, DeepSeek blue + neutral/bluish greys + amber/green/red semantics, light/dark. Font stack CJK-friendly (`-apple-system, …, 'PingFang SC', 'Microsoft YaHei'`); code font deliberately drops bare `monospace` to avoid Windows CJK fallback to SimSun. Custom SVG icon set (size suffixes) + `BrandWordmark`. Motion `cubic-bezier(0.4,0,0.2,1)`, fast 0.1s / default 0.2s / slow 0.3s. Interaction patterns: approval=composer-takeover amber bar; tools=recursive tree + per-tool atomic cards + details full args/result; streaming=incremental markdown + throttled + tail loading dots; commands=`/` catalog + `/`/`@` detection + `PopupSelectView`; reasoning=summary row expand; overlay=Modal/RiskConfirmation/HoverCard/Tooltip/Toast/Menu.

---

## 2. OneAI current state

### 2.1 Web/frontend assets
- `platforms/browser`: MV3 extension popup (chat + scenario editor, via app-server **native-messaging**). Not a standalone web app.
- `platforms/vscode`: webview + scenarios sidebar (via app-server **stdio**). Not a standalone web app.
- `platforms/shared/scenario-editor.js`: transport-agnostic single source of truth (248 lines), `sync.sh` mirrors copies.
- `crates/oneai-app-server`: JSON-RPC 2.0 + stdio/ipc/ws/native-messaging. ✅ **Protocol base, ready.** `ws` default-on (`default=["ws"]`), `serve_ws` + integration test `ws_transport_roundtrips_turn_run` verify browser WebSocket access. **Most direct webUI integration point.**
- `crates/oneai-studio`: debug tool page (StateGraph/checkpoint/trace), single-agent, **not via app-server** (own REST `/api/*` + `/ws`), `studio.js` grep `speaker|scenario|group` = 0 hits. Reusable as a viz component, not a base.
- No `platforms/web`; no standalone SPA. Browser/VS Code frontends are vanilla JS (no componentization, no design system, no StreamCoalescer).

### 2.2 Scenario group-chat engine (core) — fully built, 4-end UI aligned
- Engine: `GroupChatSession` in `oneai-agent`; `GroupChatBusObserver` turns group turn events into speaker-tagged bus yields (`SpeakerTurn` + `speaker`-tagged fragment yields), single `TurnComplete` per round.
- Bus (`crates/oneai-bus/src/protocol.rs`): `Directive::{StartGroupChat, GroupStart, GroupUserMessage, GroupSetScriptedOrder}` (693-706, `#[non_exhaustive]`); `EngineYield::{StreamChunk{turn_id,text,speaker}, SpeakerTurn, Thinking, ToolCalls, ToolResult, ApprovalRequest, ParadigmSwitch, PlanUpdate, WorkingState, TokenUsage, TurnComplete, …}` (~39 kinds). Models: `BusGroupScenario` (launch payload, :199) + `BusScenario` (editor-rich, :421, adds id/name/icon/topic_fields/debrief) + `BusScenarioMember`/`BusTopicField`/`BusDebriefConfig`/`BusReviewLoop`/`BusLocale`. **Single authoritative validator**: `BusGroupScenario::validate()`(:249)/`BusScenario::validate()`(:453) — called by `scenario/validate` RPC and all frontend editors, eliminating per-frontend mirror drift. `to_group_scenario()`(:594) drops UI-only fields at compile time.
- Scenario RPC (app-server `adapter.rs:296-376`): `group/start`→`StartGroupChat{scenario}`, `group/open`→`GroupStart`, `group/run`→`GroupUserMessage`, `group/set_order`→`GroupSetScriptedOrder` (bus, ack-only, results stream as events). `scenario/list`/`get`/`upsert`(validate-first)/`delete`/`validate` (**sync CRUD, not bus**). customs server-authoritative + presets local: id `preset-*`=local preset (server `preset-*` seeds ignored so richer local presets win); non-preset=custom (sidecar mode `scenario/upsert` to server then mirror local).
- 4-end UI aligned: macOS SwiftUI (`AgentStore.swift` 628L 5 bilingual presets + `ScenarioEditor.swift` + `ChatViewModel.swift` 2027L w/ StreamCoalescer 20fps + speaker routing + debrief), Windows WinUI3/C# mirror, VS Code + browser (shared `scenario-editor.js`). FFI: macOS/Android c_facade 3-symbol pump; Windows fine-grained P/Invoke (does **not** use app-server).

### 2.3 app-server protocol surface
All JSON-RPC methods: `turn/run`(blocking-ack↔TurnStart), `turn/cancel`, `approval/respond`, `paradigm/switch`, `config/update`, `session/create`/`load`/`clear`/`delete`(blocking-ack/ack), `session/list`(sync), `conversation/compact`, `project/init`, `group/start`/`open`/`run`/`set_order`(bus), `scenario/list`/`get`/`upsert`/`delete`/`validate`(sync), `shutdown`; outbound only `event` (params = whole `EngineYield` with `kind` tag). New yield kinds are transparent to old frontends.

### 2.4 Streaming
app-server outbound: each `EngineYield` → `event` notification → per-connection forwarder. Transports: stdio/ipc=newline-JSON, ws=one text frame per msg, native-messaging=4B-LE length-prefix. macOS `StreamCoalescer` ~20fps batches hot fragments. **Web currently has no coalescer** (browser ext `chat.js` per-token `createTextNode`) — must add (frontend batching, NOT in app-server outbound, to preserve single-message bus semantics). No SSE — streaming uniformly via `event` over WebSocket.

### 2.5 WebUI degree built
Built/usable: browser extension + VS Code (simplified chat + scenario editor + speaker routing + approval) via app-server; ws transport default-on. Missing: no standalone web app, no product-grade conversation+sidebar+settings+scenario full form, no Web StreamCoalescer, no Studio-grade trace/checkpoint integration in product webUI, Windows FFI experience not portable to web.

---

## 3. Benchmark matrix

Legend: 🟢 directly doable (engine/protocol ready, Web just needs components) · 🟡 missing-should-add (gap worth filling) · 🔴 missing-not-advised (dsh-bound / misfit / low ROI).

### 3.1 🟢 Directly doable
| UI capability | dsh pkg | OneAI engine/protocol | Path |
|---|---|---|---|
| 3-col layout | `ui-layout` | — | new `platforms/web`, mirror column-concession geometry |
| Session sidebar | `ui-sidebar` `ui-workspace` | `session/list` `create` `load` `delete` | subscribe + cache |
| Conversation stream (md/code/thinking/streaming) | `ui-conversation` | `StreamChunk`/`Thinking`/`TurnComplete` | React incremental md + **frontend StreamCoalescer 20fps** |
| Tool tree + per-tool cards | `ui-tool` | `ToolCalls`/`ToolResult` | keyed node tree + diff/read/search/terminal/web cards |
| Approval (composer-takeover) | `ApprovalPanel` | `ApprovalRequest` + `approval/respond` | amber bar + refuse/allow + parallel approval queue (memory `issue20`) |
| Model selection | `ui-model-selection` | SmartRouter + provider pool | two-level menu; hot-switch later |
| Command palette / slash | `ui-commands` `ui-input-trigger` | client-maintained directory | `/` `/scenario` `/model` `/permission` |
| Settings modal | `ui-settings-*` | provider config + DomainPack (`--domain`) | General/Models/DomainPacks/Permissions |
| Plan mode control | `ui-plan` | `paradigm/switch` + `PlanUpdate` + `config/update` | composer chip + `/plan` |
| Goal bar | `ui-goal` | working-state (goal/steps/decisions/blockers) | composer-top dock, `WorkingState{event}` |
| Subagent catalog | `ui-subagent` | `Delegate`/`DelegateComplete` | session-header drawer + `@` |
| workflow-run node | `ui-workflow-run` | `PlanUpdate` (StateGraph) | chat Node + member disclosure |
| trajectory tab | `ui-trajectory` | `IterationStart` + bus event stream | conversation tab, reuse Studio D3 renderer |
| Attachments / drop | `ui-attachment` | attachment tool | draft rail + message image + lightbox |
| Deliverables | `ui-deliverables` | file tool output | turn-tail produced files |
| User questions / plan review | `ui-user-questions` | `ApprovalRequest`(McpElicitation/PlanReview) | `QuestionComposer` + `PlanReviewPanel` |
| Permission presets | `ui-permission-presets` | `PermissionProfile` (DomainPack layer 3) | `/permission` popup + General row |
| Message feedback | `ui-message-feedback` | feedback capability | per-message action strip |
| **Scenario conversation entry** (§5) | **dsh has none** | `group/*` + `scenario/*` + `BusScenario` (**engine complete**) | **OneAI differentiator, first-class** |
| Design system | `ui-theme` `ui-primitives` | — | `--oneai-*` tokens + custom SVG icons |
| Directory browser (Miller) | `ui-directory-picker-browse` | workspace (sidecar-cwd) | pick sidecar cwd / project root |
| Cross-session resume | — | `session/list` surfaces unfinished work | sidebar "Unfinished Work" group |

### 3.2 🟡 Missing, should add
| Capability | Gap | Advice |
|---|---|---|
| Web StreamCoalescer | browser ext per-token | frontend 20fps batch (as macOS), NOT in app-server outbound |
| Provider config RPC | app-server configures at startup (`cmd_app_server.rs:337`) | add `provider/add`/`list`/`test` (or settings file + restart); first version read-only + hot-switch existing |
| DomainPack online select/manage | startup `--domain` | `domainpack/list`/`switch` (reuse `PackRegistry`) |
| Skill online manage | engine `SkillCurator`/lifecycle, no Web entry | `skill/list`/`pin`/`archive` + UI (align macOS) |
| Scenario editor componentization | current vanilla single file | rewrite React, keep transport-agnostic single source of truth |
| Trajectory/StateGraph viz | Studio has D3 but not via app-server | extract `graph-render.js` as reusable module over `EngineYield` |
| i18n framework | OneAI has `AppLocale` (zh/en), no Web system | locale store + `t()`, drive re-render (as dsh `LocaleFace`) |
| HMR/dev | none | Vite dev + app-server ws, resolve alias to src |
| Sandboxed egress viz | `NetworkApprovalMode` + `HostAllowlistStore` engine-side | `ApprovalRequest`(NetworkApproval) already via unified approval channel; approval card shows host + allow-once/always/deny |
| Usage viz | `TokenUsage` | footer / settings panel |

### 3.3 🔴 Missing, not advised
| Capability | Why not |
|---|---|
| Cordis slot system (`ui-slots`) | dsh hard-depends on vendored Cordis; OneAI is a Rust workspace with no JS plugin runtime. Plain React component tree + routing suffices |
| `agent.cordis.yml` preset format / roster / composition | dsh preset = single-agent tools+prompt; OneAI equivalent is **DomainPack** (declarative 7-layer, Rust) + **scenario BusScenario** (multi-role), not YAML files. Replicating dsh preset system conflicts with OneAI architecture |
| Runtime module system / plugin bundle fetch (`packages/client/modules`) | same — OneAI has no JS plugin distribution need |
| Typert RPC gateway / BFF assembly (`packages/api`, `packages/typert`) | OneAI protocol layer IS app-server JSON-RPC; no type-graph codegen + BFF middle layer needed |
| `schema-form` render framework | dsh's is a schemastery-envelope draft model; OneAI settings are finite and fixed-shape — hand-written React forms + `scenario/validate` suffice |
| DeepSeek onboarding dialog / fish logo | vendor-specific → OneAI branding |
| `node:http` webserver + index transform taps (`packages/host/webserver`) | OneAI static assets served by Vite dev / simple static server; no boot-manifest injection needed |
| `__DSH_BOOT__` manifest + fail-loud active sweep | dsh needs it for runtime plugin loading; OneAI is build-time static bundle, standard Vite entry suffices |
| Browser extension as product form | extension exists (simplified); SPA can be wrapped by extension, but **not the primary product** |
| Multi-process parallel independent-session orchestration UI | dsh needs session-plane orchestration because one process runs many independent sessions; OneAI single app-server already supports many sessions — sidebar list is enough |

---

## 4. OneAI WebUI overall design

### 4.1 Architecture
```
platforms/web/ (new, React 18 + Vite + TS SPA)
  App — 3-col AppFrame (sidebar|center|details)
   ├─ SidebarRoot (session list / scenario entry / settings)
   ├─ ConversationRoot (chat + composer + approval)
   └─ DetailsPanel (tool args/result / trajectory)
       │ useSyncExternalStore over ws event
       │ JSON-RPC 2.0 (ws, default-on)
crates/oneai-app-server (ready)
  serve_ws ←─ spawn_yield_forwarder → event notification
  turn/approval/session/group/scenario/*
  SharedScenarioStore (FileScenarioStore ~/.oneai/...)
       │ Directive / EngineYield (bus)
oneai-agent AgentLoop + GroupChatSession + features
```
Key decisions: (1) stack aligned with dsh (React+Vite+TS+CSS Modules+tokens) — proven to support ~40 UI domains; (2) state via `useSyncExternalStore` external store — don't replicate Cordis, but replicate its "projection snapshot + selector hook" pattern; one `OneAiRpcClient` (ws) holds the event stream, accumulates `EngineYield` into per-session projection snapshots (`getSnapshot`+`subscribe`); (3) protocol directly to app-server ws — no BFF/Typert middle layer; outbound only `event`, new yield kinds transparent; (4) no slot system — plain React tree + context providers, domains split by dir + lazy import; (5) scenario-editor single-source-of-truth preserved — Web scenario editor as transport-agnostic React component (takes `rpc.call()`), reusable by VS Code/browser.

### 4.2 Layout
Mirror dsh three-column + concession geometry (OneAI constants): sidebar 256–420px (→56px rail), center floor 640px (never yields), details 300–520px (auto-close). `<1024` auto-collapse sidebar; overlay slot for modals/approval/command palette.

### 4.3 Component tree (OneAI-ized dsh division)
```
platforms/web/src/
  app/            AppFrame / app-shell / boot
  rpc/            OneAiRpcClient (ws JSON-RPC) + projection store + useSyncExternalStore bind
  theme/          --oneai-* tokens + dark + i18n
  primitives/     React primitives + markdown renderer (micromark+shiki) + icons
  conversation/   ConversationRoot / ChatView / AssistantMarkdown / ToolCallTree / ApprovalPanel
                  / StreamCoalescer (20fps) / composer
  sidebar/        SidebarRoot / SessionList / ScenarioEntry / WorkspacePicker
  details/        DetailsPanel / ToolDetails / TrajectoryTab
  settings/       SettingsRoot (modal) / General / Models / DomainPack / Permissions
  commands/       CommandPalette / InputTrigger / PopupSelectView
  scenario/       ScenarioEntry (first-class) / ScenarioEditor / ScenarioPicker (§5)
  attachment/     AttachmentRail / ImageLightbox / DropOverlay
  trajectory/     TrajectoryView (reuse Studio graph-render)
```

### 4.4 Design tokens (`--oneai-*`)
Mirror dsh `design-platform.css` structure, OneAI brand color: brand color + neutral greys + amber/green/red semantics, light/dark (`body[data-oneai-dark-theme]`). Same CJK-friendly font stack; code font drops bare `monospace`. Motion `cubic-bezier(0.4,0,0.2,1)`. Frozen geometry constants in one place.

### 4.5 Streaming & rendering
- **StreamCoalescer** (Web gap-fill): batch hot fragments ~20fps flush, `.complete`/`.error` immediate flush, `requestAnimationFrame` scheduling — avoids per-token setState flooding React. Same semantics as macOS `ChatViewModel`.
- Incremental markdown over micromark/mdast (own impl to avoid Cordis dep); shiki sync-regex highlighter, core langs at boot, rest lazy.
- Keyed chat nodes: each row subscribes one Node key (critical perf, as dsh `ChatNodeSeat`).
- Speaker routing (group scenarios): `StreamChunk{speaker}` drives bubble switch; on speaker change mark old item done + seed new (as macOS `ChatViewModel.handleSidecarEvent`).

### 4.6 Command palette / slash
Client-maintained directory (no engine protocol): `/new` `/sessions` `/compact` `/clear`; `/model` `/permission`; `/plan` `/reflect` `/explore`; **`/scenario` `/new-scenario` `/edit-scenario`** (§5); `/workspace`; `/settings`. `PopupSelectView` shared by `/model` `/permission` `/scenario`.

---

## 5. Scenario conversation entry design (OneAI differentiator, first-class)

> This is what dsh **entirely lacks** and OneAI's engine **fully has**. Making it a first-class Web entry in dsh's UI style is the core of OneAI's webUI distinction.

### 5.1 Entry points (three paths, reusing dsh interaction patterns)
1. **Sidebar "Scenarios" section** (align dsh `sidebar.workspaces` slot + `ui-workspace` workspace-pick UX): below New Session, list local presets (`preset-*`) + custom scenarios, each with icon + name + blurb. Click → full flow **topic intake → opener → multi-role chat → debrief** (as macOS `newConversation(scenario, topicValues:)`).
2. **new-session hero chip** (align dsh `ui-agent-preset` `AgentPresetSeat` staged-select UX): empty-session hero shows scenario chips ("Interviewer × Candidate", "Debate", "Roundtable"), staged-select fills composer hint "pick a topic to enter scenario X".
3. **Slash commands** (align `/model` `/permission`): `/scenario` opens picker `PopupSelectView`; `/new-scenario` opens editor; `/edit-scenario` edits current.

> **Do NOT replicate dsh's `agent.cordis.yml` preset system** — dsh preset is single-agent composition; OneAI scenario is `BusScenario` multi-role orchestration (members + turn_policy + topic_fields + debrief). Data model = `BusScenario`, validation via `scenario/validate`, storage via `scenario/upsert`/`list`/`delete`, **shared authority with macOS/Windows/VS Code/browser**.

### 5.2 Scenario full-flow UI (align macOS/Windows native, Web-ized)
```
[Scenarios section] ─pick─▶ [ScenarioPicker] (scenario/list: local presets + server customs)
                              │ (topic_fields, visible_to controls visibility)
                              ▼
                       [TopicIntake]  (bake topic_values into member system_prompts per visible_to)
                              │  group/start (BusGroupScenario) → group/open (opener)
                              │  EngineYield: SpeakerTurn + StreamChunk{speaker}
                              ▼
                       [GroupChatView]  (speaker-routed bubbles + StreamCoalescer 20fps)
                              │  (turn_policy: scripted/round-robin/moderator;
                              │   review_loop: reviewer_id/approve_marker/max_rounds)
                              ▼
                       [Debrief]  (group/set_order narrows to debrief_member_id; summary_prompt → single-member summary)
                              ▼
                       [Summary]  → can archive as new custom scenario
```

### 5.3 Scenario editor (React-ized, single source of truth preserved)
Rewrite `platforms/shared/scenario-editor.js` (248L vanilla) as React, **keep transport-agnostic contract** (takes `rpc.call()`, not bound to ws/stdio):
- Fields: `id`/`name`/`icon`/`members[{agent_id,role,system_prompt}]`/`turn_policy`/`script_order`/`moderator_id`/`opener_agent_id`/`opener_line`/`topic_fields[{id,label,type,visible_to}]`/`debrief{button_label,summary_prompt,debrief_member_id}`/`review_loop`/`locale`.
- Live validate: debounce `scenario/validate` (returns `{ok, errors:[{field,code,message}]}`), localize via `ScenarioErrorLocalizer` (as macOS).
- Save `scenario/upsert` (validate-first); delete `scenario/delete`. Presets (`preset-*`) read-only.
- Reuse: VS Code/browser keep vanilla, or upgrade to React (shared `BusScenario` type + validate call). Long-term unify to React, `platforms/shared/scenario-editor.{js→tsx}` single source.

### 5.4 Scenario presets
Mirror macOS `AgentStore.presets(locale:)` 5 bilingual presets (zh/en), Web generates local presets (`preset-*` id); `scenario/list` merge rule `localPresets + serverCustoms` (server `preset-*` seeds ignored). Examples: Interviewer×Candidate (tech), Debate (pro/con/moderator), Roundtable (experts+moderator), Code Review (author/reviewer/moderator), Brainstorm (multi-role+moderator).

### 5.5 Speaker routing & streaming
`StreamChunk{turn_id,text,speaker=Some(member_id)}`: `speaker` drives bubble switch; on speaker change mark old done + seed new (as macOS `handleSidecarEvent`). `SpeakerTurn` = round boundary separating speaker blocks. Group turn events via `GroupChatBusObserver` → speaker-tagged yields, single `TurnComplete` per round — Web consumes directly, no special protocol.

### 5.6 Debrief
End-of-scenario "Summarize" button (as macOS `endScenarioDebrief`) → `group/set_order` narrows to `debrief_member_id` → `runGroupTask(summary_prompt)` → single-member summary. Optional: archive summary as new custom scenario (`scenario/upsert`).

---

## 6. Migration path & phasing
- **Phase W1 — skeleton & protocol**: new `platforms/web/` (Vite+React+TS+CSS Modules); `OneAiRpcClient` (ws) + projection store + `useSyncExternalStore`; AppFrame 3-col + `--oneai-*` tokens + dark + i18n skeleton; wire `turn/run` + `event`: single-agent chat + streaming markdown + StreamCoalescer 20fps. Gate: single-agent streaming chat, no long-stream stutter.
- **Phase W2 — conversation domain + sidebar**: `ChatView` keyed nodes + `AssistantMarkdown` (micromark+shiki) + `ToolCallTree` + `ApprovalPanel` (parallel approval queue); `SidebarRoot` + SessionList (`session/list`/`create`/`load`/`delete`) + cross-session resume (working-state "Unfinished Work"); composer + command palette (`/model` `/permission` `/plan`) + Plan chip. Gate: full single-agent workflow, approval/tools/plan all pass.
- **Phase W3 — scenario conversation entry (differentiator, first-class)**: `ScenarioEntry` (sidebar section + hero chip + `/scenario`) + `ScenarioPicker`; `ScenarioEditor` (React, `scenario/validate`/`upsert`/`delete`, transport-agnostic single source); `TopicIntake` → `group/start`/`open`/`run` → `GroupChatView` (speaker routing + StreamCoalescer) → `Debrief`; 5 bilingual presets + local/server merge. Gate: pick scenario→topic→multi-role chat→debrief full flow, behavior aligned with macOS native.
- **Phase W4 — capability panels + settings + viz**: `DetailsPanel`/`ToolDetails` + subagent catalog + workflow-run node + goal bar + skill manage; `TrajectoryTab` (reuse Studio `graph-render.js` over `EngineYield`); `SettingsRoot` modal (General/Models/DomainPacks/Permissions/Plugins) + provider config RPC (new `provider/*`) + DomainPack online select (`domainpack/*`); attachments/drop + deliverables + message feedback. Gate: capability panels + settings complete, trajectory viz via app-server.
- **Phase W5 — polish & alignment**: pixel-align tokens + full dark coverage + responsive; upgrade VS Code/browser extensions to shared React scenario editor; e2e tests (cf. dsh `vitest.web.config.ts` + `stress-tests`); docs `docs/webui-mechanism.md`(+`_EN`).

---

## 7. Risks & tradeoffs
| Risk/tradeoff | Note | Mitigation |
|---|---|---|
| React bundle size | ~40 domains | route-level lazy import; lazy shiki; first paint only conversation domain |
| No slot system | lose runtime plugin loading | OneAI has no JS plugin need; build-time static bundle suffices; split domains via lazy import |
| Scenario editor dual version | short-term vanilla (ext) + React (web) coexist | share `BusScenario` type + `scenario/validate`; long-term unify React |
| Provider hot-add | app-server configures at startup | Phase W4 new `provider/*` RPC or settings file + restart |
| Windows FFI not portable | Windows uses P/Invoke not app-server | Web benchmarks against macOS sidecar path (both app-server), ignore Windows FFI |
| Studio not via app-server | trajectory/StateGraph reuse needs a bridge | extract `graph-render.js` as pure render module over `EngineYield`, don't reuse Studio REST routes |
| Streaming bus semantics | no coalesce in app-server outbound | StreamCoalescer in frontend, preserve single-message bus semantics |

---

## 8. Appendix: dsh `ui-*` pkg ↔ OneAI mapping
| dsh pkg | OneAI / decision |
|---|---|
| `ui-layout` `AppFrame` | 🟢 adopt 3-col + concession geometry |
| `ui-slots` | 🔴 don't replicate (Cordis dep) — React component tree |
| `ui-primitives` | 🟢 adopt primitives + md/shiki (own impl, drop Cordis) |
| `ui-theme` | 🟢 adopt token structure → `--oneai-*` |
| `ui-conversation` | 🟢 core adoption (ChatView/AssistantMarkdown/ApprovalPanel) |
| `ui-sidebar` `ui-workspace` | 🟢 + scenario section |
| `ui-tool` | 🟢 ToolCallTree + per-tool cards |
| `ui-attachment` `ui-deliverables` | 🟢 |
| `ui-user-questions` | 🟢 QuestionComposer + PlanReviewPanel |
| `ui-message-feedback` | 🟢 |
| `ui-input-trigger` `ui-commands` | 🟢 |
| `ui-model-selection` | 🟢 |
| `ui-agent-preset` | 🔴 don't replicate preset system; borrow hero-chip staged-select UX for scenario entry |
| `ui-permission-presets` | 🟢 |
| `ui-plan` `ui-goal` `ui-skill` `ui-subagent` `ui-workflow-run` `ui-jobs` `ui-trajectory` | 🟢 all have engine counterparts |
| `ui-directory-picker-*` | 🟡 simplify (sidecar cwd / project root, Miller two-pane optional) |
| `ui-settings-*` | 🟢 (drop DeepSeek onboarding) |
| `schema-form` | 🔴 don't replicate — hand-written forms + `scenario/validate` |
| `web-react` (bind) | 🟢 borrow `useSyncExternalStoreWithSelector` pattern (drop Cordis) |
| `connection` | 🟢 borrow wire-client idea → app-server ws |
| `modules` | 🔴 don't replicate (no runtime plugin loading) |
| `host/webserver` `host/frontend-static` `host/apiproxy` | 🔴 don't replicate — app-server ws + static serve |
| **(dsh none) scenario entry** | 🟢 OneAI new first-class entry (§5) |
