//! Route definitions — sets up all REST API endpoints and WebSocket route.

use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;

use crate::state::StudioState;
use crate::handlers;
use crate::ws;

// ─── Build Router ────────────────────────────────────────────────────

/// Build the axum Router with all Studio endpoints.
///
/// Routes:
/// - `/` → Studio HTML index page
/// - `/ws` → WebSocket real-time events
/// - `/api/session` → List sessions
/// - `/api/session/:id` → Get session details
/// - `/api/session/:id/trace` → Get trace tree
/// - `/api/session/:id/metrics` → Get trace metrics
/// - `/api/graph` → List available StateGraphs
/// - `/api/graph/:name` → Get StateGraph visualization
/// - `/api/checkpoint` → List checkpoints
/// - `/api/checkpoint/:id` → Get checkpoint details
/// - `/api/checkpoint/:id/restore` → Restore from checkpoint
/// - `/api/domain-pack` → List DomainPacks
/// - `/api/domain-pack/:name` → Get DomainPack details
/// - `/api/tools` → List registered tools
pub fn build_router(state: Arc<StudioState>) -> Router {
    Router::new()
        // Index page
        .route("/", get(handlers::index))

        // Static frontend assets (CSS/JS) — embedded in the binary
        .route("/static/{file}", get(handlers::serve_static))

        // WebSocket
        .route("/ws", get(ws::ws_handler))

        // Drive the agent from a user prompt (interactive playground)
        .route("/api/run", post(handlers::run_task))

        // Session APIs
        .route("/api/session", get(handlers::list_sessions))
        .route("/api/session/{id}", get(handlers::get_session))
        .route("/api/session/{id}/trace", get(handlers::get_session_trace))
        .route("/api/session/{id}/metrics", get(handlers::get_session_metrics))

        // Graph APIs
        .route("/api/graph", get(handlers::list_graphs))
        .route("/api/graph/{name}", get(handlers::get_graph))

        // Checkpoint APIs
        .route("/api/checkpoint", get(handlers::list_checkpoints))
        .route("/api/checkpoint/{id}", get(handlers::get_checkpoint))
        .route("/api/checkpoint/{id}/restore", post(handlers::restore_checkpoint))

        // Domain Pack APIs
        .route("/api/domain-pack", get(handlers::list_domain_packs))
        .route("/api/domain-pack/{name}", get(handlers::get_domain_pack))

        // Tools API
        .route("/api/tools", get(handlers::list_tools))

        // Shared state
        .with_state(state)
}
