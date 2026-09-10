//! Session lifecycle FSM (design doc D6).
//!
//! ```text
//! Creating ──▶ Running ──idle超时──▶ Hibernating ──请求到达──▶ Resuming ──▶ Running
//!    │            │                      │
//!    │            └──崩溃──▶ Crashed ─────┴──显式删除──▶ Destroyed
//!    └──拉起失败──▶ Failed
//! ```

use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::error::{OrchestratorError, Result};
use crate::runner::{ContainerHandle, SessionSpec};

/// Session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SessionState {
    /// Container spawn in flight.
    Creating,
    /// Container up, engine port accepting connections.
    Running,
    /// `docker stop`ped for idleness; volumes preserved.
    Hibernating,
    /// Restart/recreate in flight (triggered by a request, D6).
    Resuming,
    /// Container died; volumes intact. Next request triggers Resuming.
    Crashed,
    /// Terminal spawn/resume failure (reason in `last_error`).
    Failed,
    /// Tombstone: container + volumes removed.
    Destroyed,
}

impl SessionState {
    /// Whether this state can serve WS traffic right now.
    pub fn is_runnable(self) -> bool {
        matches!(self, SessionState::Running)
    }

    /// Whether a WS upgrade should trigger a resume attempt.
    pub fn is_resumable(self) -> bool {
        matches!(self, SessionState::Hibernating | SessionState::Crashed)
    }

    /// Whether requests should wait (an operation is already in flight).
    pub fn is_pending(self) -> bool {
        matches!(self, SessionState::Creating | SessionState::Resuming)
    }
}

/// Validate a state transition against the D6 edge set (plus administrative
/// destroy: every non-terminal state may be force-deleted).
pub fn validate_transition(from: SessionState, to: SessionState) -> Result<()> {
    use SessionState::*;
    let ok = match (from, to) {
        // D6 edges.
        (Creating, Running) | (Creating, Failed) => true,
        (Running, Hibernating) | (Running, Crashed) => true,
        (Hibernating, Resuming) => true,
        (Resuming, Running) | (Resuming, Crashed) | (Resuming, Failed) => true,
        (Crashed, Resuming) => true,
        // Explicit deletion — allowed from any live state (administrative
        // override; DELETE must not be blocked by an in-flight operation).
        (Creating | Running | Hibernating | Resuming | Crashed | Failed, Destroyed) => true,
        // Destroyed is terminal.
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(OrchestratorError::IllegalTransition { from, to })
    }
}

/// In-memory session entry. Hot-path liveness fields are atomics (bumped by
/// the WS proxy without taking the routing-table lock); state changes go
/// through `RoutingTable::cas_transition`, which clones-and-replaces the Arc.
pub struct SessionEntry {
    /// Immutable spawn specification.
    pub spec: SessionSpec,
    /// Live container handle (`None` in Creating/Failed/Destroyed and in
    /// Crashed/Hibernating when the handle is stale-but-recoverable).
    pub handle: Option<ContainerHandle>,
    /// Current lifecycle state.
    pub state: SessionState,
    /// Last failure reason (Failed/Crashed diagnostics).
    pub last_error: Option<String>,
    /// Timestamp of the last state change.
    pub updated_at: DateTime<Utc>,
    /// Unix millis of the last proxied WS frame (idle sweep input).
    pub last_activity_ms: AtomicU64,
    /// Currently attached WS proxy connections (idle sweep veto).
    pub active_conns: AtomicUsize,
    /// Signalled when the session reaches Running (WS-upgrade waiters).
    /// Shared via Arc across clone-and-replace so waiters holding an older
    /// entry snapshot still wake up.
    pub ready_notify: Arc<Notify>,
}

impl SessionEntry {
    /// New entry in `Creating` with a fresh notify and activity clock.
    pub fn new_creating(spec: SessionSpec) -> Self {
        Self {
            spec,
            handle: None,
            state: SessionState::Creating,
            last_error: None,
            updated_at: Utc::now(),
            last_activity_ms: AtomicU64::new(now_millis()),
            active_conns: AtomicUsize::new(0),
            ready_notify: Arc::new(Notify::new()),
        }
    }

