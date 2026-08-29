# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

OneAI is a cross-platform AI agent framework in Rust. It is a Cargo workspace of ~24 crates (`crates/*`) plus example binaries (`examples/*`). The README.md is the authoritative architectural reference — read it before making non-trivial changes; it documents the crate map, DomainPack layers, AgentLoop decisions, permission model, and paradigms in detail.

## Build, test, run

```bash
cargo build                      # build whole workspace
cargo test                      # run all tests across crates (see README badge for the current count)
cargo test -p oneai-agent      # tests for a single crate
cargo test -p oneai-agent agent_loop  # a single test/module within a crate
cargo test -p oneai-agent --test e2e_tests   # integration test file by name
cargo clippy --workspace --all-targets   # lints (keep clean — commits commonly fix warnings)
cargo run -p oneai-cli      # launch the interactive TUI (bin name: oneai)
```

The workspace uses `resolver = "2"`, `edition = "2021"`, shared version `0.2.0` from `[workspace.package]`, and all shared dependencies are pinned in `[workspace.dependencies]` — add new deps there and reference via `{ workspace = true }` in crate Cargo.tomls.

## Network proxy

All outbound HTTP in OneAI — LLM provider APIs (OpenAI/Anthropic/Ollama/Gemini), `web_search`/`web_fetch` tools, A2A client, embedding services, MCP HTTP transport — goes through `reqwest::Client`. Proxy support is therefore env-var based and works everywhere uniformly:

- `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` — proxy URL (auto-detected by reqwest on every client build; no code path opt-in needed).
- `NO_PROXY` — comma-separated exclusion list.
- SOCKS5: set `ALL_PROXY=socks5://host:port` (requires the reqwest `socks` feature, kept on in the workspace `Cargo.toml`).
- On macOS/Windows, reqwest's `system-proxy` feature also reads the OS GUI proxy settings; env vars always win.

Do not wire a custom `reqwest::Client` into individual providers/tools for proxy purposes — rely on the env vars. If a future subsystem needs a bespoke client, build it with `reqwest::Client::builder()` so it still picks up these env vars. The `crates/oneai-provider/tests/proxy_feature.rs` smoke test guards the `socks`/proxy feature flags.

`#[non_exhaustive]` is applied to public enums as part of the v0.2.0 API-stability commitment (P3-1). Preserve it on existing public enums and add it to new externally-facing enum APIs.

## Commit convention

Git commit messages must end with `Co-Authored-By: <model>`, where `<model>` is the model actually driving the session — read from the environment's "You are powered by the model …" line and strip any bracketed annotation like `[1m]` (e.g. `deepseek-v4-pro` / `qwen-3.8-max`). Do **not** hardcode a name, and do **not** use the default Claude Opus co-author line. Commit messages in this repo are frequently written in Chinese.

## Supply-chain discipline

Reproducibility comes from the **committed `Cargo.lock`** (the workspace ships binaries — `oneai-cli` + examples — so the lockfile is tracked, not gitignored). On top of that, four gates enforce supply-chain integrity (evolution-plan §1.3):

