//! # OneAI Studio — Playground/Studio Web UI
//!
//! OneAI Studio is a visual debugging environment for OneAI agents, inspired by
//! LangGraph Studio. It provides:
//!
//! - **StateGraph visualization**: Nodes + edges + current execution position as SVG/D3.js
//! - **AgentLoop real-time tracking**: Each iteration's decisions, tool calls, results
//! - **Checkpoint time-travel**: Select any checkpoint to inspect or restore state
//! - **Trace metrics dashboard**: Success rate, token cost, latency, tool accuracy
//!
//! ## Architecture
//!
//! - **Backend**: Rust (axum HTTP + WebSocket server)
//! - **Frontend**: Vanilla HTML + JavaScript + D3.js/SVG
//! - **Data pipeline**: StudioState implements AgentLoopObserver → broadcast → WebSocket
//!
//! ## Usage
//!
//! ```ignore
//! // Start Studio server (default port 3000)
//! oneai studio
//!
//! // Custom port
//! oneai studio --port 8080
//!
//! // From Rust code
//! use oneai_studio::{StudioConfig, serve_with_state};
//! let config = StudioConfig::with_port(3000);
//! serve_with_state(config, studio_state).await?;
//! ```
//!
//! ## Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate.

pub mod graph_dto;
pub mod trace_dto;
pub mod checkpoint_dto;
pub mod state;
pub mod server;
pub mod routes;
pub mod ws;
pub mod handlers;

pub use state::{StudioState, StudioEvent, SessionView, SessionUpdate, StudioRunner, RunnerStatus, RunOutcome};
pub use server::{StudioConfig, serve, serve_with_state};
pub use graph_dto::{GraphVisualization, NodeView, EdgeView};
pub use trace_dto::{TraceTreeView, SpanView, EventView, MetricsView};
pub use checkpoint_dto::{CheckpointListView, CheckpointDetailView, CheckpointEntryView};
pub use handlers::{RunRequest};
