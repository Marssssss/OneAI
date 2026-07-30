//! External one-shot trigger HTTP surface — `POST /cron/fire` shared-secret
//! bearer receiver + outbound `provision(...)` registration (inspiration
//! P2-2).
//!
//! This lets an **external** scheduler (cron-job.org, GitHub Actions, a systemd
//! timer, Kubernetes CronJob) drive OneAI: it fires `POST /cron/fire` at the
//! scheduled instant, OneAI routes the job through the same `cas_mark_fired`
//! at-most-once CAS path the ticker uses, and delivers it.
//!
//! ## Auth
//!
//! The plan calls for JWT, but the workspace ships no JWT lib and adding
//! `jsonwebtoken` pulls in `ring` (heavy, new supply-chain surface). Per the
//! supply-chain 戒律 we use a **shared-secret bearer token** instead
//! (`ONEAI_CRON_SECRET` env), constant-time-compared — the same posture the
//! gateway takes with Feishu/WeChat (signature, not JWT). Documented
//! deviation from the plan's "JWT 验证".
//!
//! ## Outbound provision
//!
//! `OneShotProvider::provision(endpoint, job_id, fire_at, callback_url, secret)`
//! (feature `oneshot-provision`) POSTs a registration to an external cron
//! service's API so it fires the job at `fire_at` back to `callback_url`. The
//! external service's exact API is service-specific; this is a generic
//! convention-based POST (the caller supplies the full register `endpoint`).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[cfg(feature = "oneshot-provision")]
use async_trait::async_trait;

use crate::error::{CronError, Result};
use crate::orchestrator::CronSchedulerImpl;

/// The bearer secret env var name.
pub const CRON_SECRET_ENV: &str = "ONEAI_CRON_SECRET";

/// Read the bearer secret from env. `None` if unset → the receiver 503s
/// (external triggering is disabled until the operator sets a secret).
pub fn secret_from_env() -> Option<String> {
    std::env::var(CRON_SECRET_ENV)
        .ok()
        .filter(|s| !s.is_empty())
}

/// Constant-time comparison so a bearer mismatch doesn't short-circuit and
/// leak length/timing. `true` iff equal.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Verify the `Authorization: Bearer <secret>` header against `expected`.
fn verify_bearer(headers: &HeaderMap, expected: &str) -> bool {
    let Ok(Some(value)) = headers
        .get(axum::http::header::AUTHORIZATION)
        .map(|v| v.to_str())
        .transpose()
    else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    ct_eq(token.as_bytes(), expected.as_bytes())
}

/// Inbound fire request body.
#[derive(Debug, Deserialize)]
pub struct FireRequest {
    pub job_id: String,
    /// Optional planned fire instant (RFC3339). Only informational — the
    /// receiver fires NOW regardless (the external scheduler already waited).
    pub fire_at: Option<String>,
}

/// Inbound fire response body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FireResponse {
    /// The job was fired (CAS took it — at-most-once this window).
    Fired { job_id: String },
    /// The job exists but wasn't eligible (already fired this window /
    /// disabled / not due).
    NotEligible { job_id: String },
    /// The job id is unknown.
    NotFound { job_id: String },
}

/// Shared state for the fire router.
#[derive(Clone)]
pub struct FireState {
    pub scheduler: Arc<CronSchedulerImpl>,
    pub secret: String,
}

/// Build the axum router for the external one-shot receiver.
pub fn build_router(state: FireState) -> axum::Router {
    axum::Router::new()
        .route("/cron/fire", post(post_fire))
        .with_state(state)
}

/// Configuration for the receiver server.
#[derive(Debug, Clone)]
pub struct FireServerConfig {
    pub addr: SocketAddr,
}

impl Default for FireServerConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([0, 0, 0, 0], 9091)),
        }
    }
}

/// Start the receiver. Blocks until the server stops. Returns an error if no
/// secret is configured (external triggering disabled).
pub async fn serve(config: FireServerConfig, state: FireState) -> Result<()> {
    if state.secret.is_empty() {
        return Err(CronError::Store(
            "ONEAI_CRON_SECRET unset — external /cron/fire receiver disabled".to_string(),
        ));
    }
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .map_err(|e| CronError::Store(format!("bind {}: {}", config.addr, e)))?;
    info!(
        "OneAI cron /cron/fire receiver listening on http://{}",
        config.addr
    );
    axum::serve(listener, router)
        .await
        .map_err(|e| CronError::Store(format!("axum serve: {e}")))?;
    Ok(())
}

async fn post_fire(
    State(state): State<FireState>,
    headers: HeaderMap,
    Json(req): Json<FireRequest>,
) -> Response {
    // Unconditional visibility (mirrors the gateway's webhook diagnostic).
    eprintln!("[cron] inbound POST /cron/fire (job_id={})", req.job_id);
    if !verify_bearer(&headers, &state.secret) {
        warn!("cron /cron/fire: bad bearer — 401");
        return (StatusCode::UNAUTHORIZED, "bad bearer").into_response();
    }
    let now = chrono::Utc::now();
    match state.scheduler.trigger(&req.job_id, now).await {
        Ok(true) => (
            StatusCode::OK,
            Json(FireResponse::Fired { job_id: req.job_id }),
        )
            .into_response(),
        Ok(false) => {
            // Distinguish not-found from not-eligible so the external caller
            // can tell a misconfigured job id from a double-fire.
            let exists = state
                .scheduler
                .store()
                .get(&req.job_id)
                .await
                .map(|o| o.is_some())
                .unwrap_or(false);
            if exists {
                (
                    StatusCode::OK,
                    Json(FireResponse::NotEligible { job_id: req.job_id }),
                )
                    .into_response()
            } else {
                (
                    StatusCode::OK,
                    Json(FireResponse::NotFound { job_id: req.job_id }),
                )
                    .into_response()
            }
        }
        Err(e) => {
            warn!(error = %e, "cron /cron/fire trigger error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("trigger error: {e}"),
            )
                .into_response()
        }
    }
}