- **`deny.toml`** — `cargo deny check` config: advisories (RustSec), license allowlist, ban on wildcards/unknown sources. Single source of truth; `cargo audit` is NOT used in CI to avoid drifting two parallel ignore lists. Accepted-risk advisories live in `[advisories].ignore` with a per-entry `reason` string — never ignore silently. Install locally: `cargo install cargo-deny && cargo deny check`.
- **`.github/workflows/audit.yml`** — daily cron + PR-triggered `cargo deny check`. PRs only gate on `Cargo.toml`/`Cargo.lock`/`deny.toml` changes.
- **lockfile drift gate** (`ci.yml` `lockfile-gate` job) — a PR that modifies `Cargo.lock` fails CI unless `ONEAI_ALLOW_LOCKFILE_CHANGE=1` is set. To intentionally upgrade a dependency, set that env on the PR (or note it in the commit message and a maintainer re-runs with the env). Push-to-main always passes.
- **`.github/workflows/publish.yml`** — tag `v*` triggered publish to crates.io via `scripts/publish_crates.sh` (Kahn topological order + idempotent skip + 429 backoff). `id-token: write` is set for crates.io Trusted Publishing (one-time crates.io UI binding required — see the workflow's setup header). Pre-publish smoke: `./scripts/release-local.sh` runs `cargo publish --dry-run` for every crate (packages + rewrites path deps + isolated build) before you tag.

When adding a new external dependency: add it to `[workspace.dependencies]` in the root `Cargo.toml` and reference via `{ workspace = true }` in the crate. Do NOT wire per-crate version pins — they're centralized for a reason. Run `cargo deny check` before committing; if it flags a new license or advisory, add an exception/ignore with a rationale rather than widening blindly.

## Architecture: how the pieces wire together

The integration point is **`oneai-app`'s `AppBuilder`** (`crates/oneai-app/src/builder.rs`). Every subsystem (provider, tools, memory, RAG, skills, parser, persistence, trace, domain packs, WASM, MCP, A2A, SmartRouter, usage) is optional and plugged in via builder methods, then assembled into an `App` → `AppSession`. **The LLM provider is optional** — tool-only or workflow-only usage needs no provider. When changing how a subsystem is constructed or wired, this builder is the single place to update.

Dependency layering (lower crates must not depend on higher ones):
- `oneai-core` — foundation: `ContentBlock`/`Message`/`Conversation`, `PermissionLevel`, `Budget`, `ContextBudgetManager`, `PlatformCapabilities`, `ModelContextResolver`, and all core traits (`LlmProvider`, `Tool`, `InteractionGate`, `OutputParser`, `EmbeddingService`, `UsageTracker`, `RateLimiter`, `CircuitBreaker`, `TokenCounter`). `InteractionGate` unifies 7 decision points — 5 per-iteration (`PreInfer`/`PostInfer`/`ToolApproval`/`PlanDecision`/`PlanReview`) + 2 on-demand (`NetworkApproval` sandboxed-egress, `McpElicitation` server→user); `UsageTracker` records token-only usage (no USD cost tracking).
- `oneai-bus` — the unified engine↔frontend protocol crate (`Directive`/`EngineYield` + `EngineBus`); depends only on `oneai-core`, consumed by agent (`BusObserver`/`BusInteractionGate`), app (`engine_bus()`/`run_turn_via_bus`/`spawn_directive_pump`), and uniffi (c_facade 3-symbol pump / `oneai serve` sidecar). See `docs/bus-mechanism.md`.
- `oneai-app-server` — the JSON-RPC 2.0 frontend protocol layer over the bus (L2 adapter: `turn/run`/`approval/respond`/… ↔ `Directive`; `EngineYield` → `event` notification) + multi-transport (stdio / ipc / ws). Depends on `oneai-bus` + `oneai-supervisor` (for `IpcListener`), NOT `oneai-app` — the CLI (`oneai app-server`) builds the engine and passes `Arc<InProcessBus>` to `serve_all`. Feeds IDE/web/desktop non-Rust frontends; TUI stays in-process, `oneai serve` stays as newline-JSON escape hatch. See `docs/app-server-mechanism.md`.
- `oneai-provider` (LLM impls: OpenAI/Anthropic/Ollama, `ProviderPool`, `SmartRouter`), `oneai-parser` (3-layer output defense), `oneai-memory`, `oneai-tool`, `oneai-skill`, `oneai-rag`, `oneai-vector` (default retrieval stack), `oneai-workflow`, `oneai-domain`, `oneai-trace`, `oneai-persistence`, `oneai-a2a`, `oneai-wasm`, `oneai-eval`, `oneai-studio`, `oneai-mcp`, `oneai-scheduler`, `oneai-gateway`, `oneai-supervisor`, `oneai-evolve` (self-evolution loop) — feature crates depending on core.
- `oneai-agent` — depends on the feature crates; owns `AgentLoop` and paradigms.
- `oneai-app` — top of the stack; depends on everything, wires it via `AppBuilder`.
- `oneai-uniffi` + `oneai-platform-{desktop,android,ios,harmony}` — FFI/foreign-language and native `PlatformInteractionGate` adapters.

**`AgentLoop` (`crates/oneai-agent/src/agent_loop.rs`) is the dynamic execution engine** — not a fixed pipeline. Each iteration the model returns one of `DirectAnswer` (loop ends), `ToolCalls` (execute + feed back), `Delegate` (hand to a `SubAgent`), or `SwitchParadigm` (move to Plan/Reflect/Explore). Termination is governed by `TokenBudget`, not a hardcoded `max_iterations`. `delegate`/`switch_paradigm` are model-driven via injected meta-tools (`meta_tool.rs`); `apply_paradigm_switch` + `AgentLoopGraphActionExecutor` inline-upgrade the paradigm (system prompt + tool filter) when the model or a StateGraph node requests it. Related agent-side files: `context_assembler.rs` (builds per-iteration context + env-diff detection), `streaming.rs` (incremental tool_use detection mid-stream), `sub_agent.rs`, `plan_agent.rs`/`plan_state.rs`, `react_agent.rs`, `reflection_agent.rs`, `parallel_executor.rs`/`scope_state.rs`, `team.rs`/`swarm.rs`/`handoff.rs`, `hooks.rs`, `async_task_runner.rs`, `structured_output.rs`, `skill_tool.rs`, `meta_tool.rs`. `mock_provider.rs`/`mock_tool.rs` are the test doubles used across agent tests.

**Working state & cross-session resume** (`oneai-persistence`/`oneai-agent`/`oneai-app`) — a task's goal/steps/decisions/blockers are persisted as a per-task append-only JSONL event log (`FileWorkingStateStore`, `<root>/tasks/{task_id}.jsonl` + `tasks.index.json`), **not** in the session transcript. The hot read path uses the in-memory `LoopState.working_state` projection (zero file IO per turn); `append_event` is the only write path, called at every plan-control-tool mutation (exit_plan_mode / task_update / decision) so progress survives crashes. A brand-new session reads `list_open_tasks` (one index.json read) and surfaces `[Unfinished Work From Previous Sessions]`; `tasks continue <id>` binds the new session to that task and re-derives its state. Policy is declared in `MemoryProfile.working_state` (DomainPack layer 7). The old `ProgressiveCheckpointManager`/`CheckpointBackend`/`auto_checkpoint`/`AppSession::save_checkpoint` SQLite checkpoint infra was removed in favor of this — only `FilePersistence`/`StatePersistence` (used by Studio's checkpoint browser) remain. Full mechanism: `docs/working-state-mechanism.md`.

