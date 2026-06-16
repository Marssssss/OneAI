# OneAI

> A cross-platform AI agent framework built in Rust — modular, type-safe, domain-pluggable, and eval-ready.

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Crates: 19](https://img.shields.io/badge/Crates-19-orange.svg)]()
[![Tests: 257](https://img.shields.io/badge/Tests-257-green.svg)]()

---

## What is OneAI?

OneAI is a full-stack agent framework written in Rust. It provides everything you need to build, run, and evaluate AI agents — from LLM provider abstraction to tool execution, memory management, workflow orchestration, domain-specific configuration, and trajectory logging — all with cross-platform support via UniFFI bindings.

**Key principles:**

- **Modular by design** — 19 independent crates, each with a clear responsibility. Use only what you need.
- **Type-safe throughout** — sealed enum hierarchies, trait-driven abstractions, no stringly-typed configs.
- **Domain-pluggable** — DomainPack system makes domain knowledge declarative, composable, and switchable with a single line.
- **Cross-platform** — runs on macOS, Windows, Linux, Android, iOS, and HarmonyOS via UniFFI (Kotlin, Swift, C++, C#).
- **Eval-ready** — built-in OpenInference-compatible trajectory logger for agent evaluation (success rate, cost, latency, fault tolerance).
- **Human-machine collaboration** — approval gates with native UI dialogs for high-risk tool operations.
- **Dynamic agentic loop** — not a fixed pipeline; each iteration decides dynamically (direct answer, tool call, delegate to sub-agent, or switch paradigm).

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        oneai-app (Integration)                       │
│  AppBuilder → App → AppSession                                       │
│  Wires all modules together; entry point for applications            │
├──────────┬──────────┬──────────┬──────────┬──────────┬──────────────┤
│ oneai-   │ oneai-   │ oneai-   │ oneai-   │ oneai-   │ oneai-       │
│ agent    │ workflow │ memory   │ tool     │ rag      │ skill        │
│          │          │          │          │          │              │
│ AgentLoop│ Config → │ STM +    │ Registry │ Document │ Selector     │
│ +SubAgent│ DAG +    │ LTM +    │ + MCP +  │ Index +  │ + Registry   │
│ +ReAct   │ StateGrph│ Compress │ Approval │ Embedding│ + Built-in   │
│ +Plan    │ Compile →│          │ Executor │ Retrieval│   domain     │
│ +Reflect │ Execute  │          │ +12 tools│          │   skills     │
│ +Stream  │          │          │          │          │              │
│ +CtxAsmbl│          │          │          │          │              │
├──────────┴──────────┴──────────┴──────────┴──────────┴──────────────┤
│                     oneai-domain (Domain Configuration)               │
│  DomainPack (5-layer), CodingPack, ToolDecorator, ContextSource,     │
│  PermissionProfile, ParadigmStrategy, CompressionTemplate, Merge     │
├──────────────────────────────────────────────────────────────────────┤
│                     oneai-core (Foundation)                          │
│  ContentBlock, Message, Conversation, PermissionLevel, Budget,      │
│  ContextBudgetManager, PlatformCapabilities, Traits                  │
├──────────────────────────────┬──────────────────────────────────────┤
│     oneai-provider           │  oneai-parser                        │
│  OpenAI / Anthropic / Ollama │  3-layer parsing defense             │
├──────────────────────────────┼──────────────────────────────────────┤
│     oneai-persistence        │  oneai-scheduler                     │
│  ProgressiveCheckpoint +     │  In-memory task scheduling           │
│  Memory/SQLite/Postgres      │                                      │
├──────────────────────────────┼──────────────────────────────────────┤
│     oneai-trace              │  oneai-uniffi                        │
│  OpenInference trajectory    │  Kotlin / Swift / C++ / C# bindings  │
├──────────────────────────────┴──────────────────────────────────────┤
│                Platform Crates                                       │
│  oneai-platform-desktop / android / ios / harmony                    │
│  Native approval gates + PlatformCapabilities                        │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Crate Overview

| Crate | Description | Tests |
|-------|-------------|-------|
| `oneai-core` | Core types, traits, PermissionLevel, ContextBudgetManager, PlatformCapabilities | 28 |
| `oneai-provider` | LLM providers (OpenAI, Anthropic, Ollama) | 6 |
| `oneai-parser` | 3-layer output parsing defense | 12 |
| `oneai-memory` | Memory system (STM, LTM, compression, HNSW) | 20 |
| `oneai-tool` | Tool registry, MCP, approval gates, executor, 12 tools | 32 |
| `oneai-skill` | Skill system with selector + 16 built-in domain skills | — |
| `oneai-domain` | DomainPack system (5-layer config), CodingPack, merge | 40 |
| `oneai-agent` | AgentLoop + SubAgent + ReAct/Plan/Reflect + StreamParser + ContextAssembler | 15 |
| `oneai-rag` | RAG with EmbeddingService (FastEmbed/Ollama/OpenAI) | 20 |
| `oneai-workflow` | Workflow DAG + StateGraph + executor | 26 |
| `oneai-scheduler` | In-memory task scheduling | 6 |
| `oneai-persistence` | ProgressiveCheckpoint + backends (Memory/SQLite/Postgres) | 5 |
| `oneai-app` | Application integration layer (AppBuilder) | 7 |
| `oneai-trace` | OpenInference-compatible trajectory logger | 14 |
| `oneai-uniffi` | UniFFI binding definitions for FFI | 20 |
| `oneai-platform-desktop` | Desktop platform (macOS/Windows/Linux) | 2 |
| `oneai-platform-android` | Android platform | 2 |
| `oneai-platform-ios` | iOS platform | 1 |
| `oneai-platform-harmony` | HarmonyOS platform | 1 |
| **Total** | | **257** |

---

## Quick Start

### Build

```bash
# Clone the repository
git clone https://github.com/oneai-project/oneai.git
cd oneai

# Build all crates
cargo build

# Run all tests
cargo test
```

### Minimal Example

```rust
use std::sync::Arc;
use oneai_app::AppBuilder;
use oneai_tool::CalculatorTool;
use oneai_domain::coding_pack;

#[tokio::main]
async fn main() {
    // Build an app with a coding domain pack (one-line domain switch)
    let app = AppBuilder::new()
        .auto_approval_gate()
        .default_parser()
        .domain_pack(coding_pack("/project/dir"))  // ← domain switch
        .build()
        .expect("App build should succeed");

    // Create a session and execute
    let session = app.create_session();
    let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+3"})).await.unwrap();
    println!("Result: {}", result.content); // → "5"
}
```

### Full Demo (TUI)

```bash
cargo run -p oneai-cli
```

This launches an interactive TUI (ratatui + crossterm) demonstrating the full pipeline: tools, memory, RAG, workflow, checkpoint, trajectory logging, and domain packs.

---

## Core Concepts

### Domain Pack System

The DomainPack is OneAI's key architectural innovation — it makes domain knowledge **declarative, pluggable, and composable** instead of hardcoded.

> "Coding Agent embeds workflow via 5-layer implicit configuration. OneAI makes these 5 layers declarative, pluggable, and composable."

A DomainPack encapsulates 5 layers of domain-specific configuration:

| Layer | Component | Purpose |
|-------|-----------|---------|
| 1 | **Tools + ToolDecorators** | Domain-specific tool set and description overrides |
| 2 | **ContextSources** | Domain-specific environment sensing with refresh policies |
| 3 | **PermissionProfile** | Domain-specific permission classification (deny/auto/confirm) |
| 4 | **ParadigmStrategies** | Domain-specific task → paradigm mapping |
| 5 | **CompressionTemplate** | Domain-specific context preservation priorities |

Switch domains with one line:

```rust
let app = AppBuilder::new()
    .provider(provider)
    .domain_pack(coding_pack("/project/dir"))  // ← one-line domain switch
    .build()?;
```

DomainPacks can be **merged** for multi-domain agents (e.g., coding + research) using strictest-wins rules for permissions and priority-based merging for context sources.

#### CodingPack (Built-in)

The first concrete DomainPack, modeled after Claude Code's workflow embedding:

- **9 tools**: FileRead, FileEdit, Shell, Grep, Glob, FileList, NotebookEdit, Environment, WebFetch
- **8 tool decorators**: coding-specific descriptions (e.g., Shell described for compilation/testing, not general command execution)
- **6 context sources**: ProjectInstructions, GitStatus, FileTree, ProjectConfig, Date, EnvironmentInfo
- **Permission profile**: auto-approve reads, confirm edits/shell, deny dangerous commands (`rm -rf`, `mkfs`)
- **4 paradigm strategies**: refactor → Plan+ReAct+Reflect, bug → Plan+ReAct, search → Explore, implement → ReAct
- **3 sub-agent types**: searcher (read+grep+glob), coder (edit+shell), reviewer (read+grep)
- **Compression template**: preserve file paths, progress status, key decisions

#### ToolDecorator

Overrides tool descriptions for domain context — ShellTool's description changes from generic to "Execute shell commands for compilation, testing, and running scripts" in the coding domain.

#### ContextSource

Pluggable environment sensing with independent refresh policies:

| Policy | Behavior |
|--------|----------|
| `EveryIteration` | Refresh on each loop (git status) |
| `OnChange` | Refresh only when diff detected (file tree) |
| `OnceAtStart` | Load once, never refresh (project config) |
| `Periodic(Duration)` | Refresh at fixed interval (date/time) |

#### PermissionProfile

Domain-level permission overrides with resolution order:

1. `deny_by_default` → always block matching patterns (highest priority)
2. `permission_overrides` → override tool's default PermissionLevel
3. `auto_approve` → skip approval gate
4. `require_confirmation` → always route through approval gate
5. Fall back to tool's own `risk_level()`

### Agentic Loop (AgentLoop)

The core execution engine is a **dynamic loop** — not a fixed pipeline. Each iteration, the model decides what to do next:

| Decision | Action |
|----------|--------|
| **DirectAnswer** | Model produced a final answer → loop ends |
| **ToolCalls** | Model wants to invoke tools → execute and feed results back |
| **Delegate** | Model delegates a subtask to a specialized sub-agent |
| **SwitchParadigm** | Model switches to a different paradigm (Plan/Reflect/Explore) |

Iteration limits are governed by **TokenBudget** (not hardcoded `max_iterations`). When remaining budget can't support another inference, the loop auto-terminates.

### Incremental Stream Parser

Real-time detection of tool_use blocks during streaming — the UI can show the agent's intent before arguments are fully generated. Inspired by Claude Code's incremental parsing approach, replacing the old "collect full stream before processing" model.

### ContextAssembler

Constructs conversation context for each loop iteration, with automatic **environment diff detection** — detects changes (new files, git modifications, directory changes) between iterations and injects them as context even when no tool explicitly reported them.

### Sub-Agent System

Hierarchical task decomposition: the main agent delegates complex subtasks to specialized sub-agents. Domain packs define custom sub-agent types via `SubAgentTypeDefinition`:

```rust
pub enum SubAgentKind { Plan, Explore, Code, Review, Custom(String) }
```

Custom sub-agents (defined in DomainPack) have domain-specific system prompts, tool subsets, and permission thresholds.

### Permission Levels

Three-tier permission system replacing the old `RiskLevel`:

| Level | Scope | Auto-approve? |
|-------|-------|----------------|
| **Read** | File read, search, environment sensing | Yes |
| **Standard** | File edit, MCP interaction | Depends on policy |
| **Full** | Shell execution, file deletion, system commands | Requires approval |

### LLM Providers

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse>;
    async fn infer_stream(&self, req: InferenceRequest) -> Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>>;
    fn capabilities(&self) -> ModelCapability;
    fn config(&self) -> &ModelConfig;
}
```

Three providers are included: **OpenAI**, **Anthropic**, and **Ollama**.

### Agent Paradigms

| Paradigm | Pattern | Use Case |
|----------|---------|----------|
| **ReAct** | Reason → Act → Observe loop | General tool-calling tasks |
| **Plan** | Decompose → ordered step list | Complex multi-step tasks |
| **Reflection** | Verify → suggest corrections | Quality assurance, self-check |
| **Parallel** | ScopeState isolation → merge | Independent sub-tasks |
| **Explore** | Search → understand → summarize | Codebase/search exploration |

All agents use `ScopeState` for safe parallel execution.

### Tool System

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn risk_level(&self) -> RiskLevel;
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput>;
}

pub trait PermissionAwareTool: Tool {
    fn permission_level(&self) -> PermissionLevel;
}
```

**Built-in tools (12):** ShellTool (with safety blacklist + sandbox), FileReadTool (offset+limit), FileEditTool, FileWriteTool, FileListTool, GrepTool, GlobTool, EnvironmentTool, NotebookEditTool, FileDeleteTool, CalculatorTool, WebFetchTool.

MCP integration via the `rmcp` crate — connect to any MCP-compatible tool server (stdio, SSE, streamable-http transports).

**Approval gates** control high-risk tool execution:

| Gate | Behavior |
|------|----------|
| `BlockingApprovalGate` | Always denies (safe default) |
| `AutoApprovalGate` | Always approves (testing only) |
| `ChannelApprovalGate` | Sends to platform UI for human review |
| `PlatformApprovalGate` | Native dialog (NSAlert / AlertDialog / UIAlertController) |

### Skill System

Progressive disclosure of agent capabilities via skills. Built-in skills organized by domain:

| Domain | Skills |
|--------|--------|
| **Coding** (8) | project-planning, code-review, debug-analysis, refactoring, test-strategy, documentation, git-workflow, dependency-analysis |
| **Research** (5) | deep-research, academic-search, data-extraction, citation-management, fact-verification |
| **General** (3) | summarization, translation, creative-writing |

Skills are matched via `SkillSelector` using trigger keywords or embeddings, and injected into agent context on activation.

### Memory System

- **Short-term memory** — sliding window with configurable size, automatic eviction to long-term
- **Long-term memory** — embedded HNSW-like vector store + content store + hybrid scoring
- **Context compression** — summarization when token count exceeds threshold, keeping recent turns intact
- **ContextBudgetManager** — automatic per-iteration compression, proportional budget allocation across context sources

### 3-Layer Output Parser

LLM outputs are notoriously unreliable. OneAI defends with three layers:

1. **Constrained decoding** — BNF grammar guides the model's output format
2. **Fuzzy JSON repair** — bracket closing, regex extraction, embedded JSON detection
3. **Fallback self-correction** — re-prompt the model to fix its own output

### Workflow Engine

- **WorkflowDag** — declarative DAG for parallel step orchestration
- **StateGraph** — cyclic directed graph for agent flows needing iteration (ReAct loops, conditional routing, interrupt points)

### RAG (Retrieval-Augmented Generation)

- **EmbeddingService** — FastEmbed (local ONNX), Ollama, or OpenAI embeddings
- **DocumentIndex** — automatic embedding generation during `add_document()`
- **Chunking strategies** — SentenceBoundary, FixedSize, Paragraph

### Error Recovery

Systematic error recovery beyond LLM self-judgment:

| Strategy | Description |
|----------|-------------|
| **Retry** | Configurable retry policies |
| **ConditionalFallback** | Error → correction path |
| **Rollback** | State rollback from checkpoint |
| **Assertion** | Constraint hooks for interception |
| **ExternalFeedback** | Test results, compilation, API status codes |

### Progressive Checkpoint

Auto-save per iteration with multiple backends:

| Backend | Use Case |
|---------|----------|
| **MemoryCheckpointBackend** | Development/testing |
| **SqliteCheckpointBackend** | Single-device production |
| **PostgresCheckpointBackend** | Server-side production |

Auto-save policies: EveryStep, EveryNSteps, CriticalNodes. Support interrupt, replay, and fork from any checkpoint.

### Trajectory Logging (Trace)

OpenInference-compatible trace mechanism for agent evaluation:

```rust
let app = AppBuilder::new()
    .trace_in_memory()  // or .trace_to_file("/tmp/trace.json")
    .build()?;

session.end_session(SpanStatus::Ok);
let tree = session.build_trace_tree();
println!("Success rate: {:.1}%", tree.metrics.success_rate * 100.0);
```

---

## Cross-Platform Support

OneAI uses UniFFI to generate foreign-language bindings:

| Platform | Binding Language | Approval Gate | PlatformCapabilities |
|----------|-----------------|---------------|----------------------|
| macOS / Windows / Linux | C++ / C# | NSAlert / MessageBox | Screenshot, FilesystemSandbox, Notifications |
| Android | Kotlin | AlertDialog | Camera, Screenshot, Network |
| iOS | Swift | UIAlertController | Camera (limited), Screenshot |
| HarmonyOS | C++ | CommonDialog | Camera, AppSandbox |

---

## Project Structure

```
oneai/
├── crates/
│   ├── oneai-core/          # Foundation: types, traits, PermissionLevel, Budget, PlatformCapabilities
│   ├── oneai-provider/      # LLM providers (OpenAI, Anthropic, Ollama)
│   ├── oneai-parser/        # 3-layer output parsing
│   ├── oneai-memory/        # STM, LTM, compression, HNSW, MemoryManager
│   ├── oneai-tool/          # Registry, 12 local tools, MCP, approval, executor
│   ├── oneai-skill/         # Skill registry + selector + 16 built-in domain skills
│   ├── oneai-domain/        # DomainPack system (5-layer), CodingPack, merge
│   ├── oneai-agent/         # AgentLoop, SubAgent, ReAct, Plan, Reflect, StreamParser, ContextAssembler
│   ├── oneai-rag/           # Document, index, EmbeddingService, retrieval
│   ├── oneai-workflow/      # DAG, StateGraph, compiler, validator, executor
│   ├── oneai-scheduler/     # InMemoryScheduler
│   ├── oneai-persistence/   # ProgressiveCheckpoint, Memory/SQLite/Postgres backends
│   ├── oneai-app/           # AppBuilder, App, AppSession
│   ├── oneai-trace/         # OpenInference trajectory logger
│   ├── oneai-uniffi/        # UniFFI binding definitions
│   ├── oneai-platform-desktop/
│   ├── oneai-platform-android/
│   ├── oneai-platform-ios/
│   └── oneai-platform-harmony/
├── examples/
│   ├── cli/                 # Interactive TUI demo (ratatui + crossterm)
│   ├── desktop-app/         # Desktop approval gate demo
│   ├── rust/                # Channel approval gate demo
│   ├── android-app/         # Android demo (Kotlin)
│   └── ios-app/             # iOS demo (Swift)
├── bindings/                # Generated UniFFI bindings
├── scripts/                 # Build and binding generation scripts
└── Cargo.toml               # Workspace root
```

---

## Development Phases

| Phase | Focus | Status |
|-------|-------|--------|
| 1 | Core types, providers, parser | ✅ Complete |
| 2 | Agent paradigms (ReAct, Plan, Reflection, Parallel, Explore) | ✅ Complete |
| 3 | Memory, Tools (MCP + Approval), RAG basics | ✅ Complete |
| 4 | Workflow (Config + DAG + Executor), Persistence, Scheduler | ✅ Complete |
| 5 | AppBuilder + AppSession, UniFFI bindings | ✅ Complete |
| 6 | Platform UI + native approval gates | ✅ Complete |
| 7 | Trajectory Logger (OpenInference) | ✅ Complete |
| 8 | Agentic Loop, SubAgent, StateGraph, Budget, PermissionLevel | ✅ Complete |
| 9 | 12 tools, ShellTool safety, MCP real impl, EmbeddingService, WebFetchTool | ✅ Complete |
| 10 | ProgressiveCheckpoint, ErrorRecovery, PromptTemplates, PlatformCapabilities | ✅ Complete |
| 11 | DomainPack system (5-layer), CodingPack, Skill built-ins, StreamParser, ContextAssembler, TUI | ✅ Complete |

---

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.