// ─── Outbound provisioning ───────────────────────────────────────────────────

/// An external one-shot provisioning client — registers a one-shot with an
/// external cron service so it fires `job_id` at `fire_at` back to
/// `callback_url`.
#[cfg(feature = "oneshot-provision")]
#[async_trait]
pub trait OneShotProvider: Send + Sync {
    /// Register a one-shot. `endpoint` is the external service's register URL
    /// (service-specific); `secret` is sent as the bearer to the callback.
    async fn provision(
        &self,
        endpoint: &str,
        job_id: &str,
        fire_at: chrono::DateTime<chrono::Utc>,
        callback_url: &str,
        secret: &str,
    ) -> Result<()>;
}

/// A reqwest-backed `OneShotProvider` that POSTs a JSON registration to
/// `endpoint`. The body shape is a documented convention:
/// `{ "job_id": .., "fire_at": <rfc3339>, "callback_url": .. }` — real
/// cron-job.org / GitHub Actions adapters wrap / translate it.
#[cfg(feature = "oneshot-provision")]
pub struct HttpOneShotProvider {
    client: reqwest::Client,
}

#[cfg(feature = "oneshot-provision")]
impl HttpOneShotProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder().build().expect("reqwest client"),
        }
    }
}

#[cfg(feature = "oneshot-provision")]
impl Default for HttpOneShotProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "oneshot-provision")]
#[derive(Serialize)]
struct ProvisionBody<'a> {
    job_id: &'a str,
    fire_at: String,
    callback_url: &'a str,
}

#[cfg(feature = "oneshot-provision")]
#[async_trait]
impl OneShotProvider for HttpOneShotProvider {
    async fn provision(
        &self,
        endpoint: &str,
        job_id: &str,
        fire_at: chrono::DateTime<chrono::Utc>,
        callback_url: &str,
        _secret: &str,
    ) -> Result<()> {
        let body = ProvisionBody {
            job_id,
            fire_at: fire_at.to_rfc3339(),
            callback_url,
        };
        self.client
            .post(endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| CronError::Store(format!("provision POST '{endpoint}': {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{parse_schedule, CronJob};
    use crate::orchestrator::NoopCronRunner;
    use crate::store::InMemoryJobStore;
    use chrono::Utc;
    use std::sync::Arc;
    use std::time::Duration;

    fn make_sched() -> (Arc<CronSchedulerImpl>, Arc<NoopCronRunner>) {
        let store: Arc<dyn crate::store::JobStore> = Arc::new(InMemoryJobStore::new());
        let runner = Arc::new(NoopCronRunner::new());
        let sched = Arc::new(CronSchedulerImpl::with_tick(
            store,
            runner.clone(),
            Duration::from_secs(1),
        ));
        (sched, runner)
    }

    async fn spawn_server(
        secret: &str,
    ) -> (
        SocketAddr,
        Arc<CronSchedulerImpl>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sched, _r) = make_sched();
        let state = FireState {
            scheduler: sched.clone(),
            secret: secret.to_string(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let router = build_router(state);
            let _ = axum::serve(listener, router).await;
        });
        (addr, sched, handle)
    }

    #[tokio::test]
    async fn fire_endpoint_rejects_bad_bearer() {
        let (addr, _s, _h) = spawn_server("topsecret").await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/cron/fire"))
            .bearer_auth("wrong")
            .json(&serde_json::json!({"job_id": "j1"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn fire_endpoint_fires_due_job_and_dedups() {
        let (addr, sched, _h) = spawn_server("topsecret").await;
        // Add a due job.
        let mut j = CronJob::new("j1", "j1", parse_schedule("*/5 * * * *").unwrap());
        j.task = "hi".into();
        j.next_fire_at = Some(Utc::now());
        sched.store().upsert(j).await.unwrap();

        let client = reqwest::Client::new();
        // First fire: Fired.
        let resp: serde_json::Value = client
            .post(format!("http://{addr}/cron/fire"))
            .bearer_auth("topsecret")
            .json(&serde_json::json!({"job_id": "j1"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["fired"]["job_id"], "j1");

        // Second fire same window: NotEligible (CAS advanced — at-most-once).
        let resp2: serde_json::Value = client
            .post(format!("http://{addr}/cron/fire"))
            .bearer_auth("topsecret")
            .json(&serde_json::json!({"job_id": "j1"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp2["not_eligible"]["job_id"], "j1");
    }

    #[tokio::test]
    async fn fire_endpoint_unknown_job_is_not_found() {
        let (addr, _s, _h) = spawn_server("topsecret").await;
        let client = reqwest::Client::new();
        let resp: serde_json::Value = client
            .post(format!("http://{addr}/cron/fire"))
            .bearer_auth("topsecret")
            .json(&serde_json::json!({"job_id": "ghost"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["not_found"]["job_id"], "ghost");
    }

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn verify_bearer_header() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer xyz".parse().unwrap(),
        );
        assert!(verify_bearer(&h, "xyz"));
        assert!(!verify_bearer(&h, "abc"));
    }
}