**`DomainPack` (`oneai-domain`) is the central extensibility mechanism** — it makes domain knowledge declarative and composable across 7 layers: Tools+Decorators, ContextSources (with refresh policies), PermissionProfile, ParadigmStrategies, CompressionTemplate, Workflow+StateGraph, MemoryProfile. `CodingPack` is the built-in reference. DomainPacks merge for multi-domain agents (strictest-wins permissions, priority merge for context sources). `AppBuilder::domain_pack(...)` switches domains in one line.

**Permission model** is three-tier: `Read` (auto-approve), `Standard` (policy-dependent), `Full` (requires approval). Resolution order at runtime: `deny_by_default` → `permission_overrides` → `auto_approve` → `require_confirmation` → tool's own `risk_level()`. Interaction is gated by `InteractionGate` (`oneai-core` trait, 7 decision points — 5 per-iteration `PreInfer`/`PostInfer`/`ToolApproval`/`PlanDecision`/`PlanReview` + 2 on-demand `NetworkApproval`/`McpElicitation`); implementations `NoopInteractionGate`/`ChannelInteractionGate`/`ThresholdInteractionGate`/`DenyAllInteractionGate` live in `oneai-tool`, `BusInteractionGate` (approval routed through the engine bus — `EngineYield::ApprovalRequest`↔`Directive::Approve`) in `oneai-agent`, and `PlatformInteractionGate` (native NSAlert/AlertDialog/UIController dialogs for `ToolApproval`) in the platform crates. The old `ApprovalGate`/`on_plan_submitted` were removed. When adding tools, set `permission_level()` correctly rather than `RiskLevel` alone.

