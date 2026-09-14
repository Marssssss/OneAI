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
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::error::OrchestratorError;
use crate::fsm::{SessionSnapshot, SessionState};
use crate::proxy::proxy_pump;
use crate::server::OrchestratorState;
use crate::store::{ClaimOutcome, LeaseGuard};

/// Header carrying the owning replica id on a 409 lease conflict (MVS4-A) —
/// a fronting LB can learn sticky routing from it.
pub const OWNER_REPLICA_HEADER: HeaderName = HeaderName::from_static("x-oneai-owner-replica");

/// Header fallback for the tenant declaration (MVS4-B) — for proxies that
/// can't rewrite request bodies. The body field wins on conflict.
pub const TENANT_HEADER: HeaderName = HeaderName::from_static("x-oneai-tenant");

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
    /// Owning tenant (MVS4-B, `[a-zA-Z0-9_-]*`, ≤64). Canonical source for
    /// the quota bucket; the `X-Oneai-Tenant` header is the fallback when
    /// this is omitted (body wins on conflict). Empty/absent = untagged →
    /// the `"default"` bucket. Trusted-caller declared for now; JWT/OIDC
    /// tenant claims will override this in a later round.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// Extra env vars injected into the container (merged over the
    /// orchestrator's passthrough env).
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// `GET /v1/sessions` query params.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Filter to one tenant bucket (MVS4-B). `default` matches untagged
    /// sessions (they normalize to that bucket).
    #[serde(default)]
    pub tenant: Option<String>,
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
    // MVS4-B quota rejections get a machine-readable 429 body (the `reason`
    // discriminator lets clients tell "delete something" from "slow down").
    if let OrchestratorError::QuotaExceeded {
        ref tenant,
        reason,
        limit,
        current,
        retry_after_secs,
        ref message,
    } = e
    {
        let mut headers = HeaderMap::new();
        if let Some(secs) = retry_after_secs {
            if let Ok(v) = secs.to_string().parse() {
                headers.insert(header::RETRY_AFTER, v);
            }
        }
        return (
            StatusCode::TOO_MANY_REQUESTS,
            headers,
            Json(serde_json::json!({
                "error": "quota_exceeded",
                "reason": reason.as_str(),
                "tenant_id": tenant,
                "limit": limit,
                "current": current,
                "message": message,
            })),
        )
            .into_response();
    }
    let (status, msg) = match &e {
        OrchestratorError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        OrchestratorError::AlreadyExists(_) => (StatusCode::CONFLICT, e.to_string()),
        OrchestratorError::InvalidSessionId(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        OrchestratorError::InvalidTenantId(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        OrchestratorError::Runner(_) => (StatusCode::BAD_GATEWAY, e.to_string()),
        OrchestratorError::NotRunnable { .. } | OrchestratorError::ResumeTimeout(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.to_string())
        }
        OrchestratorError::Config(_) | OrchestratorError::Pg(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
        OrchestratorError::LeaseLost(_) => (StatusCode::CONFLICT, e.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Resolve the tenant declaration: body field wins over the
/// `X-Oneai-Tenant` header; both absent = untagged (`""`).
fn resolve_tenant(req_tenant: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(t) = req_tenant {
        return t.trim().to_string();
    }
    headers
        .get(TENANT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
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
    let tenant = resolve_tenant(req.tenant_id.as_deref(), &headers);
    let mut env: Vec<(String, String)> = req.env.into_iter().collect();
    env.sort();
    let create = st.create_session_for_tenant(req.session_id, &tenant, env);
    let span = tracing::info_span!(
        "orchestrator.create_session",
        tenant.id = %crate::runner::tenant_bucket(&tenant),
    );
    match create.instrument(span).await {
        Ok(session) => {
            let resp = CreateSessionResponse {
                ws_url: ws_url(&session.session_id),
                session,
            };
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => {
            if let OrchestratorError::QuotaExceeded {
                reason,
                ref message,
                ..
            } = e
            {
                tracing::warn!(
                    tenant = %crate::runner::tenant_bucket(&tenant),
                    reason = %reason,
                    "session create rejected by quota: {message}"
                );
            }
            err_response(e)
        }
    }
}

async fn list_sessions(
    State(st): State<Arc<OrchestratorState>>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Some(resp) = st.bearer.guard(&headers) {
        return resp;
    }
    let mut sessions = st.table.list().await;
    // Tenant filter (MVS4-B): `?tenant=default` matches untagged sessions
    // (bucket normalization), anything else matches the declared tenant.
    if let Some(tenant) = q.tenant.filter(|t| !t.trim().is_empty()) {
        let bucket = crate::runner::tenant_bucket(tenant.trim()).to_string();
        sessions.retain(|s| crate::runner::tenant_bucket(&s.tenant_id) == bucket);
    }
    Json(serde_json::json!({ "sessions": sessions })).into_response()
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
    let span = tracing::info_span!("orchestrator.delete_session", session.id = %id);
    match st.destroy_session(&id).instrument(span).await {
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

    // MVS4-A ownership: exactly one replica proxies a session, so activity
    // accounting and the idle sweep stay unambiguous. Claim BEFORE any
    // container operation (liveness CAS / resume) — a replica that can't
    // own the session must not operate its container either. The guard
    // heartbeats (ttl/3) for as long as this handler and then the proxy
    // pump live; early returns drop it (best-effort release). File mode:
    // no leasing — guard stays None, behaviour identical to MVS2.
    let lease_guard = match st.lease_identity() {
        Some(lease) => {
            match st
                .table
                .store()
                .try_claim_lease(&id, &lease.replica_id, lease.ttl)
                .await
            {
                Ok(ClaimOutcome::Owned { .. }) => Some(LeaseGuard::start(
                    st.table.clone(),
                    id.clone(),
                    lease.replica_id.clone(),
                    lease.ttl,
                )),
                Ok(ClaimOutcome::HeldByOther { owner_replica, .. }) => {
                    return (
                        StatusCode::CONFLICT,
                        [
                            (OWNER_REPLICA_HEADER, owner_replica.as_str()),
                            (header::RETRY_AFTER, "1"),
                        ],
                        Json(serde_json::json!({
                            "error": "session is owned by another orchestrator replica",
                            "session_id": id,
                            "owner_replica": owner_replica,
                        })),
                    )
                        .into_response();
                }
                Ok(ClaimOutcome::NotFound) => {
                    return err_response(OrchestratorError::NotFound(id));
                }
                Err(e) => return err_response(e),
            }
        }
        None => None,
    };

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

    let proxy_span = tracing::info_span!(
        "orchestrator.ws_proxy",
        session.id = %id,
        tenant.id = %crate::runner::tenant_bucket(&entry.spec.tenant_id),
    );
    ws.on_upgrade(move |socket| {
        async move {
            // Ownership lives exactly as long as the pump (drop → force-flush
            // activity + release the lease for the next replica).
            let _lease = lease_guard;
            proxy_pump(socket, upstream, entry).await;
        }
        .instrument(proxy_span)
    })
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
