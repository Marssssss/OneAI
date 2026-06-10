# OneAI

> A cross-platform AI agent framework built in Rust — modular, type-safe, and eval-ready.

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Crates: 18](https://img.shields.io/badge/Crates-18-orange.svg)]()
[![Tests: 211](https://img.shields.io/badge/Tests-211-green.svg)]()

---

## What is OneAI?

OneAI is a full-stack agent framework written in Rust. It provides everything you need to build, run, and evaluate AI agents — from LLM provider abstraction to tool execution, memory management, workflow orchestration, and trajectory logging — all with cross-platform support via UniFFI bindings.

**Key principles:**

- **Modular by design** — 18 independent crates, each with a clear responsibility. Use only what you need.
- **Type-safe throughout** — sealed enum hierarchies, trait-driven abstractions, no stringly-typed configs.
- **Cross-platform** — runs on macOS, Windows, Linux, Android, iOS, and HarmonyOS via UniFFI (Kotlin, Swift, C++, C#).
- **Eval-ready** — built-in OpenInference-compatible trajectory logger for agent evaluation (success rate, cost, latency, fault tolerance).
- **Human-machine collaboration** — approval gates with native UI dialogs for high-risk tool operations.

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
│ ReAct    │ Config → │ STM +    │ Registry │ Document │ Selector     │
│ Plan     │ DAG →    │ LTM +    │ + MCP +  │ Index +  │ + Registry   │
│ Reflect  │ Compile →│ Compress │ Approval │ Retrieval│              │
│ Parallel │ Execute  │          │ Executor │          │              │
├──────────┴──────────┴──────────┴──────────┴──────────┴──────────────┤
│                     oneai-core (Foundation)                          │
│  ContentBlock, Message, Conversation, ModelConfig, Traits            │
├──────────────────────────────┬──────────────────────────────────────┤
│     oneai-provider           │  oneai-parser                        │
│  OpenAI / Anthropic / Ollama │  3-layer parsing defense             │
├──────────────────────────────┼──────────────────────────────────────┤
│     oneai-persistence        │  oneai-scheduler                     │
│  File-based checkpoints      │  In-memory task scheduling           │
├──────────────────────────────┼──────────────────────────────────────┤
│     oneai-trace              │  oneai-uniffi                        │
│  OpenInference trajectory    │  Kotlin / Swift / C++ / C# bindings  │
├──────────────────────────────┴──────────────────────────────────────┤
│                Platform Crates                                       │
│  oneai-platform-desktop / android / ios / harmony                    │
│  Native approval gates (NSAlert, AlertDialog, UIAlertController)     │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Crate Overview

| Crate | Description | Tests |
|-------|-------------|-------|
| `oneai-core` | Core types, traits, and abstractions | 28 |
| `oneai-provider` | LLM providers (OpenAI, Anthropic, Ollama) | — |
| `oneai-parser` | 3-layer output parsing defense | 12 |
| `oneai-memory` | Memory system (STM, LTM, compression, HNSW) | 20 |
| `oneai-tool` | Tool registry, MCP, approval gates, executor | 32 |
| `oneai-skill` | Skill system with progressive disclosure | — |
| `oneai-agent` | Agent paradigms (ReAct, Plan, Reflection, Parallel) | 15 |
| `oneai-rag` | Retrieval-Augmented Generation | 20 |
| `oneai-workflow` | Workflow compiler, DAG, validator, executor | 26 |
| `oneai-scheduler` | In-memory task scheduling | 6 |
| `oneai-persistence` | State persistence and checkpoint management | 5 |
| `oneai-app` | Application integration layer (AppBuilder) | 7 |
| `oneai-trace` | OpenInference-compatible trajectory logger | 14 |
| `oneai-uniffi` | UniFFI binding definitions for FFI | 20 |
| `oneai-platform-desktop` | Desktop platform (macOS/Windows/Linux) | 2 |
| `oneai-platform-android` | Android platform | 2 |
| `oneai-platform-ios` | iOS platform | 1 |
| `oneai-platform-harmony` | HarmonyOS platform | 1 |
| **Total** | | **211** |

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

### LLM Providers

OneAI abstracts LLM inference behind the `LlmProvider` trait:

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse>;
    async fn infer_stream(&self, req: InferenceRequest) -> Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>>;
    fn capabilities(&self) -> ModelCapability;
    fn config(&self) -> &ModelConfig;
}
```

Three providers are included:

- **OpenAI** — GPT-4, GPT-3.5, and any OpenAI-compatible API
- **Anthropic** — Claude models with streaming support
- **Ollama** — Local models via the Ollama runtime

### Agent Paradigms

| Paradigm | Pattern | Use Case |
|----------|---------|----------|
| **ReAct** | Reason → Act → Observe loop | General tool-calling tasks |
| **Plan** | Decompose → ordered step list | Complex multi-step tasks |
| **Reflection** | Verify → suggest corrections | Quality assurance, self-check |
| **Parallel** | ScopeState isolation → merge | Independent sub-tasks |

All agents use `ScopeState` for safe parallel execution — local sandboxes that only merge results back to global state via explicit `Reduction` operations.

### Tool System

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn risk_level(&self) -> RiskLevel;    // Low, Medium, High
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput>;
}
```

Built-in tools: `CalculatorTool`, `ShellTool`, `FileReadTool`, `FileWriteTool`.

MCP integration via the `rmcp` crate — connect to any MCP-compatible tool server.

**Approval gates** control high-risk tool execution:

| Gate | Behavior |
|------|----------|
| `BlockingApprovalGate` | Always denies (safe default) |
| `AutoApprovalGate` | Always approves (testing only) |
| `ChannelApprovalGate` | Sends to platform UI for human review |
| `PlatformApprovalGate` | Native dialog (NSAlert / AlertDialog / UIAlertController) |

### Memory System

- **Short-term memory** — sliding window with configurable size, automatic eviction to long-term
- **Long-term memory** — embedded HNSW-like vector store + content store + hybrid scoring (semantic similarity × temporal proximity)
- **Context compression** — summarization when token count exceeds threshold, keeping recent turns intact
- **MemoryManager** — unified interface that coordinates STM ↔ LTM ↔ compression

### 3-Layer Output Parser

LLM outputs are notoriously unreliable. OneAI defends against malformed output with three layers:

1. **Constrained decoding** — BNF grammar guides the model's output format
2. **Fuzzy JSON repair** — bracket closing, regex extraction, embedded JSON detection
3. **Fallback self-correction** — re-prompt the model to fix its own output

```rust
let parser = ThreeLayerParser::new();
let result: ParsingResult = parser.parse(raw_llm_output).await?;
```

### Workflow Engine

Define workflows as declarative configs → compile to DAG → execute level-by-level with automatic parallel execution:

```rust
let config = WorkflowConfig::new("data_pipeline", vec![
    StepConfig { id: "fetch", depends_on: vec![], tool: Some("http_get"), .. },
    StepConfig { id: "parse", depends_on: vec!["fetch"], tool: Some("json_parser"), .. },
    StepConfig { id: "store", depends_on: vec!["parse"], tool: Some("db_write"), .. },
]);

let result = session.execute_workflow(&config).await?;
```

Features: timeout policies, retry strategies, approval checkpoints, continue-on-failure mode.

### RAG (Retrieval-Augmented Generation)

```rust
let mut index = DocumentIndex::with_defaults(vector_store);
let mut doc = Document::with_id("guide", "Rust is a systems language...");
doc.chunk(&ChunkingStrategy::SentenceBoundary { max_chunk_size: 200 });
index.add_document(doc)?;

let results = index.search_by_keyword("systems language", 5);
```

Chunking strategies: `SentenceBoundary`, `FixedSize`, `Paragraph`.

### Trajectory Logging (Trace)

OpenInference-compatible trace mechanism for agent evaluation:

```rust
let app = AppBuilder::new()
    .trace_in_memory()  // or .trace_to_file("/tmp/trace.json")
    .build()?;

// ... run your agent session ...

session.end_session(SpanStatus::Ok);
let tree = session.build_trace_tree();
println!("Success rate: {:.1}%", tree.metrics.success_rate * 100.0);
println!("Tool calls: {}", tree.metrics.tool_call_count);
println!("Cost: ${:.4}", tree.metrics.estimated_cost_usd);
```

**Metrics tracked:** success_rate, total_tokens, estimated_cost_usd, avg_inference_latency_ms, tool_call_count, tool_success_rate, approval_denial_rate, parser_fallback_rate, total_retries, workflow_step_success_rate, avg_iterations, checkpoint_count, error_count.

**Conditional compilation:** Disable the `trace` feature for zero-cost stubs that compile away completely.

---

## Cross-Platform Support

OneAI uses UniFFI to generate foreign-language bindings from Rust types:

| Platform | Binding Language | Approval Gate |
|----------|-----------------|---------------|
| macOS / Windows / Linux | C++ / C# | NSAlert / MessageBox |
| Android | Kotlin | AlertDialog |
| iOS | Swift | UIAlertController |
| HarmonyOS | C++ | CommonDialog |

```bash
# Generate bindings
./scripts/generate_bindings.sh
```

The `ProviderFactory` and `AppBuilder` are exposed as UniFFI objects — foreign code creates concrete instances without needing trait objects.

---

## Persistence

File-based checkpoint management for agent state recovery:

```rust
let persistence = Arc::new(FilePersistence::new("/tmp/checkpoints"));
let app = AppBuilder::new()
    .persistence(persistence)
    .build()?;

let checkpoint_id = session.save_checkpoint().await?;
// Later: load from checkpoint to resume a long-running agent
```

---

## Project Structure

```
oneai/
├── crates/
│   ├── oneai-core/          # Foundation: types, traits, error, platform
│   ├── oneai-provider/      # LLM providers (OpenAI, Anthropic, Ollama)
│   ├── oneai-parser/        # 3-layer output parsing
│   ├── oneai-memory/        # STM, LTM, compression, HNSW, MemoryManager
│   ├── oneai-tool/          # Registry, local/MCP tools, approval, executor
│   ├── oneai-skill/         # Skill registry + selector
│   ├── oneai-agent/         # ReAct, Plan, Reflection, Parallel, AgentRunner
│   ├── oneai-rag/           # Document, index, retrieval
│   ├── oneai-workflow/      # Config, DAG, compiler, validator, executor
│   ├── oneai-scheduler/     # InMemoryScheduler
│   ├── oneai-persistence/   # FilePersistence, checkpoint, state
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
│   ├── rust/                # Channel approval gate demo
│   ├── android-app/         # Android app demo
│   └── ios-app/             # iOS app demo
├── bindings/                # Generated UniFFI bindings (Kotlin, Swift, C++, C#)
├── scripts/                 # Build and binding generation scripts
├── tests/                   # Integration tests
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
| 5 | AppBuilder + AppSession, UniFFI bindings, 168 tests | ✅ Complete |
| 6 | Platform UI + native approval gates | ✅ Complete |
| 7 | Trajectory Logger (OpenInference), 211 tests | ✅ Complete |

---

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.