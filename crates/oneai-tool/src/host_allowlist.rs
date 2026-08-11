//! Per-host network allow/deny store — backs the code-mode egress gate
//! (#28 Stage 1 + Stage 6).
//!
//! The local [`NetworkProxy`](crate::network_proxy::NetworkProxy) consults a
//! [`HostAllowlistStore`] before tunnelling a sandboxed process's outbound
//! connection. An approved host is tunneled straight through; a denied host
//! is blocked without re-prompting. Hosts the user admitted (or denied) via
//! the `InteractionRequest::NetworkApproval` prompt are recorded here so
//! subsequent connections to the same host don't re-prompt within the session.
//!
//! The [`HostAllowlistStore`] trait lives in `oneai-core` (a storage trait,
//! peer to `StatePersistence`/`MemoryPersistence`); this module ships the
//! session-scoped in-memory impls + the seeded wrapper. A durable,
//! SQLite-backed impl lives in `oneai-persistence` (`SqliteHostAllowlist`).
//!
//! Re-exported here so `oneai_tool::HostAllowlistStore` stays a valid path.

/// Re-export the core trait so callers that already `use oneai_tool::...` keep
/// compiling after the trait moved to `oneai-core` (API stability, v0.2.0).
pub use oneai_core::HostAllowlistStore;

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::RwLock;

/// Session-scoped (in-memory) allow + deny store. Lost when the process
/// exits — the deliberate v1 posture for hosts that aren't also persisted
/// via `SqliteHostAllowlist`: a fresh session re-prompts, so a once-approved
/// (perhaps mistaken) host doesn't silently persist forever.
pub struct InMemoryHostAllowlist {
    hosts: Arc<RwLock<HashSet<String>>>,
    denied: Arc<RwLock<HashSet<String>>>,
}

impl InMemoryHostAllowlist {
    pub fn new() -> Self {
        Self {
            hosts: Arc::new(RwLock::new(HashSet::new())),
            denied: Arc::new(RwLock::new(HashSet::new())),
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

    async fn is_denied(&self, host: &str) -> bool {
        self.denied.read().await.contains(host)
    }

    async fn add_denied(&self, host: String) {
        self.denied.write().await.insert(host);
    }
}

/// Hosts pre-approved for sandboxed egress so that the common
/// package-manager flows (`npm install`, `pip install`, `cargo build`,
/// `rustup toolchain install`) work without a per-host prompt (#28 Stage 3).
///
/// Deliberately tight: only the *language-package-registry* origin hosts, not
/// general-purpose CDNs or git hosts. A `curl github.com/...` exfil attempt
/// still prompts (github.com is high-value and not in the seed); the registry
/// hosts are public and low-value as exfil targets, and auto-approving them is
/// the difference between "npm install works" and "every install prompts".
///
/// Lower-cased bare hostnames (no port, no scheme) — the caller lower-cases.
pub const DEFAULT_EGRESS_SEED_HOSTS: &[&str] = &[
    "registry.npmjs.org",     // npm
    "pypi.org",               // pip index
    "files.pythonhosted.org", // pip package blobs
    "crates.io",              // cargo index (https)
    "index.crates.io",        // cargo sparse index
    "static.crates.io",       // cargo index blobs
    "static.rust-lang.org",   // rustup / toolchain downloads
];

/// A [`HostAllowlistStore`] that admits a static set of trusted seed hosts on
/// top of a (session- or persistent-scoped) inner store (#28 Stage 3).
///
/// `is_allowed` returns `true` if the host is in the seed set OR already
/// recorded in the inner store; `add` delegates to the inner store (seeds are
/// not user-removable — they're the baseline trust, not per-session approvals).
///
/// The typical wiring is `SeededHostAllowlist::new(InMemoryHostAllowlist)` —
/// the seed keeps `npm install`/`pip install`/`cargo build` prompt-free, the
/// inner store records hosts the user approved at the `NetworkApproval`
/// prompt so subsequent connections within the session don't re-prompt.
pub struct SeededHostAllowlist {
    seeds: HashSet<String>,
    inner: Arc<dyn HostAllowlistStore>,
}

impl SeededHostAllowlist {
    /// Wrap `inner` with the default seed set ([`DEFAULT_EGRESS_SEED_HOSTS`]).
    pub fn new(inner: Arc<dyn HostAllowlistStore>) -> Self {
        Self::with_seeds(inner, DEFAULT_EGRESS_SEED_HOSTS)
    }

    /// Wrap `inner` with an explicit seed set (test seam / bespoke policy).
    pub fn with_seeds(inner: Arc<dyn HostAllowlistStore>, seeds: &[&str]) -> Self {
        Self {
            seeds: seeds.iter().map(|s| s.to_ascii_lowercase()).collect(),
            inner,
        }
    }
}

#[async_trait::async_trait]
impl HostAllowlistStore for SeededHostAllowlist {
    async fn is_allowed(&self, host: &str) -> bool {
        // Lower-case compare so callers can pass either case; the seed list is
        // stored lower-cased and the inner store's contract is bare lowercased
        // hostnames, so this keeps both halves consistent.
        self.seeds.contains(&host.to_ascii_lowercase()) || self.inner.is_allowed(host).await
    }

    async fn add(&self, host: String) {
        self.inner.add(host).await;
    }

    // Denials always delegate to the inner store — seeds are baseline-trust
    // admits, never denials, so a seed host can't be denied by recording it
    // (the seed always re-admits). A denied host the user explicitly blocked
    // at the gate is recorded in the inner store.
    async fn is_denied(&self, host: &str) -> bool {
        self.inner.is_denied(host).await
    }

    async fn add_denied(&self, host: String) {
        self.inner.add_denied(host).await;
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

    #[tokio::test]
    async fn seeded_admits_seed_hosts_without_inner_add() {
        let inner = Arc::new(InMemoryHostAllowlist::new()) as Arc<dyn HostAllowlistStore>;
        let store = SeededHostAllowlist::new(inner);
        for host in DEFAULT_EGRESS_SEED_HOSTS {
            assert!(
                store.is_allowed(host).await,
                "seed host {host} should be admitted"
            );
        }
    }

    #[tokio::test]
    async fn seeded_admits_inner_approved_host() {
        let inner = Arc::new(InMemoryHostAllowlist::new()) as Arc<dyn HostAllowlistStore>;
        let store = SeededHostAllowlist::new(inner.clone());
        assert!(!store.is_allowed("evil.example").await);
        store.add("evil.example".to_string()).await;
        assert!(store.is_allowed("evil.example").await);
        // add delegated to inner, not the seed set
        assert!(inner.is_allowed("evil.example").await);
    }

    #[tokio::test]
    async fn seeded_case_insensitive_on_seed_lookup() {
        let inner = Arc::new(InMemoryHostAllowlist::new()) as Arc<dyn HostAllowlistStore>;
        let store = SeededHostAllowlist::new(inner);
        assert!(store.is_allowed("REGISTRY.NPMJS.ORG").await);
        assert!(store.is_allowed("PyPI.org").await);
    }

    #[tokio::test]
    async fn seeded_non_seed_non_approved_host_denied() {
        let inner = Arc::new(InMemoryHostAllowlist::new()) as Arc<dyn HostAllowlistStore>;
        let store = SeededHostAllowlist::new(inner);
        assert!(!store.is_allowed("github.com").await); // high-value host stays gated
        assert!(!store.is_allowed("evil.example").await);
    }
}
