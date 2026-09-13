//! Pg backend selection for memory / usage / host-allowlist (MVS3-B —
//! storage externalization).
//!
//! Same rule as `working_state.rs`: when `ONEAI_PG_DSN` is set AND this
//! binary was compiled with the `postgres` feature, the durable stores live
//! in the shared Postgres (`PgMemoryStore` — pgvector KNN for LTM search,
//! `PgUsageTracker`, `PgHostAllowlist`); otherwise they stay on the local
//! SQLite defaults. A DSN that is set but unusable (unreachable server,
//! missing pgvector extension) degrades HONESTLY: loud warning per store,
//! SQLite fallback — never a silent divergence.
//!
//! Call [`apply_pg_backends`] right AFTER `sqlite_persistence*()` (the local
//! SQLite stays wired for feedback / thinking-effort / session-metadata
//! edits — `AppBuilder::memory_persistence` only takes over the
//! MemoryManager) and before `build()`.
//!
//! The second tuple element is the `host/*` JSON-RPC handle backed by the
//! SAME `Arc<PgHostAllowlist>` injected into the builder (one pool shared by
//! the engine's proxy and the RPC) — only `cmd_app_server` consumes it;
//! other entry points drop it.

#[cfg(feature = "postgres")]
use std::sync::Arc;

use oneai_app::AppBuilder;

use crate::working_state::{pg_dsn, PG_DSN_ENV};

/// Wire the shared-Pg memory/usage/host-allowlist backends onto `builder`
/// when selected. See the module docs for the contract.
pub(crate) async fn apply_pg_backends(
    builder: AppBuilder,
) -> (AppBuilder, Option<oneai_app_server::SharedHostAllowlistRpc>) {
    let Some(dsn) = pg_dsn() else {
        return (builder, None);
    };
    #[cfg(feature = "postgres")]
    {
        let mut builder = builder;

        // Memory (STM/LTM/conversations/facts). Overrides the MemoryManager
        // that sqlite_persistence() wired; the sqlite store itself stays for
        // feedback / thinking-effort (see module docs).
        match oneai_persistence::PgMemoryStore::connect(&dsn).await {
            Ok(store) => {
                eprintln!("   memory: Postgres (shared)");
                builder = builder.memory_persistence(Arc::new(store));
            }
            Err(e) => eprintln!(
                "Warning: {PG_DSN_ENV} is set but PgMemoryStore connect failed: {e} \
                 — falling back to SQLite memory persistence"
            ),
        }

        // Usage ledger (explicit tracker wins over the builder's auto-wired
        // SqliteUsageTracker fallback).
        match oneai_persistence::PgUsageTracker::connect(&dsn).await {
            Ok(tracker) => {
                eprintln!("   usage: Postgres (shared)");
                builder = builder.usage_tracker(Arc::new(tracker));
            }
            Err(e) => eprintln!(
                "Warning: {PG_DSN_ENV} is set but PgUsageTracker connect failed: {e} \
                 — falling back to SQLite usage tracking"
            ),
        }

        // Host allow/deny (shared across containers; the builder still wraps
        // it in SeededHostAllowlist). The same Arc backs the host/* RPC.
        let host_rpc = match oneai_persistence::PgHostAllowlist::connect(&dsn).await {
            Ok(store) => {
                eprintln!("   host-allowlist: Postgres (shared)");
                let store = Arc::new(store);
                builder = builder.host_allowlist_store(store.clone());
                Some(Arc::new(PgHostAllowlistRpc { store })
                    as oneai_app_server::SharedHostAllowlistRpc)
            }
            Err(e) => {
                eprintln!(
                    "Warning: {PG_DSN_ENV} is set but PgHostAllowlist connect failed: {e} \
                     — falling back to SQLite host allowlist"
                );
                None
            }
        };

        (builder, host_rpc)
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = &dsn;
        eprintln!(
            "Warning: {PG_DSN_ENV} is set but this binary was built without the `postgres` \
             feature — using SQLite memory/usage/host-allowlist"
        );
        (builder, None)
    }
}

/// `host/*` JSON-RPC adapter over the shared Pg store — the MVS3-B parallel
/// of `cmd_app_server`'s `AppHostAllowlistRpc` (which wraps
/// `SqliteHostAllowlist`). Delegates to the same inherent CRUD surface, so
/// a host admitted via the web UI lands in the same table every container's
/// proxy reads. Backend errors are swallowed + warned by the store (never a
/// panic, never a turn failure).
#[cfg(feature = "postgres")]
struct PgHostAllowlistRpc {
    store: Arc<oneai_persistence::PgHostAllowlist>,
}

#[cfg(feature = "postgres")]
#[async_trait::async_trait]
impl oneai_app_server::HostAllowlistRpc for PgHostAllowlistRpc {
    async fn list_allowed(&self) -> Vec<oneai_core::HostAllowEntry> {
        self.store.list_allowed().await
    }

    async fn list_denied(&self) -> Vec<oneai_core::HostAllowEntry> {
        self.store.list_denied().await
    }

    async fn admit(&self, host: String) {
        // `add`/`add_denied` are the HostAllowlistStore trait methods.
        oneai_core::HostAllowlistStore::add(&*self.store, host).await;
    }

    async fn deny(&self, host: String) {
        oneai_core::HostAllowlistStore::add_denied(&*self.store, host).await;
    }

    async fn remove(&self, host: String) {
        self.store.remove(&host).await;
    }

    async fn remove_denied(&self, host: String) {
        self.store.remove_denied(&host).await;
    }
}
