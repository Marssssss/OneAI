# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] — 2026-07-15

First stable, public release. The Rust core, the `oneai-cli` TUI, and the
unsigned macOS app are now distributable; Windows / Android / iOS / HarmonyOS
apps remain pre-release.

### Agent SDK (crates.io)

The following crates are published to crates.io under a shared `1.0.0` version:

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

[1.0.0]: https://github.com/Marssssss/OneAI/releases/tag/v1.0.0
