//! Orchestrator core: shared state + session lifecycle operations + the
//! control-plane server entrypoint.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::config::{OrchestratorConfig, ORCHESTRATOR_SECRET_ENV};
use crate::error::{OrchestratorError, Result};
use crate::fsm::{SessionEntry, SessionSnapshot, SessionState};
use crate::idle::spawn_idle_sweep;
use crate::registry::RoutingTable;
use crate::routes::router;
use crate::runner::{
    state_volume_name, workspace_volume_name, ContainerHandle, ContainerRunner, SessionSpec,
};

/// Shared orchestrator state (axum state + lifecycle operations).
pub struct OrchestratorState {
    /// Effective configuration.
    pub config: OrchestratorConfig,
    /// Session routing table (persisted).
    pub table: RoutingTable,
    /// Container backend.
    pub runner: Arc<dyn ContainerRunner>,
    /// Frontend → orchestrator bearer secret (D7).
    pub bearer: oneai_http_auth::BearerSecret,
}

impl OrchestratorState {
    /// Build state from config + runner, loading and reconciling the
    /// persisted routing table (D3: alive containers are re-mounted, dead
    /// ones marked Crashed for lazy resume).
    pub async fn new(
        config: OrchestratorConfig,
        runner: Arc<dyn ContainerRunner>,
    ) -> Result<Arc<Self>> {
        let bearer =
            oneai_http_auth::BearerSecret::from_env(ORCHESTRATOR_SECRET_ENV).ok_or_else(|| {
                OrchestratorError::Config(format!(
                    "{ORCHESTRATOR_SECRET_ENV} must be set to a non-empty value \
                 (frontend→orchestrator bearer auth; refuse to start without it)"
                ))
            })?;
        tokio::fs::create_dir_all(&config.registry_dir).await?;
        let table = RoutingTable::load_and_reconcile(&config.registry_dir, runner.as_ref()).await?;
        Ok(Arc::new(Self {
            config,
            table,
            runner,
            bearer,
        }))
    }

    /// Compose the spawn spec for a session id (image/volumes/ports/env from
    /// config + per-session env).
    pub fn build_spec(&self, session_id: &str, extra_env: Vec<(String, String)>) -> SessionSpec {
        let mut env = self.config.resolved_passthrough_env();
        for (k, v) in extra_env {
            // Per-session env wins over passthrough on key collision.
            if let Some(slot) = env.iter_mut().find(|(ek, _)| ek == &k) {
                slot.1 = v;
            } else {
                env.push((k, v));
            }
        }
        env.sort();
        SessionSpec {
            session_id: session_id.to_string(),
            image: self.config.image.clone(),
            state_volume: state_volume_name(session_id),
            workspace_volume: workspace_volume_name(session_id),
            env,
            bind_host: self.config.container_bind_host.clone(),
            container_port: self.config.container_port,
            provider_config: self.config.provider_config.clone(),
            created_at: chrono::Utc::now(),
        }
    }

