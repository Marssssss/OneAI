# OneAI Architecture & Technical Design

> One AI, Every Platform — one Rust core, six targets. This is the technical overview; the entry point is [README](../README_EN.md).

OneAI is a full-stack agent framework written in Rust, providing everything needed to build, run, and evaluate AI agents: from LLM-provider abstraction to tool execution, memory management, workflow orchestration, domain-specific config, multi-agent collaboration, and tracing — all cross-platform. **The LLM provider is optional** — tool-only or workflow-only usage needs no provider.

## Design principles

- **Modular** — 28 independent crates, each with one job, used on demand.
- **Type-safe** — sealed-enum hierarchies (every public enum is `#[non_exhaustive]`), trait-driven abstractions, no string-config.
- **Domain-pluggable** — [DomainPack](domain-pack-mechanism_EN.md) makes domain knowledge declarative, composable, one-line-switchable; validatable against a JSON Schema and shareable via a pack market.
- **Natively multi-agent** — model-driven SubAgent hierarchical delegation (`delegate` meta-tool, multi-delegate per turn + dependency-aware parallel-wave scheduling) + paradigm switch (`switch_paradigm` into Plan/Reflect/Explore graph flows) + engine-level [GroupChat](multi-agent-mechanism_EN.md) primitive for scenario-based multi-role conversations.
- **Production-grade infrastructure** — [ProviderPool](provider-mechanism_EN.md) fallback chain, SmartRouter multi-factor routing, usage tracking, rate limiting, circuit breaking, token-aware context management.
- **Cross-platform** — [UniFFI + hand-written extern "C" facade](cross-platform-mechanism_EN.md) for macOS / Windows / Linux / Android / iOS / HarmonyOS, one Rust core.
- **Evaluable** — built-in [OpenInference-compatible tracing](trace-mechanism_EN.md) + a standalone [eval framework](eval-mechanism_EN.md) (6 metrics, 3 suites + SWE-bench three-axis).
- **Human-in-the-loop** — high-risk tools gated by [native UI dialogs](permission-mechanism_EN.md); a Plan-mode approval gate before execution.
- **Dynamic Agentic Loop** — not a fixed pipeline; each iteration dynamically decides (direct answer / tool call / delegate to a sub-agent / switch paradigm).

## Dependency layering

Lower crates must not depend on higher ones:

```
oneai-core                      foundation: types + core traits (no downstream deps)
      ↑
oneai-provider / -parser / -memory / -tool / -skill / -rag
/ -workflow / -domain / -trace / -persistence / -a2a / -wasm
/ -eval / -studio / -mcp / -scheduler / -gateway / -supervisor / -vector   feature crates (depend on core)
      ↑
oneai-agent                     execution engine: AgentLoop + paradigms + delegation
      ↑
oneai-app                       integration layer: AppBuilder → App → AppSession (the one assembly point)
      ↑
oneai-uniffi + oneai-platform-* FFI / native platform adapters
```

The integration point is **`oneai-app`'s `AppBuilder`** (`crates/oneai-app/src/builder.rs`). Every subsystem is optional and plugged in via builder methods (the LLM provider included). When changing how a subsystem is constructed or wired, this is the single place to update. For contributor-grade working guidance see [CLAUDE.md](../CLAUDE.md).

## Architecture diagram

