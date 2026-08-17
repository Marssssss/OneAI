//! Durable host allow/deny list — the shared, front-end-agnostic handle for
//! the user's admitted (or blocked) sandbox-egress hosts, surfaced over the
//! `host/list` + `host/allow` + `host/deny` + `host/remove` +
//! `host/remove-denied` JSON-RPC methods.
//!
//! Mirrors [`crate::feedback::FeedbackStore`] in shape: an object-safe
//! `#[async_trait]` the app-server holds as `Arc<dyn HostAllowlistRpc + Send +
//! Sync>`, so the crate stays decoupled from `oneai-app` (it never touches an
//! `App` directly — the CLI passes a concrete impl wrapping a
//! `SqliteHostAllowlist` pointing at the same `~/.oneai/oneai.db` the engine's
//! `NetworkProxy` consults, so a host admitted via the web UI is honoured by
//! the proxy's next CONNECT without a shared in-memory `Arc`).
//!
//! The `host/*` methods are synchronous CRUD (like `feedback/submit` /
//! `session/list`) — they do not go through the bus/Directive path. A host
//! admission is user→engine metadata about future egress, not a turn action;
//! the engine never re-infers on it. The proxy's own `Proceed` path still
//! writes the durable store on a per-session allow; these RPCs let the web UI
//! do the same cross-session ("always") and audit/revoke it.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

// `HostAllowEntry` is defined in `oneai-core` (shared with the persistence
// layer); re-export here so `lib.rs` can `pub use host_allowlist::…` and so
// this module names the type directly.
pub use oneai_core::HostAllowEntry;

/// A handle to the durable host allow/deny store, backing `host/*` JSON-RPC.
/// Object-safe via `#[async_trait]` so the app-server holds
/// `Arc<dyn HostAllowlistRpc + Send + Sync>` and tests can swap an in-memory
/// impl. The production impl (in `cmd_app_server`) wraps a
/// `SqliteHostAllowlist` (sharing `~/.oneai/oneai.db`) and delegates to its
/// inherent CRUD methods; a backend that fails surfaces as an empty `Vec` /
/// silent no-op — never a panic, never a turn failure (mirrors `FeedbackStore`).
#[async_trait]
pub trait HostAllowlistRpc: Send + Sync {
    /// All admitted hosts (the `host_allowlist` table), ordered by host.
    async fn list_allowed(&self) -> Vec<HostAllowEntry>;

    /// All denied hosts (the `host_denylist` table), ordered by host.
    async fn list_denied(&self) -> Vec<HostAllowEntry>;

    /// Admit `host` (persistently — survives restart, honoured by the proxy).
    /// Idempotent; admitting a previously-denied host clears the denial.
    async fn admit(&self, host: String);

    /// Deny `host` persistently — future tunnel attempts are blocked without
    /// re-prompting. Idempotent; denying a previously-admitted host clears the
    /// admission.
    async fn deny(&self, host: String);

    /// Remove `host` from the allowlist (revoke an admission).
    async fn remove(&self, host: String);

    /// Remove `host` from the denylist (revoke a denial).
    async fn remove_denied(&self, host: String);
}

/// Shared, thread-safe handle threaded through `serve_all` → transports →
/// `serve_connection` → `handle_request` for the `host/*` methods.
/// `Arc<dyn HostAllowlistRpc + Send + Sync>`.
pub type SharedHostAllowlistRpc = Arc<dyn HostAllowlistRpc + Send + Sync>;

/// An in-memory [`HostAllowlistRpc`] for tests — no IO, deterministic. Two
/// `Mutex<Vec<HostAllowEntry>>` (allowed + denied) so the trait is `&self` +
/// object-safe. `recorded_at_ms` is 0 (tests don't order by it).
pub struct InMemoryHostAllowlistRpc {
    allowed: Mutex<Vec<HostAllowEntry>>,
    denied: Mutex<Vec<HostAllowEntry>>,
}

impl Default for InMemoryHostAllowlistRpc {
    fn default() -> Self {
        Self {
            allowed: Mutex::new(Vec::new()),
            denied: Mutex::new(Vec::new()),
        }
    }
}

impl InMemoryHostAllowlistRpc {
    pub fn new() -> Self {
        Self::default()
    }

    fn lc(host: String) -> String {
        host.to_ascii_lowercase()
    }

    /// Swap a host from the denied list into the allowed list (mirrors the
    /// durable store's mutual-exclusion invariant).
    async fn move_to_allowed(&self, host: String) {
        let host = Self::lc(host);
        {
            let mut d = self.denied.lock().await;
            d.retain(|e| e.host != host);
        }
        let mut a = self.allowed.lock().await;
        if !a.iter().any(|e| e.host == host) {
            a.push(HostAllowEntry {
                host,
                recorded_at_ms: 0,
            });
            a.sort_by(|x, y| x.host.cmp(&y.host));
        }
    }

    async fn move_to_denied(&self, host: String) {
        let host = Self::lc(host);
        {
            let mut a = self.allowed.lock().await;
            a.retain(|e| e.host != host);
        }
        let mut d = self.denied.lock().await;
        if !d.iter().any(|e| e.host == host) {
            d.push(HostAllowEntry {
                host,
                recorded_at_ms: 0,
            });
            d.sort_by(|x, y| x.host.cmp(&y.host));
        }
    }
}

#[async_trait]
impl HostAllowlistRpc for InMemoryHostAllowlistRpc {
    async fn list_allowed(&self) -> Vec<HostAllowEntry> {
        self.allowed.lock().await.clone()
    }

    async fn list_denied(&self) -> Vec<HostAllowEntry> {
        self.denied.lock().await.clone()
    }

    async fn admit(&self, host: String) {
        self.move_to_allowed(host).await;
    }

    async fn deny(&self, host: String) {
        self.move_to_denied(host).await;
    }

    async fn remove(&self, host: String) {
        let host = Self::lc(host);
        self.allowed.lock().await.retain(|e| e.host != host);
    }

    async fn remove_denied(&self, host: String) {
        let host = Self::lc(host);
        self.denied.lock().await.retain(|e| e.host != host);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admit_then_list_then_remove() {
        let store = InMemoryHostAllowlistRpc::new();
        store.admit("Beta.example".into()).await; // case-normalized
        store.admit("alpha.example".into()).await;
        let allowed = store.list_allowed().await;
        assert_eq!(allowed.len(), 2);
        assert_eq!(allowed[0].host, "alpha.example"); // sorted
        assert_eq!(allowed[1].host, "beta.example");
        store.remove("alpha.example".into()).await;
        assert_eq!(store.list_allowed().await.len(), 1);
    }

    #[tokio::test]
    async fn deny_moves_out_of_allowed() {
        let store = InMemoryHostAllowlistRpc::new();
        store.admit("flaky.example".into()).await;
        assert_eq!(store.list_allowed().await.len(), 1);
        store.deny("flaky.example".into()).await;
        assert!(store.list_allowed().await.is_empty());
        assert_eq!(store.list_denied().await.len(), 1);
    }

    #[tokio::test]
    async fn admit_moves_out_of_denied() {
        let store = InMemoryHostAllowlistRpc::new();
        store.deny("flaky.example".into()).await;
        store.admit("flaky.example".into()).await;
        assert!(store.list_denied().await.is_empty());
        assert_eq!(store.list_allowed().await.len(), 1);
    }

    #[tokio::test]
    async fn remove_denied_revokes() {
        let store = InMemoryHostAllowlistRpc::new();
        store.deny("bad.example".into()).await;
        store.remove_denied("bad.example".into()).await;
        assert!(store.list_denied().await.is_empty());
    }
}
