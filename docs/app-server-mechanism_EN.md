# OneAI App-Server Mechanism

> `oneai-app-server` — the JSON-RPC 2.0 protocol layer between the engine and
> every **non-Rust frontend** (IDE plugin / web / TS-JS / desktop
> macOS-Swift·Windows-C#). One frontend protocol + multiple transports
> (stdio / unix-socket / named-pipe / WebSocket) listened concurrently; one
> engine process feeds all such frontends.

## 1. Overview

`oneai-app-server` is an **L2 adapter** sitting on top of `oneai-bus` (the L3
engine-level newline-JSON protocol). It maps a frontend-facing **JSON-RPC 2.0**
schema onto the underlying `Directive`/`EngineYield` semantics. The engine is
unaware — it sees the same `InProcessBus` whether the other end is an
in-process TUI or an out-of-process IDE/web/desktop.

Once non-Rust frontends arrive (from "one desktop" to "four classes"), unifying
onto a single JSON-RPC frontend protocol beats "desktop passthrough + each IDE
its own" — JSON-RPC reuses IDE/MCP tooling (`generate-ts`/`json-schema`), and
the operation-oriented API (`turn/run` has a return value) is naturally RPC.

## 2. Layering

- **L3 bus** (unchanged, `oneai-bus`): `Directive`/`EngineYield` newline-JSON +
  `InProcessBus`. Internal canonical protocol; the TUI connects in-process
  (zero serialization).
- **L2 app-server adapter** (this crate):
  - **inbound**: JSON-RPC method → `bus.submit(Directive)`. `turn/run`→`UserMessage`,
    `turn/cancel`→`Interrupt`, `approval/respond`→`Approve`, `session/*`→session
    directives, …
  - **outbound**: `bus.subscribe_yields()` → a single JSON-RPC `event`
    notification (`params` = the full `EngineYield`, with its `kind` tag).
  - **Dispatcher**: resolves "blocking-ack" requests (`turn/run`, etc.) —
    fulfills their oneshot when the matching later yield arrives.
- **L1 transports** (this crate): stdio (IDE LSP-style spawn) / ipc
  (`oneai-supervisor::IpcListener`, Unix=UDS / Win=named-pipe) / ws
  (`tokio-tungstenite` + `TcpListener`, browser WS handshake) — concurrent.
- **L4 engine** (unchanged): `spawn_directive_pump` → AgentLoop + `BusObserver`
  + `BusInteractionGate`. Built by the CLI (`oneai app-server`), which passes
  `Arc<InProcessBus>` to the crate's `serve_all`.

## 3. Process topology

```
oneai app-server --listen stdio --listen ipc://~/.oneai/app-server.sock --listen ws://127.0.0.1:8787
```

One process listens on three transport classes concurrently; an IDE plugin
spawns stdio, web connects WS, desktop connects ipc, TUI is in-process. **One
engine process feeds five frontend classes** (incl. TUI + mobile Shape A) →
crash isolation + single binary + signature decoupling (`.app`/`.vsix` thin
shells, `oneai` engine binary upgrades independently).

vs `oneai serve` (newline-JSON passthrough sidecar): `app-server` speaks the
JSON-RPC operation-oriented schema (IDE/MCP tooling-friendly); `serve` is raw
bus passthrough (escape hatch, still available). Separate sockets
(`app-server.sock` vs `serve.sock`) so both coexist.

## 4. JSON-RPC schema

`id` is `serde_json::Value` (supports `null`/str/num); notifications carry no
`id`. Hand-rolled envelope (not reusing `oneai-a2a`'s `JsonRpcRequest` — that's
`id:u64`, HTTP-only, no notifications, unsuited to a bidirectional stream).

### Inbound requests (have `id`, expect a response)

| method | params | → Directive | response |
|---|---|---|---|
| `turn/run` | `{content:[ContentBlock]}` | `UserMessage` | returns `{turn_id}` at TurnStart |
| `turn/cancel` | `{reason?:InterruptReason}` | `Interrupt` | ack `{ok:true}` |
| `approval/respond` | `{request_id,response:InteractionResponse}` | `Approve` | ack |
| `paradigm/switch` | `{to:BusParadigmKind}` | `SwitchParadigm` | ack |
| `config/update` | `{plan_mode?:bool}` | `UpdateConfig` | ack |
| `session/create` | `{id?:String}` | `CreateSession` | returns `{id}` at SessionCreated |
| `session/load` | `{id:String}` | `LoadSession` | returns `{id,messages}` at SessionLoaded |
| `session/clear` | `{}` | `ClearSession` | returns `{id}` at SessionCleared |
| `session/delete` | `{id:String}` | `DeleteSession` | ack (result via `event`) |
| `conversation/compact` | `{keep_recent_turns:usize}` | `Compact` | ack (result via `event`) |
| `project/init` | `{format?,force?,no_llm?}` | `InitProject` | returns `{message}` at InitResult |
| `group/start` | `{scenario:BusGroupScenario}` | `StartGroupChat` | ack |
| `group/open` | `{}` | `GroupStart` | ack |
| `group/run` | `{user_input:String}` | `GroupUserMessage` | ack |
| `group/set_order` | `{order:[String]}` | `GroupSetScriptedOrder` | ack |
| `scenario/list` | `{}` | — (sync CRUD, not a Directive) | `{scenarios:[BusScenario]}` |
| `scenario/get` | `{id:String}` | — | `BusScenario` (missing → `-32602`) |
| `scenario/upsert` | `{scenario:BusScenario}` | — | `{ok, id?}` (validates first; returns errors if invalid) |
| `scenario/delete` | `{id:String}` | — | ack |
| `scenario/validate` | `{scenario:BusScenario}` | — | `{ok, errors:[{field,code,message}]}` |
| `shutdown` | `{}` | `Shutdown` | ack |

- **`scenario/*` is pure shared-state CRUD — no Directive/bus** — it reads/writes the process-wide `ScenarioStore` and returns immediately. One shared scenario library (macOS / VS Code / browser edit the same `~/.oneai/scenarios.json`); `scenario/validate` is the single authoritative validator (kills the per-frontend client-side mirror drift). `BusScenario` is the rich editor unit (cast + turn policy + topic fields + debrief + review loop); the frontend compiles it to `BusGroupScenario` for `group/start` (see `BusScenario::to_group_scenario`).

- **"returns"** = the Dispatcher fulfills a oneshot when the matching yield
  arrives (strips the `kind` tag, returns only the fields); **ack** =
  `{ok:true}` immediately after `bus.submit` succeeds.
- **`turn/run` returns `{turn_id}` at TurnStart** (not blocking until
  TurnComplete) — streaming fragments still arrive as `event` notifications;
  the round ends via `turn/complete` (an `event` of kind `turn_complete`). No
  long-held request; turn_id known early.
- **`session/delete` / `conversation/compact` are ack, not "returns"** — the
  pump emits `EngineYield::Error` (not the result yield) on failure, which
  would hang a blocking-ack; the result (`SessionDeleted`/`CompactResult`/
  `Error`) arrives via `event` notification, the frontend branches on
  `params.kind`.
- Unknown method → `-32601`; submit failure → `-32603`; bad JSON → `-32700`;
  missing method → `-32600`; bad params → `-32602`.

### Outbound notifications (no `id`)

A single method `event`, `params` = the full `EngineYield` JSON (with `kind`
tag). The frontend branches on `params.kind` (`turn_start`/`stream_chunk`/
`tool_calls`/`approval_request`/`turn_complete`/`session_*`/…). New yield
variants (`#[non_exhaustive]`) arrive as unknown `kind`s a frontend ignores —
zero per-variant RPC method explosion; the protocol grows with the bus without
breaking old frontends. `approval_request`'s `request_id` lives in `params`;
the frontend replies with `approval/respond`.

## 5. Dispatcher — why a single consumer

The bus is a `broadcast`: every connection's yield forwarder sees **every**
yield. But "blocking-ack" request resolution is a **global FIFO** concern — if
each connection resolved its own, a single `TurnStart` would be popped by N
consumers. So one app-server process has **one `Dispatcher`**, shared across
all connections/transports, holding per-variant FIFO queues
(`pending_turns`/`pending_session_create/load/clear/delete`/`pending_compact`/
`pending_init`), drained by a **single** yield-consumer task.

The directive pump is **serial** (bounded mpsc drained one at a time) → yields
for a variant fire in submit order → FIFO-per-variant is correct. Subscription
happens **before** spawning (`serve_all`/tests both `bus.subscribe_yields()`
then spawn `run`), avoiding a lost-first-yield race.

## 6. Files & core abstractions

| item | location |
|---|---|
| crate doc + `ListenSpec` + `serve_all` + `AppServerError` | `crates/oneai-app-server/src/lib.rs` |
| JSON-RPC envelope (`Request`/`Response`/`Notification`/`RpcError`) + method constants + error codes + `decode_inbound` | `crates/oneai-app-server/src/protocol.rs` |
| `Dispatcher` (per-variant FIFO queues + single yield consumer) | `crates/oneai-app-server/src/dispatcher.rs` |
| `serve_connection` (outbound forwarder + inbound dispatch + method→Directive map) | `crates/oneai-app-server/src/adapter.rs` |
| transports: `serve_stdio`/`serve_ipc`/`serve_ws` + line/frame bridge | `crates/oneai-app-server/src/transport.rs` |
| CLI subcommand `oneai app-server` (build engine + parse `--listen` + `serve_all`) | `examples/cli/src/cmd_app_server.rs` |

## 7. Relationship to other frontend paths

| frontend | path | via app-server? |
|---|---|---|
| TUI (`examples/cli`) | in-process, `Arc<InProcessBus>` direct to L3 | no (zero serialization) |
| native macOS/Windows (desktop sidecar) | `oneai app-server --listen ipc://` JSON-RPC client | **yes** (migrating; old `OneAIBusClient` newline-JSON demoted to escape hatch) |
| IDE plugin (TS) | spawn `oneai app-server --listen stdio` JSON-RPC | **yes** (pending) |
| web/JS | `ws://` JSON-RPC | **yes** (pending) |
| mobile (iOS/Android/HarmonyOS) | in-process c_facade 3-symbol pump (Shape A) | no (on-device: no spawn, no cloud-engine fallback) |
| Feishu gateway | delivered via bus, attaches to the same daemon's L2 adapter | optional |
| A2A | separate process (protocol boundary, P5-C) | no |

## 8. Extension points

- **Add a frontend method**: add a `method::` constant + match arm in
  `adapter::handle_request` → submit the matching `Directive`; for blocking-ack,
  add a `pending_*` queue + `register_*` + a `dispatch` match arm in
  `Dispatcher`.
- **Add a transport**: add `serve_<x>` in `transport.rs` (bridge concrete bytes
  ↔ `mpsc<String>` to `serve_connection`) + a `ListenSpec` variant + a `parse`
  branch + a `serve_all` arm.
- **Custom frontend UI**: connect on any transport, send `turn/run`, render
  off `event`'s `params.kind`; on `approval_request`, take
  `params.request_id`, prompt, reply `approval/respond`.

## 9. Tests

`crates/oneai-app-server` (28 tests): unit (`ListenSpec` parse / envelope
round-trip / method→Directive map / Dispatcher FIFO + kind stripping) +
integration (mpsc-channel-driven `serve_connection`: `turn/run`→event stream +
turn_id response / approval roundtrip / `turn/cancel` fires token /
`session/create` / unknown method -32601 / bad JSON -32700) + WS e2e (real
ephemeral port + `tokio-tungstenite` client round-trip).

## 10. Further reading

- [bus-mechanism.md](bus-mechanism.md) — the L3 engine bus (the canonical
  protocol beneath this layer)
- [cross-platform-mechanism.md](cross-platform-mechanism.md) — desktop sidecar
  vs mobile Shape A vs TUI in-process
- [cli-reference.md](cli-reference.md) — the `oneai app-server` subcommand
- Source: `crates/oneai-app-server/src/` (6 files: `lib`/`protocol`/`adapter`/`dispatcher`/`transport`/`scenario`) + `examples/cli/src/cmd_app_server.rs` + `platforms/{vscode,browser,macos,windows}`

## 11. Auto-spawn (Codex model) — the user never starts a server

Design principle: **a frontend that can spawn a process owns the spawn** —
the user never runs `oneai app-server` manually. This is the Codex CLI model:
the VS Code extension `child_process.spawn`s the engine on activation, speaks
JSON-RPC over stdio, restarts with exponential backoff on crash, disposes on
deactivate. Dispatched by frontend spawn capability:

| Frontend | Can spawn | Auto-spawn approach |
|---|---|---|
| VS Code extension | ✅ | activation `spawn(oneai, app-server --listen stdio)` (`platforms/vscode/src/server.ts`); webview relays via postMessage (it cannot spawn) |
| Browser extension | ❌ (sandbox) | Chrome native messaging: `install-host.sh` registers a host manifest once, then the browser spawns `oneai app-server --listen native-messaging` on connect (4-byte LE length-prefix framing, `serve_native_messaging`) |
| macOS desktop | ✅ | `EngineProcessManager.swift` spawns `oneai app-server --listen ipc://<ephemeral app-server-<pid>.sock>` (`.app/Contents/Resources/bin` first → PATH), hands off to `OneAiRpcClient`, restarts on exit |
| Windows desktop | ✅ | `EngineProcessManager.cs` spawns `--listen pipe://oneai-<pid>` (skeleton; wiring deferred) |
| Mobile | ❌ | in-process c_facade (Shape A; on-device, no spawn) |

**stdout discipline**: in `stdio` and `native-messaging` modes stdout is the
message stream — `oneai app-server` routes banners/diagnostics to stderr (LSP
convention), and `tracing` likewise to stderr, so stdout carries only framed
messages and never corrupts the protocol.

**Binary discovery**: frontends check the bundle first (`.app/Contents/
Resources/bin/oneai` / inside the `.vsix`), then PATH — consistent with
signature decoupling + independent engine-binary upgrades; VS Code uses a
`oneai.oneaiPath` setting + PATH fallback (mirrors Codex `codex.cliPath`).

**macOS wiring status (honest)**: the `OneAiRpcClient` + `EngineProcessManager`
infra is complete, compiles, and doesn't break the existing app build; the
`ChatViewModel` `oneai_engine_transport` flag wiring (single-agent +
group/scenarios via sidecar, FFI global fallback) + the `specView`
topic-baking port is the remaining work, which needs macOS-host runtime
verification — FFI remains the default transport, sidecar infra pending
adoption.