```mermaid
flowchart TB
    subgraph FE ["🖥️ Frontends — two frontends, one core"]
        direction LR
        TUI["CLI / TUI<br/>oneai-cli · ratatui+crossterm<br/>general agentic execution / subsystem exploration"]
        Native["Native apps<br/>macOS · Win · Linux<br/>Android · iOS · HarmonyOS<br/>scenario-based multi-agent group chat"]
    end

    subgraph FFI ["🔌 FFI layer · oneai-uniffi + oneai-platform-*"]
        direction LR
        UniFFI["UniFFI bindings<br/>Kotlin · Swift · Python"]
        CFacade["Hand-written extern C facade<br/>C# · C++ · ArkTS<br/>UTF-8 JSON across the boundary, CJK round-trips correctly"]
    end

    subgraph App ["🧩 Integration layer · oneai-app"]
        Builder["AppBuilder → App → AppSession<br/>the one assembly point · every subsystem optional, plugged in on demand"]
    end

    subgraph Agent ["⚙️ Execution engine · oneai-agent (dynamic loop, not a fixed pipeline)"]
        Loop["AgentLoop · each iteration the model dynamically decides<br/>iteration cap bounded by TokenBudget (not hardcoded max_iterations)"]
        Loop -->|DirectAnswer| Done["return final answer → loop ends"]
        Loop -->|ToolCalls| Exec["execute tools → feed back results → continue"]
        Loop -->|Delegate| Sub["SubAgent<br/>Plan / Explore / Code / Review (optional worktree isolation)"]
        Loop -->|SwitchParadigm| Paradigm["switch to Plan / Reflect / Explore<br/>apply_paradigm_switch inline upgrade<br/>system prompt + tool filtering"]
        Paradigm -. via meta_tool .-> Loop
    end

    Domain["🎨 oneai-domain · DomainPack 7 layers<br/>① tools+decorators ② ContextSource ③ PermissionProfile<br/>④ ParadigmStrategy ⑤ CompressionTemplate ⑥ Workflow+StateGraph ⑦ MemoryProfile<br/>+ market + JSON-Schema spec validator — cross-cutting declarative config: one-line switch, mergeable, validatable, shareable"]

    subgraph Features ["📦 Feature layer · Feature crates (grouped by domain, all depend on oneai-core)"]
        direction LR
        subgraph F1 ["Provider & parsing"]
            Prov["oneai-provider<br/>OpenAI/Anthropic/Gemini/Ollama<br/>ProviderPool fallback chain · SmartRouter multi-factor routing · 429 retry"]
            Parser["oneai-parser<br/>3-layer output defense: constrained decode → fuzzy repair → self-correct re-prompt"]
        end
        subgraph F2 ["Tools · skills · RAG"]
            Tool["oneai-tool<br/>Registry + 15 built-in tools + MCP client + InteractionGate"]
            Skill["oneai-skill<br/>selector + registry + convention-dir discovery"]
            Rag["oneai-rag<br/>EmbeddingService + hybrid retrieval + auto embedding"]
        end
        subgraph F3 ["Memory · persistence · tracing"]
            Mem["oneai-memory<br/>Letta 3 tiers (recall/core/archival) + compression-coupled extraction + persistence"]
            Persist["oneai-persistence<br/>SQLite(sessions/LTM/usage) + file event log (working state)"]
            Trace["oneai-trace<br/>OpenInference-compatible + OTEL exporter"]
        end
        subgraph F4 ["Orchestration · extensions"]
            Wf["oneai-workflow<br/>DAG + StateGraph (closed-loop with AgentLoop)"]
            Wasm["oneai-wasm<br/>Wasmtime sandbox + WasmTool"]
            A2a["oneai-a2a<br/>Agent-to-Agent protocol SDK + server host"]
            Eval["oneai-eval<br/>6 metrics + 3 suites + SWE-bench three-axis"]
            Studio["oneai-studio<br/>axum HTTP+WS + D3 visualization"]
            Mcp["oneai-mcp<br/>MCP server host + plugin registry"]
        end
    end

    subgraph Core ["🧱 Foundation layer · oneai-core (no downstream deps)"]
        CoreT["types: ContentBlock · Message · Conversation · PermissionLevel · Budget<br/>ContextBudgetManager · PlatformCapabilities · ModelContextResolver<br/>core traits: LlmProvider · Tool · InteractionGate(5 decision points)<br/>EmbeddingService · UsageTracker · RateLimiter · CircuitBreaker · TokenCounter"]
    end

    Native --> UniFFI
    Native --> CFacade
    TUI --> Builder
    UniFFI --> Builder
    CFacade --> Builder
    Builder --> Loop
    Loop --> Features
    Domain -. cross-cutting domain config .-> Features
    Features --> Core
    Domain -. reuses core traits .-> Core
```

> Arrow direction = dependency / data flow (upper depends on lower). Solid lines are compile-time deps and runtime calls; dashed lines are cross-cutting declarative config. `oneai-domain` is not a layer but a declarative-config layer cross-cutting all feature layers — `AppBuilder::domain_pack(...)` switches the whole domain behavior in one line.

## Crate map