    /// Create a session and spawn its container. Blocks until the engine
    /// port is accepting connections (or the spawn fails → Failed state).
    pub async fn create_session(
        self: &Arc<Self>,
        session_id: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<SessionSnapshot> {
        let id = match session_id {
            Some(id) => {
                SessionSpec::validate_session_id(&id)?;
                id
            }
            None => uuid::Uuid::new_v4().simple().to_string(),
        };
        let spec = self.build_spec(&id, env);
        let entry = self
            .table
            .insert_new(SessionEntry::new_creating(spec.clone()))
            .await?;
        // Persist the Creating entry so a mid-spawn orchestrator crash
        // reconciles it away on restart.
        self.table.persist().await?;

        match self.spawn_and_mark(entry.clone()).await {
            Ok(arc) => Ok(arc.snapshot()),
            Err(e) => Err(e),
        }
    }

    /// Runner spawn + health wait + CAS Creating→Running / →Failed.
    async fn spawn_and_mark(
        self: &Arc<Self>,
        entry: Arc<SessionEntry>,
    ) -> Result<Arc<SessionEntry>> {
        let id = entry.spec.session_id.clone();
        match self.runner.spawn(&entry.spec).await {
            Ok(handle) => match self.wait_healthy(&handle).await {
                Ok(()) => {
                    let arc = self
                        .table
                        .cas_transition(
                            &id,
                            SessionState::Creating,
                            SessionState::Running,
                            Some(handle),
                            None,
                        )
                        .await?
                        .ok_or_else(|| OrchestratorError::CasMiss {
                            id: id.clone(),
                            expected: SessionState::Creating,
                            found: SessionState::Destroyed,
                        })?;
                    Ok(arc)
                }
                Err(health_err) => {
                    let _ = self.runner.destroy(&handle, false).await;
                    let _ = self
                        .table
                        .cas_transition(
                            &id,
                            SessionState::Creating,
                            SessionState::Failed,
                            None,
                            Some(format!("engine not healthy after spawn: {health_err}")),
                        )
                        .await;
                    Err(OrchestratorError::Runner(format!(
                        "engine not healthy after spawn: {health_err}"
                    )))
                }
            },
            Err(e) => {
                let _ = self
                    .table
                    .cas_transition(
                        &id,
                        SessionState::Creating,
                        SessionState::Failed,
                        None,
                        Some(e.to_string()),
                    )
                    .await;
                Err(e)
            }
        }
    }

    /// Poll `runner.health` until true or `resume_timeout_secs` elapses.
    async fn wait_healthy(&self, handle: &ContainerHandle) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(self.config.resume_timeout_secs);
        loop {
            if self.runner.health(handle).await.unwrap_or(false) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(OrchestratorError::Runner(format!(
                    "container {} never became healthy",
                    handle.container_name
                )));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    /// Resume a Hibernating/Crashed session (D6: request-triggered).
    /// CAS guarantees at-most-one concurrent resume actually operates the
    /// container; losers return Ok immediately (the caller waits via
    /// `wait_until_running`).
    pub async fn resume_session(self: &Arc<Self>, id: &str) -> Result<()> {
        let Some(entry) = self.table.get(id).await else {
            return Err(OrchestratorError::NotFound(id.to_string()));
        };
        let from = entry.state;
        if from == SessionState::Running || from.is_pending() {
            return Ok(()); // nothing to do / already in flight
        }
        if !from.is_resumable() {
            return Err(OrchestratorError::NotRunnable {
                id: id.to_string(),
                state: from,
                reason: entry.last_error.clone().unwrap_or_default(),
            });
        }
        // CAS → Resuming; a miss means another request won the race.
        let Some(_) = self
            .table
            .cas_transition(id, from, SessionState::Resuming, None, None)
            .await?
        else {
            return Ok(());
        };

        let result = match from {
            // Hibernate path: same container, docker start, port re-inspect.
            SessionState::Hibernating => match &entry.handle {
                Some(h) => self.runner.start(h).await,
                None => self.runner.spawn(&entry.spec).await,
            },
            // Crash path: brand-new container on the SAME volumes (G3).
            // DockerRunner::spawn tolerates and replaces the dead remnant.
            _ => self.runner.spawn(&entry.spec).await,
        };

        match result {
            Ok(handle) => match self.wait_healthy(&handle).await {
                Ok(()) => {
                    let _ = self
                        .table
                        .cas_transition(
                            id,
                            SessionState::Resuming,
                            SessionState::Running,
                            Some(handle),
                            None,
                        )
                        .await;
                    tracing::info!(session = %id, from = ?from, "session resumed");
                    Ok(())
                }
                Err(e) => {
                    let _ = self
                        .table
                        .cas_transition(
                            id,
                            SessionState::Resuming,
                            SessionState::Crashed,
                            None,
                            Some(format!("resume unhealthy: {e}")),
                        )
                        .await;
                    Err(e)
                }
            },
            Err(e) => {
                let _ = self
                    .table
                    .cas_transition(
                        id,
                        SessionState::Resuming,
                        SessionState::Crashed,
                        None,
                        Some(format!("resume failed: {e}")),
                    )
                    .await;
                Err(e)
            }
        }
    }

    /// Wait until a session reaches Running (WS-upgrade / RPC gating).
    /// Polls the entry with a `ready_notify` fast-path; returns Err on
    /// timeout or when the session settles into a non-runnable state.
    pub async fn wait_until_running(
        &self,
        id: &str,
        timeout: Duration,
    ) -> Result<Arc<SessionEntry>> {
        let deadline = Instant::now() + timeout;
        loop {
            let Some(entry) = self.table.get(id).await else {
                return Err(OrchestratorError::NotFound(id.to_string()));
            };
            match entry.state {
                SessionState::Running => return Ok(entry),
                SessionState::Failed | SessionState::Crashed => {
                    return Err(OrchestratorError::NotRunnable {
                        id: id.to_string(),
                        state: entry.state,
                        reason: entry.last_error.clone().unwrap_or_default(),
                    })
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(OrchestratorError::ResumeTimeout(id.to_string()));
            }
            // Wait for the Running notification (or re-check every 500ms —
            // race-proof even if a notify is missed between get() and
            // notified()).
            let _ = tokio::time::timeout(Duration::from_millis(500), entry.ready_notify.notified())
                .await;
        }
    }

    /// Destroy a session: stop/remove container + volumes, tombstone the
    /// routing entry.
    pub async fn destroy_session(&self, id: &str) -> Result<()> {
        let Some(entry) = self.table.get(id).await else {
            return Err(OrchestratorError::NotFound(id.to_string()));
        };
        // CAS current → Destroyed (administrative; legal from every live
        // state). A miss means a concurrent transition — re-read and retry
        // once.
        let from = entry.state;
        if self
            .table
            .cas_transition(id, from, SessionState::Destroyed, None, None)
            .await?
            .is_none()
        {
            let Some(entry) = self.table.get(id).await else {
                return Ok(()); // gone already
            };
            self.table
                .cas_transition(id, entry.state, SessionState::Destroyed, None, None)
                .await?;
        }
        if let Some(handle) = &entry.handle {
            // Tolerate a dead/absent container.
            let _ = self.runner.destroy(handle, true).await;
        }
        self.table.remove(id).await?;
        tracing::info!(session = %id, "session destroyed (container + volumes removed)");
        Ok(())
    }

    /// Fast TCP liveness probe of a session's published engine port (no
    /// docker CLI involved — the orchestrator reaches the port the same way
    /// the WS proxy does).
    pub async fn probe_upstream(entry: &SessionEntry) -> bool {
        let Some(port) = entry.handle.as_ref().map(|h| h.host_port) else {
            return false;
        };
        let host = if entry.spec.bind_host == "0.0.0.0" {
            "127.0.0.1"
        } else {
            entry.spec.bind_host.as_str()
        };
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect((host, port)),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
    }

    /// Request-time crash detection (D6 "container died, orchestrator marks
    /// Crashed"): if the entry says Running but the upstream port refuses
    /// connections (e.g. `docker kill` — MVS2 has no background health
    /// poller), CAS Running→Crashed so the caller can trigger a resume.
    /// Returns the freshest entry either way.
    pub async fn check_upstream_liveness(&self, id: &str) -> Option<Arc<SessionEntry>> {
        let entry = self.table.get(id).await?;
        if entry.state == SessionState::Running && !Self::probe_upstream(&entry).await {
            tracing::warn!(session = %id, "Running session upstream unreachable — marking Crashed");
            let _ = self
                .table
                .cas_transition(
                    id,
                    SessionState::Running,
                    SessionState::Crashed,
                    None,
                    Some("upstream unreachable on connect".into()),
                )
                .await;
            return self.table.get(id).await;
        }
        Some(entry)
    }

    /// Upstream ws URL for a running session entry.
    pub fn upstream_ws_url(entry: &SessionEntry) -> Option<String> {
        entry.handle.as_ref().map(|h| {
            format!(
                "ws://{}:{}/ws",
                // The orchestrator reaches the published port on the same
                // host it bound it to; bind_host 0.0.0.0 probes via
                // 127.0.0.1.
                if entry.spec.bind_host == "0.0.0.0" {
                    "127.0.0.1"
                } else {
                    &entry.spec.bind_host
                },
                h.host_port
            )
        })
    }
}

/// Run the orchestrator control plane until Ctrl-C (or `cancel`).
pub async fn run(
    config: OrchestratorConfig,
    runner: Arc<dyn ContainerRunner>,
    cancel: CancellationToken,
) -> Result<()> {
    let state = OrchestratorState::new(config.clone(), runner).await?;
    let listen: std::net::SocketAddr =
        state.config.listen.parse().map_err(|e| {
            OrchestratorError::Config(format!("listen {}: {e}", state.config.listen))
        })?;

    // Idle-hibernation sweep (D6). `idle_timeout_secs == 0` disables it
    // (the CLI advertises "disabled" for 0 — honor that).
    let _sweep = (state.config.idle_timeout_secs > 0).then(|| {
        spawn_idle_sweep(
            state.table.clone(),
            state.runner.clone(),
            Duration::from_secs(state.config.idle_timeout_secs),
            crate::idle::DEFAULT_SWEEP_TICK,
            cancel.clone(),
        )
    });

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, sessions = state.table.len().await, "orchestrator control plane listening");
    eprintln!(
        "orchestrator listening on http://{listen} ({} sessions)",
        state.table.len().await
    );

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = cancel.cancelled() => {},
            }
        })
        .await
        .map_err(|e| OrchestratorError::Runner(e.to_string()))?;
    Ok(())
}
