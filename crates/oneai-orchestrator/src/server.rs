//! Orchestrator core: shared state + session lifecycle operations + the
//! control-plane server entrypoint.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::archive::{DeepArchive, LocalDirArchiveStore, VolumeArchiveStore};
use crate::config::{OrchestratorConfig, ORCHESTRATOR_SECRET_ENV};
use crate::error::{OrchestratorError, Result};
use crate::fsm::{SessionEntry, SessionSnapshot, SessionState};
use crate::idle::spawn_idle_sweep;
use crate::registry::RoutingTable;
use crate::routes::router;
use crate::runner::{
    state_volume_name, workspace_volume_name, ContainerHandle, ContainerRunner, SessionSpec,
};
use crate::store::{FileSessionStore, LeaseIdentity, SessionStore};

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
    /// Deep-archive volume store (MVS3-C); `None` when deep hibernation is
    /// disabled (`deep_archive_timeout_secs == 0`).
    pub archive_store: Option<Arc<dyn VolumeArchiveStore>>,
    /// This replica's lease identity (MVS4-A): uuid unless pinned via
    /// config. Only meaningful when the store `supports_leasing()`.
    pub replica_id: String,
}

impl OrchestratorState {
    /// Build state from config + runner, loading and reconciling the
    /// persisted routing table (D3: alive containers are re-mounted, dead
    /// ones marked Crashed for lazy resume). The archive store follows the
    /// config (`archive_dir` → `LocalDirArchiveStore`).
    pub async fn new(
        config: OrchestratorConfig,
        runner: Arc<dyn ContainerRunner>,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        let archive_store = archive_store_from_config(&config);
        Self::with_archive_store(config, runner, archive_store).await
    }

    /// Like [`new`](Self::new) with an explicitly provided archive store —
    /// the injection point for custom `VolumeArchiveStore` backends (MVS4
    /// S3/GCS) and for tests. `config.archive_dir` is only used by `new`.
    pub async fn with_archive_store(
        config: OrchestratorConfig,
        runner: Arc<dyn ContainerRunner>,
        archive_store: Option<Arc<dyn VolumeArchiveStore>>,
    ) -> Result<Arc<Self>> {
        tokio::fs::create_dir_all(&config.registry_dir).await?;
        let store = Arc::new(FileSessionStore::open(&config.registry_dir).await?);
        Self::with_session_store(config, runner, archive_store, store).await
    }

    /// Like [`with_archive_store`](Self::with_archive_store) with an
    /// explicitly provided session store — the MVS4-A injection point for
    /// the shared Postgres backend (multi-replica) and for tests running
    /// several states against one store.
    pub async fn with_session_store(
        config: OrchestratorConfig,
        runner: Arc<dyn ContainerRunner>,
        archive_store: Option<Arc<dyn VolumeArchiveStore>>,
        store: Arc<dyn SessionStore>,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        let bearer =
            oneai_http_auth::BearerSecret::from_env(ORCHESTRATOR_SECRET_ENV).ok_or_else(|| {
                OrchestratorError::Config(format!(
                    "{ORCHESTRATOR_SECRET_ENV} must be set to a non-empty value \
                 (frontend→orchestrator bearer auth; refuse to start without it)"
                ))
            })?;
        let replica_id = if config.replica_id.trim().is_empty() {
            uuid::Uuid::new_v4().simple().to_string()
        } else {
            config.replica_id.trim().to_string()
        };
        // Lease identity is needed BEFORE reconcile (startup reconcile is
        // lease-gated in multi-replica mode).
        let lease = store.supports_leasing().then(|| LeaseIdentity {
            replica_id: replica_id.clone(),
            ttl: Duration::from_secs(config.lease_ttl_secs.max(1)),
        });
        let table =
            RoutingTable::reconcile_with_store(store, runner.as_ref(), lease.as_ref()).await?;
        tracing::info!(
            %replica_id,
            leasing = lease.is_some(),
            lease_ttl_secs = config.lease_ttl_secs,
            "orchestrator replica identity"
        );
        Ok(Arc::new(Self {
            config,
            table,
            runner,
            bearer,
            archive_store,
            replica_id,
        }))
    }

