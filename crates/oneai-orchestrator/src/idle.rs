//! Idle-hibernation sweep (D6): periodically stop Running sessions that have
//! no attached WS connections and haven't proxied a frame within the idle
//! timeout. Volumes are preserved; the next request resumes (see
//! `server::resume_session`).
//!
//! Second tier (MVS3-C, `archive.rs`): when a [`DeepArchive`] bundle is
//! configured, sessions that stay `Hibernating` past its timeout have their
//! volumes exported to the archive store and the container + local volumes
//! destroyed — cold sessions stop consuming local disk. Data-loss red line:
//! `destroy(remove_volumes=true)` runs ONLY after `store.archive()`
//! confirmed success; any earlier failure leaves the session hibernating
//! locally and retries next sweep.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::archive::DeepArchive;
use crate::fsm::SessionState;
use crate::registry::RoutingTable;
use crate::runner::ContainerRunner;
use crate::store::{ClaimOutcome, LeaseGuard, LeaseIdentity};

/// Default sweep tick interval.
pub const DEFAULT_SWEEP_TICK: Duration = Duration::from_secs(30);

/// Spawn the sweep task. It runs until `cancel` is triggered (or the table
/// and runner are dropped). The candidate list is advisory — the CAS inside
/// is authoritative, so a session that gains a connection or transitions
/// between listing and CAS is never stopped.
///
/// `lease` (MVS4-A) scopes the sweep to this replica: on a leasing backend
/// every candidate must be lease-claimed before any container operation, so
/// exactly one replica acts and foreign-owned sessions are skipped.
pub fn spawn_idle_sweep(
    table: RoutingTable,
    runner: Arc<dyn ContainerRunner>,
    deep_archive: Option<DeepArchive>,
    idle_timeout: Duration,
    tick: Duration,
    lease: Option<LeaseIdentity>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            sweep_once(
                &table,
                runner.as_ref(),
                deep_archive.as_ref(),
                idle_timeout,
                lease.as_ref(),
            )
            .await;
        }
        tracing::debug!("idle sweep task stopped");
    })
}

/// Whether lease gating is active (identity present AND the store leases —
/// file mode skips gating entirely).
fn active_lease<'a>(
    table: &RoutingTable,
    lease: Option<&'a LeaseIdentity>,
) -> Option<&'a LeaseIdentity> {
    lease.filter(|_| table.store().supports_leasing())
}

/// One sweep pass (exposed for tests): hibernate idle Running sessions,
/// then deep-archive long-hibernating ones (when configured).
pub async fn sweep_once(
    table: &RoutingTable,
    runner: &dyn ContainerRunner,
    deep_archive: Option<&DeepArchive>,
    idle_timeout: Duration,
    lease: Option<&LeaseIdentity>,
) {
    let lease = active_lease(table, lease);
    let candidates = table
        .list_idle_candidates(idle_timeout.as_millis() as u64)
        .await;
    for id in candidates {
        // MVS4-A: claim ownership before touching the container. A miss
        // (HeldByOther) means a live replica owns the session — its own
        // sweep handles it; NotFound means it just died — the next pass
        // re-reads the store. The claim's lease then expires on its own
        // (no heartbeat here — ownership is per-action, not sticky).
        if let Some(l) = lease {
            match table
                .store()
                .try_claim_lease(&id, &l.replica_id, l.ttl)
                .await
            {
                Ok(ClaimOutcome::Owned { .. }) => {}
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!(session = %id, error = %e, "idle sweep: lease claim failed");
                    continue;
                }
            }
        }
        // CAS Running → Hibernating is the authoritative check; a miss means
        // the session moved (gained a conn / was deleted / crashed) — skip.
        let Ok(Some(_)) = table
            .cas_transition(
                &id,
                SessionState::Running,
                SessionState::Hibernating,
                None,
                None,
            )
            .await
        else {
            continue;
        };
        let Some(entry) = table.get(&id).await else {
            continue;
        };
        if let Some(handle) = &entry.handle {
            if let Err(e) = runner.stop(handle).await {
                tracing::warn!(session = %id, error = %e, "idle hibernate stop failed");
            }
        }
        tracing::info!(session = %id, "auto-hibernated (idle)");
    }

    if let Some(deep) = deep_archive {
        deep_archive_pass(table, runner, deep, lease).await;
    }
}

