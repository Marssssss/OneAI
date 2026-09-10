//! Idle-hibernation sweep (D6): periodically stop Running sessions that have
//! no attached WS connections and haven't proxied a frame within the idle
//! timeout. Volumes are preserved; the next request resumes (see
//! `server::resume_session`).

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::fsm::SessionState;
use crate::registry::RoutingTable;
use crate::runner::ContainerRunner;

/// Default sweep tick interval.
pub const DEFAULT_SWEEP_TICK: Duration = Duration::from_secs(30);

/// Spawn the sweep task. It runs until `cancel` is triggered (or the table
/// and runner are dropped). The candidate list is advisory — the CAS inside
/// is authoritative, so a session that gains a connection or transitions
/// between listing and CAS is never stopped.
pub fn spawn_idle_sweep(
    table: RoutingTable,
    runner: Arc<dyn ContainerRunner>,
    idle_timeout: Duration,
    tick: Duration,
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
            sweep_once(&table, runner.as_ref(), idle_timeout).await;
        }
        tracing::debug!("idle sweep task stopped");
    })
}

/// One sweep pass (exposed for tests).
pub async fn sweep_once(
    table: &RoutingTable,
    runner: &dyn ContainerRunner,
    idle_timeout: Duration,
) {
    let candidates = table
        .list_idle_candidates(idle_timeout.as_millis() as u64)
        .await;
    for id in candidates {
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
        sweep_once(&t, rec.as_ref(), Duration::from_secs(60)).await;

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
            Duration::from_secs(3600),
            Duration::from_millis(10),
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
