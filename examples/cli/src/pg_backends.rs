//! Pg backend selection for memory / usage / host-allowlist / session-events
//! / feedback (MVS3-B + MVS3-C — storage externalization).
//!
//! Same rule as `working_state.rs`: when `ONEAI_PG_DSN` is set AND this
//! binary was compiled with the `postgres` feature, the durable stores live
//! in the shared Postgres (`PgMemoryStore` — pgvector KNN for LTM search,
//! `PgUsageTracker`, `PgHostAllowlist`, `PgSessionEventStore`,
//! `PgFeedbackStore`); otherwise they stay on the local SQLite/file
//! defaults. A DSN that is set but unusable (unreachable server, missing
//! pgvector extension) degrades HONESTLY: loud warning per store, local
//! fallback — never a silent divergence.
//!
//! Call [`apply_pg_backends`] right AFTER `sqlite_persistence*()` (the local
//! SQLite stays wired for thinking-effort / session-metadata edits —
//! `AppBuilder::memory_persistence` only takes over the MemoryManager) and
//! before `build()`.
//!
//! Tuple elements 2+3 are the `host/*` and `feedback/*` JSON-RPC handles
//! backed by the SAME `Arc`s injected into the builder (one pool shared by
//! the engine and the RPCs) — only `cmd_app_server` consumes them; other
//! entry points drop them. The session-event store needs no RPC handle: it
//! goes straight onto the builder (`AppBuilder::session_event_store`) and
//! the existing `session/trajectory` probe reads it through the App.

#[cfg(feature = "postgres")]
use std::sync::Arc;

use oneai_app::AppBuilder;

use crate::working_state::{pg_dsn, PG_DSN_ENV};

/// Wire the shared-Pg memory/usage/host-allowlist/session-event/feedback
/// backends onto `builder` when selected. See the module docs for the
/// contract.
pub(crate) async fn apply_pg_backends(
    builder: AppBuilder,
) -> (
    AppBuilder,
    Option<oneai_app_server::SharedHostAllowlistRpc>,
    Option<oneai_app_server::SharedFeedbackStore>,
) {
    let Some(dsn) = pg_dsn() else {
        return (builder, None, None);
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

        // Session-event trajectory log (MVS3-C). The trait lives in core, so
        // the store goes straight onto the builder — session/trajectory then
        // replays from Pg (survives volume loss, cross-container queryable).
        match oneai_persistence::PgSessionEventStore::connect(&dsn).await {
            Ok(store) => {
                eprintln!("   session-events: Postgres (shared)");
                builder = builder.session_event_store(Arc::new(store));
            }
            Err(e) => eprintln!(
                "Warning: {PG_DSN_ENV} is set but PgSessionEventStore connect failed: {e} \
                 — falling back to the file session-event store"
            ),
        }

        // Per-message feedback (MVS3-C). The FeedbackStore TRAIT lives in
        // oneai-app-server (persistence must not depend on it), so the store
        // exposes an inherent record/list surface and this adapter implements
        // the trait — the exact pattern of PgHostAllowlistRpc below.
        let feedback_rpc = match oneai_persistence::PgFeedbackStore::connect(&dsn).await {
            Ok(store) => {
                eprintln!("   feedback: Postgres (shared)");
                Some(Arc::new(PgFeedbackStoreRpc {
                    store: Arc::new(store),
                }) as oneai_app_server::SharedFeedbackStore)
            }
            Err(e) => {
                eprintln!(
                    "Warning: {PG_DSN_ENV} is set but PgFeedbackStore connect failed: {e} \
                     — falling back to SQLite feedback"
                );
                None
            }
        };

        (builder, host_rpc, feedback_rpc)
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = &dsn;
        eprintln!(
            "Warning: {PG_DSN_ENV} is set but this binary was built without the `postgres` \
             feature — using SQLite memory/usage/host-allowlist/session-events/feedback"
        );
        (builder, None, None)
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

/// `feedback/*` JSON-RPC adapter over the shared Pg store — the MVS3-C
/// parallel of `PgHostAllowlistRpc` (and of `cmd_app_server`'s
/// `AppFeedbackStore`, which wraps the local SQLite path). The
/// `FeedbackStore` trait lives in `oneai-app-server` and
/// `oneai-persistence` must not depend on it, hence this thin delegate to
/// the store's inherent `record`/`list` surface. Error semantics are the
/// store's (record = logged no-op, list = empty on failure) — feedback is
/// non-critical UX state, never an RPC error, never a panic.
#[cfg(feature = "postgres")]
struct PgFeedbackStoreRpc {
    store: Arc<oneai_persistence::PgFeedbackStore>,
}

#[cfg(feature = "postgres")]
#[async_trait::async_trait]
impl oneai_app_server::FeedbackStore for PgFeedbackStoreRpc {
    async fn record(
        &self,
        session_id: &str,
        turn_id: &str,
        message_role: &str,
        kind: &str,
        text: Option<&str>,
    ) {
        self.store
            .record(session_id, turn_id, message_role, kind, text)
            .await
    }

    async fn list(&self, session_id: &str) -> Vec<oneai_core::FeedbackEntry> {
        self.store.list(session_id).await
    }
}
