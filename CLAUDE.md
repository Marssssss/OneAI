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
cargo run -p oneai-cli-demo     # launch the interactive TUI demo (bin name: oneai-cli)
```

The workspace uses `resolver = "2"`, `edition = "2021"`, shared version `0.2.0` from `[workspace.package]`, and all shared dependencies are pinned in `[workspace.dependencies]` — add new deps there and reference via `{ workspace = true }` in crate Cargo.tomls.

`#[non_exhaustive]` is applied to public enums as part of the v0.2.0 API-stability commitment (P3-1). Preserve it on existing public enums and add it to new externally-facing enum APIs.

## Commit convention

Git commit messages must end with `Co-Authored-By: glm-5.2` (the model actually driving this repo), **not** the default Claude Opus co-author line. Commit messages in this repo are frequently written in Chinese.

## Architecture: how the pieces wire together

The integration point is **`oneai-app`'s `AppBuilder`** (`crates/oneai-app/src/builder.rs`). Every subsystem (provider, tools, memory, RAG, skills, parser, persistence, trace, domain packs, WASM, MCP, A2A, SmartRouter, usage) is optional and plugged in via builder methods, then assembled into an `App` → `AppSession`. **The LLM provider is optional** — tool-only or workflow-only usage needs no provider. When changing how a subsystem is constructed or wired, this builder is the single place to update.

Dependency layering (lower crates must not depend on higher ones):
- `oneai-core` — foundation: `ContentBlock`/`Message`/`Conversation`, `PermissionLevel`, `Budget`, `ContextBudgetManager`, `PlatformCapabilities`, `ModelContextResolver`, and all core traits (`LlmProvider`, `Tool`, `InteractionGate`, `OutputParser`, `EmbeddingService`, `UsageTracker`, `RateLimiter`, `CircuitBreaker`, `TokenCounter`). `InteractionGate` unifies 5 decision points (`PreInfer`/`PostInfer`/`ToolApproval`/`PlanDecision`/`PlanReview`); `UsageTracker` records token-only usage (no USD cost tracking).
- `oneai-provider` (LLM impls: OpenAI/Anthropic/Ollama, `ProviderPool`, `SmartRouter`), `oneai-parser` (3-layer output defense), `oneai-memory`, `oneai-tool`, `oneai-skill`, `oneai-rag`, `oneai-workflow`, `oneai-domain`, `oneai-trace`, `oneai-persistence`, `oneai-a2a`, `oneai-wasm`, `oneai-eval`, `oneai-studio`, `oneai-mcp` — feature crates depending on core.
- `oneai-agent` — depends on the feature crates; owns `AgentLoop` and paradigms.
- `oneai-app` — top of the stack; depends on everything, wires it via `AppBuilder`.
- `oneai-uniffi` + `oneai-platform-{desktop,android,ios,harmony}` — FFI/foreign-language and native `PlatformInteractionGate` adapters.

**`AgentLoop` (`crates/oneai-agent/src/agent_loop.rs`) is the dynamic execution engine** — not a fixed pipeline. Each iteration the model returns one of `DirectAnswer` (loop ends), `ToolCalls` (execute + feed back), `Delegate` (hand to a `SubAgent`), or `SwitchParadigm` (move to Plan/Reflect/Explore). Termination is governed by `TokenBudget`, not a hardcoded `max_iterations`. `delegate`/`switch_paradigm` are model-driven via injected meta-tools (`meta_tool.rs`); `apply_paradigm_switch` + `AgentLoopGraphActionExecutor` inline-upgrade the paradigm (system prompt + tool filter) when the model or a StateGraph node requests it. Related agent-side files: `context_assembler.rs` (builds per-iteration context + env-diff detection), `streaming.rs` (incremental tool_use detection mid-stream), `sub_agent.rs`, `plan_agent.rs`/`plan_state.rs`, `react_agent.rs`, `reflection_agent.rs`, `parallel_executor.rs`/`scope_state.rs`, `team.rs`/`swarm.rs`/`handoff.rs`, `hooks.rs`, `async_task_runner.rs`, `structured_output.rs`, `skill_tool.rs`, `meta_tool.rs`. `mock_provider.rs`/`mock_tool.rs` are the test doubles used across agent tests.

**`DomainPack` (`oneai-domain`) is the central extensibility mechanism** — it makes domain knowledge declarative and composable across 7 layers: Tools+Decorators, ContextSources (with refresh policies), PermissionProfile, ParadigmStrategies, CompressionTemplate, Workflow+StateGraph, MemoryProfile. `CodingPack` is the built-in reference. DomainPacks merge for multi-domain agents (strictest-wins permissions, priority merge for context sources). `AppBuilder::domain_pack(...)` switches domains in one line.

**Permission model** is three-tier: `Read` (auto-approve), `Standard` (policy-dependent), `Full` (requires approval). Resolution order at runtime: `deny_by_default` → `permission_overrides` → `auto_approve` → `require_confirmation` → tool's own `risk_level()`. Interaction is gated by `InteractionGate` (`oneai-core` trait, 5 decision points `PreInfer`/`PostInfer`/`ToolApproval`/`PlanDecision`/`PlanReview`); implementations `NoopInteractionGate`/`ChannelInteractionGate`/`ThresholdInteractionGate`/`DenyAllInteractionGate` live in `oneai-tool`, and `PlatformInteractionGate` (native NSAlert/AlertDialog/UIController dialogs for `ToolApproval`) in the platform crates. The old `ApprovalGate`/`on_plan_submitted` were removed. When adding tools, set `permission_level()` correctly rather than `RiskLevel` alone.

**Tools** (`oneai-tool`): `Tool` + `PermissionAwareTool` traits, `ToolRegistry`, `ToolExecutor`, 12 built-in tools, MCP integration via `rmcp`, and `ShellTool` safety blacklist/sandbox. **3-layer parser** (`oneai-parser`) defends unreliable LLM output: constrained decoding → fuzzy JSON repair → fallback self-correction — reuse it rather than parsing model output directly.

## TUI (examples/cli)

The interactive demo (`examples/cli`, bin `oneai-cli`) is a ratatui+crossterm TUI exercising the full pipeline. It has many clap subcommands (provider/team/swarm/handoff/usage/route/token/embed/session/mcp/a2a/wasm/pack/eval/studio/...) mirroring subsystem features — useful as a working example of how to drive any given subsystem from `AppBuilder`. When implementing a new subsystem feature, add both an `AppBuilder` method and a CLI subcommand for parity with the existing pattern.

Recent TUI work fixed: scroll ghosting (Clear widget), long-history scroll lag (viewport virtualization in `draw_chat`), and added `InteractionMode` (Normal/Auto/Plan via Shift+Tab) where Plan mode blocks tool execution. Preserve these when touching TUI rendering.
