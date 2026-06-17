//! # OneAI WASM — Sandbox Execution Engine
//!
//! Wasmtime-based WASM sandbox execution engine for OneAI agents.
//! Implements the "Code-as-WASM-Action" paradigm — Smolagents' code-as-action
//! concept but in Rust's WASM sandbox (real process-level isolation, not AST-level).
//!
//! ## Architecture
//!
//! The WASM sandbox provides:
//! - **WasmRuntime**: Engine + Store management, fuel/epoch-based resource limiting
//! - **WasmTool**: Tool trait implementation wrapping WASM modules
//! - **WasmModuleManager**: Module loading, caching, lifecycle management
//! - **WasmActionTemplate**: Predefined templates for code-as-action execution
//! - **Host API**: Minimal host functions (log, get_env, abort) exposed to guests
//!
//! ## Security Model
//!
//! WASM modules execute in a fully sandboxed environment:
//! - No filesystem access
//! - No network access
//! - No process creation
//! - Memory limited (default: 16 pages = 1MB)
//! - Execution time limited (fuel + epoch interrupt + tokio timeout)
//! - Only 3 host functions available to guests
//!
//! ## Guest Tool Interface
//!
//! WASM guest modules must export these functions:
//! - `tool_name() -> (ptr, len)` — tool name
//! - `tool_description() -> (ptr, len)` — tool description
//! - `tool_parameters_schema() -> (ptr, len)` — JSON Schema string
//! - `tool_risk_level() -> (ptr, len)` — "low"/"medium"/"high"
//! - `execute(input_ptr, input_len) -> (output_ptr, output_len)` — main execution
//!
//! ## Usage
//!
//! ```ignore
//! let runtime = WasmRuntime::new(WasmRuntimeConfig::default())?;
//! let manager = WasmModuleManager::new(Arc::new(runtime));
//!
//! // Load a WASM tool from file
//! let wasm_tool = manager.load_from_file("calculator", Path::new("calc.wasm")).await?;
//!
//! // Register in ToolRegistry like any other tool
//! registry.register(wasm_tool).await?;
//!
//! // Execute — runs in WASM sandbox, zero I/O access
//! let result = wasm_tool.execute(serde_json::json!({"expression": "2+3"})).await?;
//! ```
//!
//! ## DomainPack Integration
//!
//! WASM tools integrate seamlessly — same `Arc<dyn Tool>` type as native tools:
//!
//! ```ignore
//! let pack = DomainPackBuilder::new("data_analysis")
//!     .tool(wasm_sort_tool)    // WASM tool — transparent
//!     .tool(Arc::new(FileReadTool::new()))  // native tool
//!     .build();
//! ```

pub mod error;
pub mod config;
pub mod runtime;
pub mod guest_api;
pub mod module;
pub mod tool;
pub mod action_template;

// ─── Public exports ──────────────────────────────────────────────────────────────

pub use error::{WasmError, Result};
pub use config::WasmRuntimeConfig;
pub use runtime::WasmRuntime;
pub use guest_api::{WasmGuestApi, WasmHostFunction};
pub use module::WasmModuleManager;
pub use tool::{WasmTool, WasmToolMetadata};
pub use action_template::{WasmActionTemplate, WasmActionTool, WasmActionKind};
