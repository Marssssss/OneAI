//! axum control plane: session CRUD + WS reverse proxy + healthz.
//!
//! All `/v1/*` endpoints require `Authorization: Bearer <secret>` (D7).
//! The WS endpoint additionally accepts `?token=<secret>` because browsers
//! cannot set headers on a WebSocket handshake. JSON-RPC payloads on the WS
//! proxy are passed through untouched (D3 — zero protocol translation).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::fsm::{SessionSnapshot, SessionState};
use crate::proxy::proxy_pump;
use crate::server::OrchestratorState;

/// Assemble the control-plane router.
pub fn router(state: Arc<OrchestratorState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/sessions", post(create_session).get(list_sessions))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/sessions/{id}/ws", get(ws_session))
        .with_state(state)
}

// ─── Request/response shapes ──────────────────────────────────────────────────

/// `POST /v1/sessions` body (all fields optional).
#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    /// Client-chosen session id (`[a-zA-Z0-9_-]+`, ≤64). Random uuid when
    /// omitted.
    pub session_id: Option<String>,
    /// Extra env vars injected into the container (merged over the
    /// orchestrator's passthrough env).
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// `POST /v1/sessions` response.
#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub session: SessionSnapshot,
    /// WS reverse-proxy URL for this session (relative to the control plane).
    pub ws_url: String,
}

fn ws_url(id: &str) -> String {
    format!("/v1/sessions/{id}/ws")
}

fn err_response(e: OrchestratorError) -> Response {
    let (status, msg) = match &e {
        OrchestratorError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        OrchestratorError::AlreadyExists(_) => (StatusCode::CONFLICT, e.to_string()),
        OrchestratorError::InvalidSessionId(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        OrchestratorError::Runner(_) => (StatusCode::BAD_GATEWAY, e.to_string()),
        OrchestratorError::NotRunnable { .. } | OrchestratorError::ResumeTimeout(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.to_string())
        }
        OrchestratorError::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Query params for the WS endpoint (browser-compatible token auth).
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    /// Bearer token (alternative to the Authorization header).
    pub token: Option<String>,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn create_session(
    State(st): State<Arc<OrchestratorState>>,
    headers: HeaderMap,
    Json(req): Json<CreateSessionRequest>,
) -> Response {
    if let Some(resp) = st.bearer.guard(&headers) {
        return resp;
    }
    let mut env: Vec<(String, String)> = req.env.into_iter().collect();
    env.sort();
    match st.create_session(req.session_id, env).await {
        Ok(session) => {
            let resp = CreateSessionResponse {
                ws_url: ws_url(&session.session_id),
                session,
            };
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => err_response(e),
    }
}

async fn list_sessions(State(st): State<Arc<OrchestratorState>>, headers: HeaderMap) -> Response {
    if let Some(resp) = st.bearer.guard(&headers) {
        return resp;
    }
    Json(serde_json::json!({ "sessions": st.table.list().await })).into_response()
}

async fn get_session(
    State(st): State<Arc<OrchestratorState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = st.bearer.guard(&headers) {
        return resp;
    }
    match st.table.get(&id).await {
        Some(entry) => Json(entry.snapshot()).into_response(),
        None => err_response(OrchestratorError::NotFound(id)),
    }
}

async fn delete_session(
    State(st): State<Arc<OrchestratorState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = st.bearer.guard(&headers) {
        return resp;
    }
    match st.destroy_session(&id).await {
        Ok(()) => Json(serde_json::json!({ "deleted": id })).into_response(),
        Err(e) => err_response(e),
    }
}

/// WS upgrade → reverse proxy (behaviour matrix per D6):
///
/// | state on upgrade | action |
/// |---|---|
/// | Running | proxy immediately |
/// | Creating / Resuming | wait for readiness (timeout → 503 + Retry-After) |
/// | Hibernating / Crashed | trigger resume, then wait |
/// | Failed | 503 with last_error |
async fn ws_session(
    State(st): State<Arc<OrchestratorState>>,
    headers: HeaderMap,
    Query(q): Query<WsQuery>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    // Auth: header bearer OR ?token= (browsers can't set ws headers).
    let authed =
        st.bearer.verify(&headers) || q.token.as_deref().is_some_and(|t| st.bearer.verify_str(t));
    if !authed {
        return st
            .bearer
            .guard(&headers)
            .unwrap_or_else(|| (StatusCode::UNAUTHORIZED, "unauthorized").into_response());
    }

    // Liveness check doubles as request-time crash detection: a Running
    // entry whose upstream port refuses connections (docker kill — MVS2 has
    // no background poller) is CAS'd to Crashed here, so the resume path
    // below kicks in on THIS reconnect (acceptance: "杀容器后前端重连自动
    // Resuming").
    let Some(entry) = st.check_upstream_liveness(&id).await else {
        return err_response(OrchestratorError::NotFound(id));
    };

    // Trigger resume for dormant/crashed sessions (fire-and-forget; the CAS
    // inside resume_session guarantees at-most-one container operation).
    if entry.state.is_resumable() {
        let st2 = st.clone();
        let id2 = id.clone();
        tokio::spawn(async move {
            if let Err(e) = st2.resume_session(&id2).await {
                tracing::warn!(session = %id2, error = %e, "ws-triggered resume failed");
            }
        });
    }

    if !entry.state.is_runnable() {
        // Wait for readiness (Creating/Resuming/just-triggered resume).
        let timeout = Duration::from_secs(st.config.resume_timeout_secs);
        match st.wait_until_running(&id, timeout).await {
            Ok(_) => {}
            Err(OrchestratorError::ResumeTimeout(_)) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::RETRY_AFTER, "5")],
                    Json(serde_json::json!({
                        "error": "session not ready",
                        "session_id": id,
                        "retry_after": 5,
                    })),
                )
                    .into_response();
            }
            Err(e) => return err_response(e),
        }
    }

    // Re-read: state is Running now (wait_until_running guarantees, but the
    // handle may have been refreshed by the resume).
    let Some(entry) = st.table.get(&id).await else {
        return err_response(OrchestratorError::NotFound(id));
    };
    if entry.state != SessionState::Running {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            Json(serde_json::json!({ "error": "session not ready", "retry_after": 5 })),
        )
            .into_response();
    }
    let Some(upstream) = OrchestratorState::upstream_ws_url(&entry) else {
        return err_response(OrchestratorError::Runner(format!(
            "session {id} Running without a container handle"
        )));
    };

    ws.on_upgrade(move |socket| proxy_pump(socket, upstream, entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_shape() {
        assert_eq!(ws_url("abc"), "/v1/sessions/abc/ws");
    }

    #[test]
    fn err_status_mapping() {
        let cases = [
            (
                OrchestratorError::NotFound("x".into()),
                StatusCode::NOT_FOUND,
            ),
            (
                OrchestratorError::AlreadyExists("x".into()),
                StatusCode::CONFLICT,
            ),
            (
                OrchestratorError::InvalidSessionId("x".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                OrchestratorError::Runner("x".into()),
                StatusCode::BAD_GATEWAY,
            ),
            (
                OrchestratorError::ResumeTimeout("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ];
        for (e, want) in cases {
            assert_eq!(err_response(e).status(), want);
        }
    }
}
