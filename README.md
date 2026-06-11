# OneAI

> A cross-platform AI agent framework built in Rust — modular, type-safe, and eval-ready.

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Crates: 18](https://img.shields.io/badge/Crates-18-orange.svg)]()
[![Tests: 212](https://img.shields.io/badge/Tests-212-green.svg)]()

---

## What is OneAI?

OneAI is a full-stack agent framework written in Rust. It provides everything you need to build, run, and evaluate AI agents — from LLM provider abstraction to tool execution, memory management, workflow orchestration, and trajectory logging — all with cross-platform support via UniFFI bindings.

**Key principles:**

- **Modular by design** — 18 independent crates, each with a clear responsibility. Use only what you need.
- **Type-safe throughout** — sealed enum hierarchies, trait-driven abstractions, no stringly-typed configs.
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
│ +ReAct   │ StateGrph│ Compress │ Approval │ Embedding│              │
│ +Plan    │ Compile →│          │ Executor │ Retrieval│              │
│ +Reflect │ Execute  │          │ +30 tools│          │              │
├──────────┴──────────┴──────────┴──────────┴──────────┴──────────────┤
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
| `oneai-provider` | LLM providers (OpenAI, Anthropic, Ollama) | — |
| `oneai-parser` | 3-layer output parsing defense | 12 |
| `oneai-memory` | Memory system (STM, LTM, compression, HNSW) | 20 |
| `oneai-tool` | Tool registry, MCP, approval gates, executor, 10+ tools | 32 |
| `oneai-skill` | Skill system with progressive disclosure | — |
| `oneai-agent` | AgentLoop + SubAgent + ReAct/Plan/Reflect/Parallel | 15 |
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
| **Total** | | **212** |

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

#[tokio::main]
async fn main() {
    // Build an app with auto-approval (for testing)
    let app = AppBuilder::new()
        .auto_approval_gate()
        .default_parser()
        .build()
        .expect("App build should succeed");

    // Register a tool
    app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();

    // Create a session and execute
    let session = app.create_session();
    let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+3"})).await.unwrap();
    println!("Result: {}", result.content); // → "5"
}
```

### Full Demo

```bash
cargo run -p oneai-cli-demo
```

This demonstrates the full pipeline: tools, memory, RAG, workflow, checkpoint, and trajectory logging.

---

## Core Concepts

### Agentic Loop (AgentLoop)

The core execution engine is a **dynamic loop** — not a fixed pipeline. Each iteration, the model decides what to do next:

| Decision | Action |
|----------|--------|
| **DirectAnswer** | Model produced a final answer → loop ends |
| **ToolCalls** | Model wants to invoke tools → execute and feed results back |
| **Delegate** | Model delegates a subtask to a specialized sub-agent |
| **SwitchParadigm** | Model switches to a different paradigm (Plan/Reflect/Explore) |

Iteration limits are governed by **TokenBudget** (not hardcoded `max_iterations`). When remaining budget can't support another inference, the loop auto-terminates.

### Sub-Agent System

Hierarchical task decomposition: the main agent delegates complex subtasks to specialized sub-agents (Plan, Explore, Code, Review), each running with its own context window and token budget. Sub-agents return only a **summary** to the main agent, keeping the main context window clean.

```rust
pub enum SubAgentKind { Plan, Explore, Code, Review, Custom(String) }
```

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

**Built-in tools:** ShellTool (with safety blacklist + sandbox), FileReadTool (offset+limit), FileEditTool, FileWriteTool, FileListTool, GrepTool, GlobTool, EnvironmentTool, NotebookEditTool, FileDeleteTool, CalculatorTool.

MCP integration via the `rmcp` crate — connect to any MCP-compatible tool server (stdio, SSE, streamable-http transports).

**Approval gates** control high-risk tool execution:

| Gate | Behavior |
|------|----------|
| `BlockingApprovalGate` | Always denies (safe default) |
| `AutoApprovalGate` | Always approves (testing only) |
| `ChannelApprovalGate` | Sends to platform UI for human review |
| `PlatformApprovalGate` | Native dialog (NSAlert / AlertDialog / UIAlertController) |

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
│   ├── oneai-tool/          # Registry, 10+ local tools, MCP, approval, executor
│   ├── oneai-skill/         # Skill registry + selector
│   ├── oneai-agent/         # AgentLoop, SubAgent, ReAct, Plan, Reflect, Parallel
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
│   ├── cli/                 # Interactive REPL demo
│   ├── desktop-app/         # Desktop approval gate demo
│   └── rust/                # Channel approval gate demo
├── bindings/                # Generated UniFFI bindings
├── scripts/                 # Build and binding generation scripts
└── Cargo.toml               # Workspace root
```

---

## Development Phases

| Phase | Focus | Status |
|-------|-------|--------|
| 1 | Core types, providers, parser | ✅ Complete |
| 2 | Agent paradigms (ReAct, Plan, Reflection, Parallel) | ✅ Complete |
| 3 | Memory, Tools (MCP + Approval), RAG basics | ✅ Complete |
| 4 | Workflow (Config + DAG + Executor), Persistence, Scheduler | ✅ Complete |
| 5 | AppBuilder + AppSession, UniFFI bindings | ✅ Complete |
| 6 | Platform UI + native approval gates | ✅ Complete |
| 7 | Trajectory Logger (OpenInference) | ✅ Complete |
| 8 | Agentic Loop, SubAgent, StateGraph, Budget, PermissionLevel | ✅ Complete |
| 9 | 10+ tools, ShellTool safety, MCP real impl, EmbeddingService | ✅ Complete |
| 10 | ProgressiveCheckpoint, ErrorRecovery, PromptTemplates, PlatformCapabilities | ✅ Complete |

---

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.