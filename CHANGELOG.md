# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Tracking `docs/gap-analysis-2026-07.md` + `docs/evolution-plan-2026-07.md`. The
`1.0.0` / `1.1.0` tags were premature (never published on crates.io, no
external users) and are **retracted** — versioning restarted at `0.1.0`.

## [0.2.0] — 2026-08-17

First release to ship the **WebUI** (`oneai web` / `npx oneai-cli web`) — one
command launches the engine + React SPA + `/ws` JSON-RPC on one port and
opens the browser. The web dist is bundled into the npm tarball
(`prepublishOnly` runs `build-web.sh`); cargo/binary users auto-detect
`./platforms/web/dist` or `~/.oneai/web-dist`, or pass `--dist`.

### WebUI (browser frontend, recommended)

- W1–W5: React 19 + Vite SPA over the `oneai-app-server` ws JSON-RPC protocol
  — projection store, 20fps stream coalescer, column-yield geometry + mobile
  overlay drawer, scenario group-chat with speaker routing, settings/probe
  RPC + live `ProviderPool` management, dark tokens (zero hard-coded colors),
  vitest + Playwright e2e. `docs/webui-mechanism.md`.
- `oneai web` one-command launch: `serve_web` (axum `http` feature, default-on)
  serves `ServeDir` (SPA fallback) + `WebSocketUpgrade` on one route, bridging
  to the existing `serve_connection` seam — zero duplicated JSON-RPC.
- Attachments (drag/paste → base64 image `ContentBlock`), deliverables
  (`ToolOutput.artifacts` from `write_file`/`apply_patch`), per-message
  👍/👎 feedback (`feedback/submit`·`feedback/list` SQLite).
- Network-egress authorization persisted: `host/*` sync RPC + web
  Allow once/Always/Deny + Settings Network panel.
- Session rename/archive land in the engine (`session/rename` +
  `session/archive` RPC); WebUI drops localStorage session meta.

### Engine / cross-frontend

- Canonical session DB `~/.oneai/oneai.db` — macOS (FFI + sidecar), Windows,
  and WebUI/TUI all share one backend (was `~/Library/Application Support/...`
  on mac, diverging from the canonical default).
- Architecture documented as migrating from in-process FFI to JSON-RPC 2.0 /
  separate-process: the `oneai-uniffi` cdylib now exports only a 3-symbol bus
  pump (`oneai_submit_directive` / `oneai_poll_yield` / `oneai_shutdown`) —
  same `Directive`/`EngineYield` protocol as the sidecar. WebUI / VS Code /
  browser are fully on the sidecar; macOS has FFI (default) + sidecar (opt-in);
  Android stays in-process on-device; Windows has a sidecar skeleton (its C#
  P/Invoke surface is stale vs the collapsed facade, pending migration).

### Docs

- README: webUI brand logo (theme-aware), WebUI promoted to the primary
  frontend (npm + source run), "at a glance" removed, macOS sidecar build
  steps updated.
- `architecture.md` / `cross-platform-mechanism.md` (CN+EN) reframed for the
  FFI → JSON-RPC migration; the legacy 29-symbol C facade references removed
  in favor of the 3-symbol bus pump; honest per-target migration status table.

## [1.0.0] — 2026-07-15 (retracted)

First stable, public release. The Rust core, the `oneai-cli` TUI, and the
unsigned macOS app are now distributable; Windows / Android / iOS / HarmonyOS
apps remain pre-release.

### Agent SDK (crates.io)

The following crates were slated for crates.io under a shared `1.0.0` version
(never published — retracted; see [Unreleased] above):

- `oneai-core` — `ContentBlock`/`Message`/`Conversation`, `PermissionLevel`,
  `Budget`, `ContextBudgetManager`, core traits (`LlmProvider`, `Tool`,
  `InteractionGate`, `OutputParser`, `EmbeddingService`, `UsageTracker`,
  `RateLimiter`, `CircuitBreaker`, `TokenCounter`).
- `oneai-provider` — OpenAI / Anthropic / Ollama providers, `ProviderPool`
  fallback chain, `SmartRouter` multi-factor routing.
- `oneai-agent` — `AgentLoop` dynamic execution engine (DirectAnswer /
  ToolCalls / Delegate / SwitchParadigm), Plan / ReAct / Reflection / Explore
  paradigms, SubAgent, parallel executor, team / swarm / handoff.
- `oneai-workflow` — workflow compiler, DAG, validator, executor, StateGraph.
- `oneai-memory` — STM / LTM / context compression, DomainPack MemoryProfile.
- `oneai-tool` — `ToolRegistry`, `ToolExecutor`, 12 built-in tools, MCP
  integration via `rmcp`, `ShellTool` safety sandbox.
- `oneai-skill` — progressive-disclosure skill system.
- `oneai-parser` — 3-layer output defense (constrained decoding → fuzzy
  repair → fallback self-correction).
- `oneai-rag` — retrieval-augmented generation, embedding services.
- `oneai-scheduler`, `oneai-persistence` (SQLite session store),
  `oneai-trace` (OpenInference trajectory logger), `oneai-domain` (DomainPack
  7-layer extensibility, `CodingPack` reference), `oneai-a2a` (A2A protocol
  SDK), `oneai-wasm` (Wasmtime sandbox), `oneai-eval`, `oneai-studio`,
  `oneai-mcp`, `oneai-platform-{desktop,android,ios,harmony}`.
- `oneai-app` — **SDK entry point**: `AppBuilder` wires every optional
  subsystem into an `App` → `AppSession`. The provider is optional; tool-only
  and workflow-only usage needs no provider.
- `oneai-uniffi` — UniFFI foreign-language binding definitions.

`cargo add oneai-app` embeds the framework; `cargo install oneai-cli` installs
the TUI. Public enums are `#[non_exhaustive]`; breaking changes will be
signaled by a minor version bump.

### TUI (`oneai-cli`)

`cargo install oneai-cli` provides the interactive `ratatui`+`crossterm` REPL
plus non-interactive inference, exposing subsystems as `clap` subcommands
(provider / team / swarm / handoff / usage / route / token / embed / session /
mcp / a2a / wasm / pack / eval / studio …). InteractionMode (Normal / Auto /
Plan via Shift+Tab); Plan mode blocks tool execution.

### macOS app (unsigned)

Native SwiftUI app built via `scripts/build_apple.sh` + `platforms/macos/build_macos.sh`
— universal arm64+x86_64 `.app` linking the static `liboneai.a`. Unsigned:
first launch requires right-click → Open to bypass Gatekeeper. macOS 13+.

### Documentation & tooling

- Added `LICENSE` (Apache-2.0).
- `[profile.release]` now uses thin LTO, single codegen unit, and `strip`.
- Internal workspace dependencies carry explicit `version` fields so
  `cargo publish` rewrites path deps to registry requirements.

### Known limitations

- Windows / Android / iOS / HarmonyOS native apps are not part of this
  release.
- macOS app is unsigned / un-notarized.
- TUI is distributed only via crates.io (`cargo install`); no prebuilt
  binaries or Homebrew formula in this release.

[Unreleased]: https://github.com/Marssssss/OneAI/tree/main
[1.0.0]: https://github.com/Marssssss/OneAI/releases/tag/v1.0.0
