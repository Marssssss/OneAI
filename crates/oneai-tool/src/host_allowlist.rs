//! Per-host network allow-list — the session store backing the code-mode
//! egress gate (#28 Stage 1).
//!
//! The local [`NetworkProxy`](crate::network_proxy::NetworkProxy) consults a
//! `HostAllowlistStore` before tunnelling a sandboxed process's outbound
//! connection. An approved host (one the user admitted via the
//! `InteractionRequest::NetworkApproval` prompt) is recorded here so
//! subsequent connections to the same host don't re-prompt within the session.
//!
//! v1 ships the in-memory (session-scoped) implementation. A persistent,
//! SQLite-backed implementation is a follow-up — the trait is the seam.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::RwLock;

/// A store of hosts the user has approved for sandboxed egress.
///
/// Implementations need not be crash-durable; session scope is the v1
/// contract. `host` is the bare hostname (no port, lowercased by the caller).
#[async_trait::async_trait]
pub trait HostAllowlistStore: Send + Sync {
    /// Whether `host` is on the approved list.
    async fn is_allowed(&self, host: &str) -> bool;

    /// Add `host` to the approved list (idempotent).
    async fn add(&self, host: String);
}

/// Session-scoped (in-memory) allow-list. Lost when the process exits — the
/// deliberate v1 posture: a fresh session re-prompts, so a once-approved
/// (perhaps mistaken) host doesn't silently persist forever.
pub struct InMemoryHostAllowlist {
    hosts: Arc<RwLock<HashSet<String>>>,
}

impl InMemoryHostAllowlist {
    pub fn new() -> Self {
        Self {
            hosts: Arc::new(RwLock::new(HashSet::new())),
        }
    }
}

impl Default for InMemoryHostAllowlist {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl HostAllowlistStore for InMemoryHostAllowlist {
    async fn is_allowed(&self, host: &str) -> bool {
        self.hosts.read().await.contains(host)
    }

    async fn add(&self, host: String) {
        self.hosts.write().await.insert(host);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_add_then_allowed() {
        let store = InMemoryHostAllowlist::new();
        assert!(!store.is_allowed("example.com").await);
        store.add("example.com".to_string()).await;
        assert!(store.is_allowed("example.com").await);
        // case-sensitive as-stored (callers lower-case); a different host stays out
        assert!(!store.is_allowed("other.com").await);
    }
}