**Tools** (`oneai-tool`): `Tool` + `PermissionAwareTool` traits, `ToolRegistry`, `ToolExecutor`, 17 built-in tools (incl. `code_interpreter` — sandboxed CPython that marshals in-script tool calls back to the host through the same approval path), MCP integration via `rmcp`, and `ShellTool` safety blacklist/sandbox. **`ToolExposure`** (6 values: `Direct`/`Deferred`/`DeferredModelOnly`/`DirectModelOnly`/`CodeModeOnly`/`Hidden`) controls a tool's visibility to the model (initial schema / `tool_search` / code mode), resolved via `ExposureResolver` against DomainPack `PermissionProfile.tool_exposure`. **Sandboxed egress (#28)**: `code_interpreter`/`shell` run inside Seatbelt (mac) / Bubblewrap (linux) with loopback-only network; outbound HTTPS is funnelled through a local CONNECT proxy that checks `HostAllowlistStore` + `NetworkApprovalMode{Prompt/Defer/Deny}`; shell commands go through `Guardian` (`ApprovalPolicy` 4-level) + `ExecPolicy` (token-prefix rule engine, strictest-wins) + `ExecPolicyStore` (post-approval amendment hot-swap + JSONL persistence) — approve once, the rule gates upfront next time. File tools route through `SandboxedFileOps`. **3-layer parser** (`oneai-parser`) defends unreliable LLM output: constrained decoding → fuzzy JSON repair → fallback self-correction — reuse it rather than parsing model output directly.

**Footprint Ladder** (new-capability decision rule for "where does this tool live?"): prefer the rung with the smallest per-session schema footprint the model sees, climbing only when the lower rung can't satisfy it — `extend` (compose from existing tools, no new schema) → `skill` (a markdown prompt, zero tool schema) → `service-gated tool` (a `Tool` whose `service_available()` returns `false` when its backing service is missing, so it **vanishes from the schema** — zero footprint, not merely "disabled"; register via `ToolRegistry::register_gated(tool, check_fn)` or override `Tool::service_available`) → `plugin`/`MCP tool` (external process, conditionally connected; lazy MCP servers stay deferred and surface via a `tool_search`-triggered `McpLazyConnectTool`) → `core tool` (always present in the schema). `ToolExposure` then governs visibility *after* a tool lands on a rung. A tool whose prerequisites are unmet must never appear as a broken option the model would try to call; gate it. `build_tool_definitions_for_paradigm`/`build_tool_definitions_for_state` apply this filter on every iteration and log when a configured tool is hidden.

## TUI (examples/cli)

The interactive demo (`examples/cli`, bin `oneai-cli`) is a ratatui+crossterm TUI exercising the full pipeline. It has many clap subcommands (provider/team/swarm/handoff/usage/route/token/embed/session/mcp/a2a/wasm/pack/eval/studio/serve/evolve/...) mirroring subsystem features — useful as a working example of how to drive any given subsystem from `AppBuilder`. `oneai serve` runs the engine-bus sidecar (exposes the engine over UDS/named-pipe to out-of-process native frontends); `oneai evolve` runs the self-evolution loop. When implementing a new subsystem feature, add both an `AppBuilder` method and a CLI subcommand for parity with the existing pattern.

Recent TUI work fixed: scroll ghosting (Clear widget), long-history scroll lag (viewport virtualization in `draw_chat`), and added `InteractionMode` (Normal/Auto/Plan via Shift+Tab) where Plan mode blocks tool execution. Preserve these when touching TUI rendering.