/// Deep-archive pass (MVS3-C): export the volumes of long-hibernating
/// sessions to the archive store, then destroy container + local volumes.
///
/// Per-session sequence (red line — see module docs):
/// 1. `store.archive(volumes)` — on failure: volumes untouched, `last_error`
///    recorded, retry next sweep.
/// 2. `cas_set_archived(manifest)` — the claim; a miss means a concurrent
///    pass already archived this session (our archive files are the same
///    deterministic layout — the winner's destroy handles cleanup).
/// 3. `runner.destroy(handle, true)` — removes container + volumes. A
///    failure here is loud but recoverable: the marker is already set and
///    the archive confirmed, so resume restores from the archive while
///    `spawn` tolerates the leftover container/volumes.
async fn deep_archive_pass(
    table: &RoutingTable,
    runner: &dyn ContainerRunner,
    deep: &DeepArchive,
    lease: Option<&LeaseIdentity>,
) {
    let candidates = table
        .list_deep_archive_candidates(deep.timeout.as_millis() as u64)
        .await;
    for id in candidates {
        let Some(entry) = table.get(&id).await else {
            continue;
        };
        // Re-validate under a fresh read (the candidate list is advisory).
        if entry.state != SessionState::Hibernating || entry.archived.is_some() {
            continue;
        }
        let Some(handle) = entry.handle.clone() else {
            continue;
        };
        let volumes = vec![
            entry.spec.state_volume.clone(),
            entry.spec.workspace_volume.clone(),
        ];

        // MVS4-A: own the session for the whole archive operation. The
        // export can run for minutes (docker save | gzip) — a bare claim
        // would expire mid-export and let another replica race a concurrent
        // tar into the same deterministic layout. The guard heartbeats the
        // lease until the pass finishes (drop → release).
        let _ownership = match lease {
            Some(l) => match table
                .store()
                .try_claim_lease(&id, &l.replica_id, l.ttl)
                .await
            {
                Ok(ClaimOutcome::Owned { .. }) => Some(LeaseGuard::start(
                    table.clone(),
                    id.clone(),
                    l.replica_id.clone(),
                    l.ttl,
                )),
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!(session = %id, error = %e, "deep archive: lease claim failed");
                    continue;
                }
            },
            None => None,
        };

        // 1. Export FIRST — until this succeeds, nothing may be removed.
        let manifest = match deep.store.archive(&id, &volumes, &deep.docker_bin).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    session = %id, error = %e,
                    "deep-archive export failed — volumes intact, retrying next sweep"
                );
                let _ = table
                    .set_last_error(&id, format!("deep-archive failed: {e}"))
                    .await;
                continue;
            }
        };

        // 2. Claim (CAS on the archived marker; clears the stale handle).
        match table.cas_set_archived(&id, Some(manifest.clone())).await {
            Ok(true) => {}
            Ok(false) => {
                // Concurrent pass won the claim — same deterministic layout,
                // nothing to undo; the winner runs the destroy.
                tracing::debug!(session = %id, "deep-archive claim lost to a concurrent pass");
                continue;
            }
            Err(e) => {
                tracing::error!(session = %id, error = %e, "deep-archive claim failed");
                continue;
            }
        }

        // 3. Only now may the local copies go.
        if let Err(e) = runner.destroy(&handle, true).await {
            // Recoverable: archive confirmed + marker set; resume restores
            // from the archive and spawn tolerates the leftover container.
            tracing::error!(
                session = %id, error = %e,
                "deep-archive: container/volume destroy failed after successful archive \
                 (resume still restores from the archive; leftover volumes may need \
                 manual `docker volume rm`)"
            );
            let _ = table
                .set_last_error(&id, format!("deep-archive destroy failed: {e}"))
                .await;
        }
        tracing::info!(
            session = %id,
            volumes = manifest.volumes.len(),
            bytes = manifest.volumes.iter().map(|v| v.size_bytes).sum::<u64>(),
            "deep-archived (volumes exported, container + local volumes removed)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::tests::test_spec;
    use crate::runner::ContainerHandle;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    /// Minimal fake: records stop calls, everything else Ok.
    struct StopRecorder {
        stops: Mutex<Vec<String>>,
        stop_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ContainerRunner for StopRecorder {
        async fn spawn(
            &self,
            spec: &crate::runner::SessionSpec,
        ) -> crate::error::Result<ContainerHandle> {
            Ok(ContainerHandle {
                container_id: "x".into(),
                container_name: spec.container_name(),
                host_port: 1,
            })
        }
        async fn stop(&self, handle: &ContainerHandle) -> crate::error::Result<()> {
            self.stop_count.fetch_add(1, Ordering::Relaxed);
            self.stops.lock().await.push(handle.container_name.clone());
            Ok(())
        }
        async fn start(&self, handle: &ContainerHandle) -> crate::error::Result<ContainerHandle> {
            Ok(handle.clone())
        }
        async fn health(&self, _handle: &ContainerHandle) -> crate::error::Result<bool> {
            Ok(true)
        }
        async fn commit(&self, _h: &ContainerHandle, _tag: &str) -> crate::error::Result<()> {
            Ok(())
        }
        async fn destroy(&self, _h: &ContainerHandle, _v: bool) -> crate::error::Result<()> {
            Ok(())
        }
    }

    async fn running_table(dir: &std::path::Path, ids: &[&str]) -> RoutingTable {
        let t = RoutingTable::new(dir);
        for id in ids {
            t.insert_new(crate::fsm::SessionEntry::new_creating(test_spec(id)))
                .await
                .unwrap();
            t.cas_transition(
                id,
                SessionState::Creating,
                SessionState::Running,
                Some(ContainerHandle {
                    container_id: "cid".into(),
                    container_name: format!("oneai-orch-{id}"),
                    host_port: 1000,
                }),
                None,
            )
            .await
            .unwrap();
        }
        t
    }

    #[tokio::test]
    async fn sweep_stops_idle_sessions_only() {
        let dir = tempfile::tempdir().unwrap();
        let t = running_table(dir.path(), &["idle1", "busy", "fresh"]).await;
        // busy: attached connection vetoes.
        t.get("busy")
            .await
            .unwrap()
            .active_conns
            .fetch_add(1, Ordering::Relaxed);
        // fresh: recent activity vetoes (touch happened at creation; use a
        // timeout large enough that only never-touched-since entries pass —
        // instead make idle1 explicitly old).
        t.get("idle1")
            .await
            .unwrap()
            .last_activity_ms
            .store(1, Ordering::Relaxed); // ancient

        let rec = Arc::new(StopRecorder {
            stops: Mutex::new(Vec::new()),
            stop_count: AtomicUsize::new(0),
        });
        sweep_once(&t, rec.as_ref(), None, Duration::from_secs(60), None).await;

        assert_eq!(rec.stop_count.load(Ordering::Relaxed), 1);
        assert_eq!(*rec.stops.lock().await, vec!["oneai-orch-idle1"]);
        assert_eq!(
            t.get("idle1").await.unwrap().state,
            SessionState::Hibernating
        );
        assert_eq!(t.get("busy").await.unwrap().state, SessionState::Running);
        assert_eq!(t.get("fresh").await.unwrap().state, SessionState::Running);
    }

    #[tokio::test]
    async fn sweep_task_respects_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        let rec = Arc::new(StopRecorder {
            stops: Mutex::new(Vec::new()),
            stop_count: AtomicUsize::new(0),
        });
        let cancel = CancellationToken::new();
        let h = spawn_idle_sweep(
            t,
            rec.clone(),
            None,
            Duration::from_secs(3600),
            Duration::from_millis(10),
            None,
            cancel.clone(),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        // Task must finish promptly after cancellation.
        tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("sweep task did not stop on cancel")
            .unwrap();
    }
}
