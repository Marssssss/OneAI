//! # OneAI Workflow
//!
//! Workflow compiler, DAG, validator, executor, StateGraph (cyclic graph support),
//! and StateGraphExecutor (cyclic graph execution).
//!
//! A workflow is a declarative specification of multi-step agent behavior.
//! It defines steps, dependencies, tool bindings, and execution policies.
//! The workflow is compiled into a DAG (for acyclic workflows) or a
//! StateGraph (for cyclic workflows like ReAct loops) and executed.
//!
//! P2-2: GraphActionExecutor trait enables AgentLoop integration —
//! LlmInfer/ToolCall nodes can delegate to the AgentLoop's full pipeline
//! (hooks, permission, domain pack, tool definitions, context assembly).

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


pub mod config;
pub mod dag;
pub mod compiler;
pub mod validator;
pub mod executor;
pub mod state_graph;
pub mod state_executor;
pub mod render;

pub use config::*;
pub use dag::*;
pub use compiler::*;
pub use validator::*;
pub use executor::*;
pub use state_graph::*;
pub use state_executor::*;
pub use render::*;