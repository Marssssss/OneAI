# Contributing to OneAI

First off — thank you for considering a contribution. OneAI is a large, layered
Rust workspace, and a little orientation up front saves a lot of back-and-forth
during review. This document is the **human-facing** companion to
[`CLAUDE.md`](./CLAUDE.md) (which guides AI coding agents); both describe the
same architecture, so read whichever suits you.

## 1. Getting the project to build

You need a recent Rust toolchain. The repo ships a `rust-toolchain.toml` that
pins `stable` with the `clippy` and `rustfmt` components — `rustup` will install
it automatically the first time you run a cargo command.

```bash
git clone https://github.com/Marssssss/OneAI.git
cd OneAI
cargo build                  # build the whole workspace
cargo test                   # run the full test suite
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

The declared MSRV is in the root `Cargo.toml` (`rust-version`), currently
**1.74**. CI also runs an MSRV build job.

### System dependencies

`sqlite-vec`, `tantivy`, and `usearch` compile their bundled C/C++ sources, so
you need a working C toolchain (`build-essential` on Debian/Ubuntu,
Xcode Command Line Tools on macOS). No other system packages are required for a
default build.

### Proxy / network

All outbound HTTP (LLM providers, `web_search`/`web_fetch`, A2A, embeddings, MCP
HTTP transport) goes through one `reqwest::Client` that honors the standard
proxy env vars: `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY`. Set
these in your shell if your network needs them. **Do not** wire a custom
`reqwest::Client` into individual providers or tools for proxy purposes.

## 2. Architecture in one screen

OneAI is a Cargo workspace of ~26 crates with strict layering — **lower crates
must not depend on higher ones**:

```
oneai-core              ← foundation: ContentBlock/Message, traits (LlmProvider,
                          Tool, InteractionGate, OutputParser, EmbeddingService,
                          UsageTracker, RateLimiter, CircuitBreaker, …)
   ↑
oneai-provider          ← LLM impls (OpenAI/Anthropic/Ollama), ProviderPool, SmartRouter
oneai-parser            ← 3-layer output defense (constrained → fuzzy repair → self-correct)
oneai-memory / oneai-rag / oneai-tool / oneai-skill / oneai-workflow
oneai-domain            ← DomainPack (7-layer declarative domain config; CodingPack is reference)
oneai-trace / oneai-persistence / oneai-a2a / oneai-wasm / oneai-eval / oneai-mcp
   ↑
oneai-agent             ← owns AgentLoop + paradigms (Plan/Reflect/Explore, Delegate)
   ↑
oneai-app               ← AppBuilder wires every subsystem into an App → AppSession
   ↑
oneai-uniffi + oneai-platform-{desktop,android,ios,harmony}  ← FFI / native gates
```

**The integration point is `oneai-app`'s `AppBuilder`** (`crates/oneai-app/src/builder.rs`).
Every subsystem is optional and plugged in via a builder method. When you add a
new subsystem, add both an `AppBuilder` method and a CLI subcommand (see
`examples/cli`) for parity with the existing pattern.

`AgentLoop` (`crates/oneai-agent/src/agent_loop.rs`) is a **dynamic execution
engine**, not a fixed pipeline — each iteration the model returns
`DirectAnswer` / `ToolCalls` / `Delegate` / `SwitchParadigm`. Termination is
governed by `TokenBudget`, not a hardcoded `max_iterations`.

`DomainPack` (`oneai-domain`) is the central extensibility mechanism across 7
layers: Tools+Decorators, ContextSources, PermissionProfile, ParadigmStrategies,
CompressionTemplate, Workflow+StateGraph, MemoryProfile.

The `README.md` is the authoritative architectural reference — read it before
non-trivial changes.

## 3. Conventions you must not break

These exist because the codebase has been hardened against unreliable LLM
output and cross-platform deployment. When in doubt, follow the surrounding
code.

- **Crate layering is enforced.** A lower crate (`oneai-core`,
  `oneai-provider`, …) must never depend on a higher one (`oneai-agent`,
  `oneai-app`). If you find yourself importing "up", you are in the wrong layer.
- **Parse model output through the 3-layer parser** (`oneai-parser`), never
  `serde_json::from_str` directly. The parser does constrained decoding → fuzzy
  JSON repair → fallback self-correction precisely so a malformed model response
  doesn't crash the loop.
- **Set `permission_level()` correctly when adding a tool**, not just
  `risk_level()`. Permission is three-tier (`Read` / `Standard` / `Full`); the
  resolution order is `deny_by_default → permission_overrides → auto_approve →
  require_confirmation → tool.risk_level()`.
- **Preserve `#[non_exhaustive]`** on existing public enums and add it to new
  externally-facing enum APIs (part of the v0.2.0 API-stability commitment).
- **Persist working state via `append_event`**, not the session transcript. See
  `docs/working-state-mechanism.md`.
- **Proxy is env-var based** (see §1). Don't add bespoke clients.
- **When touching the TUI** (`examples/cli`): preserve the Clear-widget fix for
  scroll ghosting, the viewport virtualization in `draw_chat`, and the
  `InteractionMode` (Normal/Auto/Plan via Shift+Tab) where Plan mode blocks tool
  execution.

## 4. Commit & PR style

- Commit messages are frequently written in **Chinese** and follow Conventional
  Commits (`feat:` / `fix:` / `docs:` / `refactor:` / `test:` / `chore:`).
  English is equally welcome.
- Keep commits focused and reviewable. Avoid drive-by refactors in a feature
  commit — split them out.
- If you author a commit with an AI coding agent, keep the `Co-Authored-By`
  trailer the agent adds. Human-authored commits need no co-author trailer.

## 5. Before opening a PR — self-check

- [ ] `cargo fmt --all --check` is clean.
- [ ] `cargo clippy --workspace --all-targets` introduces **no new warnings**
  (the repo has a known tail of historical lints tracked as good-first-issues —
  don't add to it).
- [ ] `cargo test --workspace` passes.
- [ ] You have not broken crate layering (no lower→higher imports).
- [ ] New/changed behavior has a test.
- [ ] Public API additions carry `#[non_exhaustive]` where appropriate.
- [ ] Docs updated if behavior changed (`README.md` and `README_EN.md` are kept
  in sync — mirror changes across both).

## 6. Finding something to work on

- Issues labeled [`good first issue`](https://github.com/Marssssss/OneAI/labels/good%20first%20issue)
  are scoped to be approachable without deep architecture knowledge.
- Issues labeled `help wanted` are welcome contributions of any size.
- The clippy lint cleanup is tracked as a rolling batch of small, independent
  fixes — each is a self-contained PR.

If you want to work on something not filed as an issue, open a draft issue or
discussion first so we can agree on scope before you spend time on it.

## 7. Questions

Open a [GitHub Discussion](https://github.com/Marssssss/OneAI/discussions) for
usage questions and design proposals; reserve issues for concrete bugs and
feature requests.

By contributing, you agree your contributions are licensed under the project's
[Apache-2.0](./LICENSE) license.
