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