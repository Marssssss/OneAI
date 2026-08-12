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

pub mod checkpoint_dto;
pub mod graph_dto;
pub mod handlers;
pub mod routes;
pub mod server;
pub mod state;
pub mod trace_dto;
pub mod ws;

pub use checkpoint_dto::{CheckpointDetailView, CheckpointEntryView, CheckpointListView};
pub use graph_dto::{EdgeView, GraphVisualization, NodeView};
pub use handlers::RunRequest;
pub use server::{serve, serve_with_state, StudioConfig};
pub use state::{RunOutcome, RunnerStatus, SessionUpdate, SessionView, StudioRunner, StudioState};
pub use trace_dto::{EventView, MetricsView, SpanView, TraceTreeView};
