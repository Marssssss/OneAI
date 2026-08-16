# OneAI webUI (`platforms/web`)

OneAI's end-user-facing Web interface — a React SPA over the
`oneai app-server` JSON-RPC (WebSocket) protocol. This is the W1 scaffold:
single-agent streaming chat over the bus, with the design language and
architecture benchmarked against deepseek-harness's webUI (see
`docs/webui-refactor-design.md`).

## Stack

- React 19 + Vite 7 + TypeScript (strict, `verbatimModuleSyntax`)
- CSS Modules + `--oneai-*` design tokens (light/dark)
- `useSyncExternalStore` external store (no Redux/Zustand) — the projection
  store accumulates `EngineYield` events into chat nodes
- `react-markdown` + `remark-gfm` + `rehype-highlight` (the incremental
  micromark renderer is a W2/W5 refinement)
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
  theme/locale. Verified: typecheck + build + live streaming turn against a
  real engine (turn_start → stream_chunk\* → turn_complete).
- **W2 (next)**: full tool tree + approval panel (parallel queue) + plan mode +
  keyed node perf + incremental markdown.
- **W3**: the **scenario conversation entry** — OneAI's differentiator dsh
  entirely lacks (`group/*` + `scenario/*`, multi-role group chat).