    /// Bump `last_activity_ms` to now (called by the WS proxy per frame).
    pub fn touch(&self) {
        self.last_activity_ms
            .store(now_millis(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Milliseconds since the last activity bump (or since creation).
    pub fn idle_ms(&self) -> u64 {
        now_millis().saturating_sub(
            self.last_activity_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Snapshot for JSON responses (no atomics).
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            session_id: self.spec.session_id.clone(),
            state: self.state,
            last_error: self.last_error.clone(),
            updated_at: self.updated_at,
            host_port: self.handle.as_ref().map(|h| h.host_port),
            container_name: self.handle.as_ref().map(|h| h.container_name.clone()),
            active_conns: self.active_conns.load(std::sync::atomic::Ordering::Relaxed),
            idle_ms: self.idle_ms(),
        }
    }
}

/// Atomic-clock helper: unix millis via `AtomicU64` (std has no
/// `AtomicInstant`).
fn now_millis() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

/// Clone copies plain fields and the *current values* of the atomics; the
/// `Arc<Notify>` is shared, not duplicated.
impl Clone for SessionEntry {
    fn clone(&self) -> Self {
        Self {
            spec: self.spec.clone(),
            handle: self.handle.clone(),
            state: self.state,
            last_error: self.last_error.clone(),
            updated_at: self.updated_at,
            last_activity_ms: AtomicU64::new(
                self.last_activity_ms
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            active_conns: AtomicUsize::new(
                self.active_conns.load(std::sync::atomic::Ordering::Relaxed),
            ),
            ready_notify: self.ready_notify.clone(),
        }
    }
}

/// JSON-serializable view of a session (API responses).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionSnapshot {
    pub session_id: String,
    pub state: SessionState,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub host_port: Option<u16>,
    pub container_name: Option<String>,
    pub active_conns: usize,
    pub idle_ms: u64,
}

/// On-disk persisted form of a session entry (registry file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedEntry {
    pub spec: SessionSpec,
    pub handle: Option<ContainerHandle>,
    pub state: SessionState,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl SessionEntry {
    /// Reduce to the persisted form.
    pub fn to_persisted(&self) -> PersistedEntry {
        PersistedEntry {
            spec: self.spec.clone(),
            handle: self.handle.clone(),
            state: self.state,
            last_error: self.last_error.clone(),
            updated_at: self.updated_at,
        }
    }

    /// Rehydrate from the persisted form (activity clock starts now).
    pub fn from_persisted(p: PersistedEntry) -> Self {
        Self {
            spec: p.spec,
            handle: p.handle,
            state: p.state,
            last_error: p.last_error,
            updated_at: p.updated_at,
            last_activity_ms: AtomicU64::new(now_millis()),
            active_conns: AtomicUsize::new(0),
            ready_notify: Arc::new(Notify::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionState::*;

    #[test]
    fn legal_d6_edges() {
        for (from, to) in [
            (Creating, Running),
            (Creating, Failed),
            (Running, Hibernating),
            (Running, Crashed),
            (Hibernating, Resuming),
            (Resuming, Running),
            (Resuming, Crashed),
            (Resuming, Failed),
            (Crashed, Resuming),
        ] {
            assert!(validate_transition(from, to).is_ok(), "{from:?}->{to:?}");
        }
    }

    #[test]
    fn admin_destroy_from_any_live_state() {
        for from in [Creating, Running, Hibernating, Resuming, Crashed, Failed] {
            assert!(validate_transition(from, Destroyed).is_ok(), "{from:?}");
        }
    }

    #[test]
    fn illegal_edges() {
        for (from, to) in [
            (Destroyed, Running),
            (Destroyed, Creating),
            (Running, Creating),
            (Running, Resuming),
            (Hibernating, Running), // must go through Resuming
            (Crashed, Running),     // must go through Resuming
            (Failed, Running),
            (Creating, Crashed),
            (Hibernating, Crashed),
            (Running, Failed),
        ] {
            assert!(validate_transition(from, to).is_err(), "{from:?}->{to:?}");
        }
    }

    #[test]
    fn state_predicates() {
        assert!(Running.is_runnable());
        assert!(!Hibernating.is_runnable());
        assert!(Hibernating.is_resumable() && Crashed.is_resumable());
        assert!(Creating.is_pending() && Resuming.is_pending());
        assert!(!Running.is_pending() && !Running.is_resumable());
    }

    #[test]
    fn entry_clone_shares_notify_and_copies_atomics() {
        let spec = SessionSpec {
            session_id: "s".into(),
            image: "i".into(),
            state_volume: "sv".into(),
            workspace_volume: "wv".into(),
            env: vec![],
            bind_host: "127.0.0.1".into(),
            container_port: 8787,
            provider_config: None,
            created_at: Utc::now(),
        };
        let e = SessionEntry::new_creating(spec);
        e.touch();
        e.active_conns
            .fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        let c = e.clone();
        assert!(Arc::ptr_eq(&e.ready_notify, &c.ready_notify));
        assert_eq!(c.active_conns.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(c.state, Creating);
    }

    #[test]
    fn persisted_roundtrip() {
        let spec = SessionSpec {
            session_id: "rt".into(),
            image: "img".into(),
            state_volume: "sv".into(),
            workspace_volume: "wv".into(),
            env: vec![("K".into(), "V".into())],
            bind_host: "127.0.0.1".into(),
            container_port: 8787,
            provider_config: None,
            created_at: Utc::now(),
        };
        let e = SessionEntry::new_creating(spec);
        let p = e.to_persisted();
        let json = serde_json::to_string(&p).unwrap();
        let p2: PersistedEntry = serde_json::from_str(&json).unwrap();
        let e2 = SessionEntry::from_persisted(p2);
        assert_eq!(e2.spec.session_id, "rt");
        assert_eq!(e2.spec.env, vec![("K".into(), "V".into())]);
        assert_eq!(e2.state, Creating);
    }
}