    /// This replica's lease identity, or `None` when the backing store has
    /// no leasing (file mode — the single replica owns everything).
    pub fn lease_identity(&self) -> Option<LeaseIdentity> {
        self.table
            .store()
            .supports_leasing()
            .then(|| LeaseIdentity {
                replica_id: self.replica_id.clone(),
                ttl: Duration::from_secs(self.config.lease_ttl_secs.max(1)),
            })
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
        // Durability barrier (file backend: whole-file write; shared stores:
        // the insert itself is already durable).
        self.table.persist().await?;
        // Multi-replica: the creator is the initial owner (no heartbeat —
        // the lease simply lapses once idle, making the session claimable
        // by whichever replica serves it next; sweeps re-claim on demand).
        if let Some(lease) = self.lease_identity() {
            if let Err(e) = self
                .table
                .store()
                .try_claim_lease(&id, &lease.replica_id, lease.ttl)
                .await
            {
                tracing::warn!(session = %id, error = %e, "initial lease claim failed (CAS still arbitrates sweeps)");
            }
        }

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

        // Deep-archived session (MVS3-C): its volumes live ONLY in the
        // archive store — restore them before any container op. The marker
        // is cleared only after the session is fully back to Running, so a
        // mid-way failure retries the restore on the next request (the
        // archive is the source of truth until then). Restore is keyed on
        // the marker, not on `from` — a session that CRASHED right after a
        // restore-spawn keeps its marker until its first successful resume.
        if let Some(manifest) = &entry.archived {
            let Some(store) = &self.archive_store else {
                let msg = "session is deep-archived but no archive store is configured \
                           (archive_dir/deep_archive_timeout_secs changed since archival?)"
                    .to_string();
                let _ = self
                    .table
                    .cas_transition(
                        id,
                        SessionState::Resuming,
                        SessionState::Crashed,
                        None,
                        Some(msg.clone()),
                    )
                    .await;
                return Err(OrchestratorError::Runner(msg));
            };
            if let Err(e) = store.restore(manifest, &self.config.docker_bin).await {
                let msg = format!("archive restore failed: {e}");
                tracing::error!(session = %id, error = %e, "deep-archive restore failed");
                let _ = self
                    .table
                    .cas_transition(
                        id,
                        SessionState::Resuming,
                        SessionState::Crashed,
                        None,
                        Some(msg.clone()),
                    )
                    .await;
                return Err(OrchestratorError::Runner(msg));
            }
            tracing::info!(
                session = %id,
                volumes = manifest.volumes.len(),
                "volumes restored from deep archive"
            );
        }

        let result = match from {
            // Hibernate path: same container, docker start, port re-inspect.
            // A deep-archived entry has no handle (container destroyed) →
            // spawn fresh on the just-restored volumes.
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
                    if entry.archived.is_some() {
                        // Volumes are live locally again — release the
                        // marker, then drop the archive files (a failed
                        // cleanup is harmless: the next deep-archive pass
                        // overwrites the same deterministic layout).
                        let _ = self.table.cas_set_archived(id, None).await;
                        if let Some(store) = &self.archive_store {
                            if let Err(e) = store.remove(id).await {
                                tracing::warn!(
                                    session = %id, error = %e,
                                    "archive cleanup after resume failed (harmless)"
                                );
                            }
                        }
                    }
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
        } else {
            // No live handle (deep-archived, or crashed mid-spawn): still
            // sweep any canonically-named leftovers — `destroy` derives the
            // volume names from the container name and tolerates absence.
            let synthetic = ContainerHandle {
                container_id: String::new(),
                container_name: entry.spec.container_name(),
                host_port: 0,
            };
            let _ = self.runner.destroy(&synthetic, true).await;
        }
        // A deep-archived session's archive files die with the session.
        if let Some(store) = &self.archive_store {
            if let Err(e) = store.remove(id).await {
                tracing::warn!(session = %id, error = %e, "archive removal on destroy failed");
            }
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

/// Deep-archive store (MVS3-C): opt-in via config; the marker on persisted
/// entries decides whether a resume needs a restore.
fn archive_store_from_config(config: &OrchestratorConfig) -> Option<Arc<dyn VolumeArchiveStore>> {
    match (config.deep_archive_timeout_secs, &config.archive_dir) {
        (timeout, Some(dir)) if timeout > 0 => {
            tracing::info!(
                dir = %dir.display(),
                timeout_secs = timeout,
                "deep volume archive enabled (LocalDirArchiveStore)"
            );
            Some(Arc::new(LocalDirArchiveStore::new(dir.clone())))
        }
        _ => None,
    }
}

/// Run the orchestrator control plane until Ctrl-C (or `cancel`), on the
/// file-backed routing table (single replica — the MVS2 default).
pub async fn run(
    config: OrchestratorConfig,
    runner: Arc<dyn ContainerRunner>,
    cancel: CancellationToken,
) -> Result<()> {
    run_with_store(config, runner, None, cancel).await
}

/// Run the control plane on an explicit session store (MVS4-A): pass a
/// `PgSessionStore` for multi-replica mode (shared routing table + per-
/// session leases); `None` keeps the file backend.
pub async fn run_with_store(
    config: OrchestratorConfig,
    runner: Arc<dyn ContainerRunner>,
    store: Option<Arc<dyn SessionStore>>,
    cancel: CancellationToken,
) -> Result<()> {
    let archive_store = archive_store_from_config(&config);
    let state = match store {
        Some(store) => {
            OrchestratorState::with_session_store(config.clone(), runner, archive_store, store)
                .await?
        }
        None => {
            OrchestratorState::with_archive_store(config.clone(), runner, archive_store).await?
        }
    };
    let listen: std::net::SocketAddr =
        state.config.listen.parse().map_err(|e| {
            OrchestratorError::Config(format!("listen {}: {e}", state.config.listen))
        })?;

    // Idle-hibernation sweep (D6) + deep-archive second tier (MVS3-C).
    // `idle_timeout_secs == 0` disables the whole sweep (the CLI advertises
    // "disabled" for 0 — honor that; deep archive is unreachable without
    // hibernation anyway — config.validate() rejects that combination).
    let deep_archive = state.archive_store.as_ref().map(|store| DeepArchive {
        store: store.clone(),
        timeout: Duration::from_secs(state.config.deep_archive_timeout_secs),
        docker_bin: state.config.docker_bin.clone(),
    });
    let lease = state.lease_identity();
    let _sweep = (state.config.idle_timeout_secs > 0).then(|| {
        spawn_idle_sweep(
            state.table.clone(),
            state.runner.clone(),
            deep_archive,
            Duration::from_secs(state.config.idle_timeout_secs),
            crate::idle::DEFAULT_SWEEP_TICK,
            lease.clone(),
            cancel.clone(),
        )
    });

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, sessions = state.table.len().await, "orchestrator control plane listening");
    eprintln!(
        "orchestrator listening on http://{listen} ({} sessions)",
        state.table.len().await
    );

    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = cancel.cancelled() => {},
            }
        })
        .await
        .map_err(|e| OrchestratorError::Runner(e.to_string()))?;
    // Graceful shutdown: hand our sessions back immediately instead of
    // making the surviving replicas wait out the lease TTL.
    if let Some(lease) = lease {
        if let Err(e) = state
            .table
            .store()
            .release_all_leases(&lease.replica_id)
            .await
        {
            tracing::warn!(error = %e, "lease release on shutdown failed (expiry covers it)");
        }
    }
    Ok(())
}
