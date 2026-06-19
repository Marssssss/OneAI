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
//! - **WASI**: Restricted filesystem access with whitelisted directories
//!
//! ## Security Model
//!
//! WASM modules execute in a fully sandboxed environment:
//! - No filesystem access (unless WASI is explicitly configured)
//! - No network access
//! - No process creation
//! - Memory limited (default: 16 pages = 1MB)
//! - Execution time limited (fuel + epoch interrupt + tokio timeout)
//! - Only 3 host functions available to guests (plus WASI if enabled)
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
//! ## WASI Access
//!
//! When WASI is enabled via `WasiConfig`, guest modules can access
//! whitelisted directories on the host filesystem. This is the only
//! I/O capability available in the WASM sandbox — network and process
//! spawning remain blocked.
//!
//! ```ignore
//! let wasi_config = WasiConfig::restricted(vec![
//!     WasiDirConfig::readonly(PathBuf::from("/tmp/data"), "/data"),
//! ]);
//! let runtime = WasmRuntime::new(WasmRuntimeConfig::default().with_wasi_config(wasi_config))?;
//! ```
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

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.


pub mod error;
pub mod config;
pub mod runtime;
pub mod wasi;
pub mod guest_api;
pub mod module;
pub mod registry;
pub mod monitor;
pub mod tool;
pub mod action_template;

// ─── Public exports ──────────────────────────────────────────────────────────────

pub use error::{WasmError, Result};
pub use config::WasmRuntimeConfig;
pub use runtime::{WasmRuntime, WasmHostState, WasmStoreState};
pub use wasi::{WasiConfig, WasiDirConfig, WasiAccessMode};
pub use guest_api::{WasmGuestApi, WasmHostFunction};
pub use module::WasmModuleManager;
pub use registry::{WasmModuleRegistry, WasmModuleEntry, WasmModuleSource, WasmModuleVersion, WasmModuleHealth};
pub use monitor::{WasmResourceMonitor, WasmExecutionMetrics, WasmResourceEvent, WasmResourceEventSubscriber, WasmLogSubscriber};
pub use tool::{WasmTool, WasmToolMetadata};
pub use action_template::{WasmActionTemplate, WasmActionTool, WasmActionKind, WasmActionExecutionMode};