| Crate | Description |
|-------|------|
| `oneai-core` | Core types, traits, PermissionLevel, Budget, PlatformCapabilities, ModelContextResolver |
| `oneai-provider` | LLM provider (OpenAI/Anthropic/Gemini/Ollama) + ProviderPool + SmartRouter |
| `oneai-parser` | 3-layer output-parser defense |
| `oneai-memory` | Memory system (3 tiers + compression-coupled extraction + persistence, wired to the `oneai-vector` default stack) |
| `oneai-tool` | Tool registry, MCP client, InteractionGate, executor, 15 built-in tools |
| `oneai-skill` | Skill selector + registry + built-in domain skills + lifecycle |
| `oneai-domain` | DomainPack system (7 layers), CodingPack, market, spec validator |
| `oneai-agent` | AgentLoop + SubAgent + ReAct/Plan/Reflect/Explore + delegate/switch_paradigm meta-tools + GroupChat |
| `oneai-rag` | RAG + EmbeddingService (multi-provider + auto-probe + fallback) |
| `oneai-vector` | Default retrieval stack — InMemory/SqliteVec/usearch + Tantivy BM25 + BGE-M3/reranker + RRF |
| `oneai-workflow` | Workflow DAG + StateGraph + compiler + executor |
| `oneai-scheduler` | In-memory task scheduling (cron/ISO/NL, CAS at-most-once) |
| `oneai-persistence` | SQLite (sessions/LTM/usage) + file event log (working state / cross-session continuation) |
| `oneai-a2a` | A2A protocol SDK — client + server host + DomainPack→AgentCard |
| `oneai-wasm` | WASM sandbox engine — Wasmtime + WasmTool + module registry |
| `oneai-eval` | Eval framework — cases/metrics/Runner/3 suites + SWE-bench three-axis |
| `oneai-studio` | Studio Web UI — axum HTTP+WS + D3.js StateGraph visualization + Checkpoint time-travel |
| `oneai-mcp` | MCP ecosystem — host + plugin registry + config |
| `oneai-gateway` | Message gateway — axum webhook + Feishu/WeChat/Loopback adapters |
| `oneai-supervisor` | headless supervisor daemon — persistent instances + crash recovery + IPC |
| `oneai-app` | Application integration layer (AppBuilder + default retrieval-stack wiring) |
| `oneai-trace` | OpenInference-compatible trace logger + OTEL export |
| `oneai-uniffi` | UniFFI binding defs + hand-written `extern "C"` facade |
| `oneai-platform-desktop` | Desktop platform (macOS/Windows/Linux native gate) |
| `oneai-platform-android` | Android platform (JNI bridge + native gate) |
| `oneai-platform-ios` | iOS platform |
| `oneai-platform-harmony` | HarmonyOS platform |

> The whole workspace has roughly 1700 tests (per `cargo test --workspace`; per-crate counts drift so they're not listed). There's also `oneai-staticlib` (a crate-type=staticlib packaging crate, excluded from `default-members`, so not counted above).

## Module design-doc index

| Module | Doc | One-liner |
|---|---|---|
| AgentLoop / delegation / GroupChat | [multi-agent-mechanism_EN.md](multi-agent-mechanism_EN.md) | Dynamic loop + model-driven delegation + scenario multi-role chat |
| Memory | [memory-mechanism_EN.md](memory-mechanism_EN.md) | Letta 3 tiers + compression-coupled extraction + persistence |
| Context management | [context-management-mechanism_EN.md](context-management-mechanism_EN.md) | Durable/ephemeral separation + token budget + 3-layer model-context resolution |
| Working state | [working-state-mechanism_EN.md](working-state-mechanism_EN.md) | File event log + projection + cross-session continuation |
| DomainPack | [domain-pack-mechanism_EN.md](domain-pack-mechanism_EN.md) | 7-layer declarative domain config, one-line switch |
| Permissions / InteractionGate | [permission-mechanism_EN.md](permission-mechanism_EN.md) | 3-tier permissions + unified 5-point gate |
| Tool system | [tool-mechanism_EN.md](tool-mechanism_EN.md) | Tool trait + 15 tools + Footprint ladder |
| Provider / routing / parser | [provider-mechanism_EN.md](provider-mechanism_EN.md) | Provider abstraction + fallback pool + SmartRouter + 3-layer parser |
| RAG / Embedding | [rag-mechanism_EN.md](rag-mechanism_EN.md) | EmbeddingService + auto-probe + default retrieval stack |
| Workflow / StateGraph | [workflow-mechanism_EN.md](workflow-mechanism_EN.md) | DAG + cyclic graph, closed-loop with AgentLoop |
| Eval | [eval-mechanism_EN.md](eval-mechanism_EN.md) | Eval framework + SWE-bench three-axis |
| Extensions (A2A/WASM/Studio/MCP/Scheduler/Gateway/Supervisor) | [extension-mechanism_EN.md](extension-mechanism_EN.md) | Outward exposure / sandbox / visualization / scheduling / message intake |
| Tracing | [trace-mechanism_EN.md](trace-mechanism_EN.md) | OpenInference-compatible + OTEL export |
| Persistence | [persistence-mechanism_EN.md](persistence-mechanism_EN.md) | SQLite + file event log, two paths |
| Cross-platform | [cross-platform-mechanism_EN.md](cross-platform-mechanism_EN.md) | UniFFI + extern C facade, one core six targets |

> Evolution background and gap analysis in [evolution-plan-2026-07.md](evolution-plan-2026-07.md) and [gap-analysis-2026-07.md](gap-analysis-2026-07.md) (Chinese — internal planning docs).
