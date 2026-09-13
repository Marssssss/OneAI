//! Postgres-backed host allow/deny store (MVS3-B — storage externalization,
//! see `docs/cloud-orchestrator-design.md` §6/§7).
//!
//! Drop-in replacement for [`crate::SqliteHostAllowlist`] behind the same
//! `HostAllowlistStore` trait, for cloud deployments where N session
//! containers share one Postgres: a host the user admitted (or blocked) in
//! one container is honoured by every other container — and by the next
//! container that replaces a killed one — without re-prompting.
//!
//! Semantics mirror the SQLite backend exactly:
//! - hosts are lower-cased before storage/lookup;
//! - `add` / `add_denied` are mutually exclusive (admitting clears a prior
//!   denial and vice versa) — done in ONE transaction here, so a concurrent
//!   reader never observes a host on both lists;
//! - read failures fail CLOSED for `is_allowed`/`is_denied` (both return
//!   `false`, which routes to the gate-prompt path — the safe default, same
//!   as the SQLite backend); write failures are warned + swallowed (the
//!   trait returns no Result).
//!
//! `SeededHostAllowlist` (the 7-source seed decorator) continues to wrap
//! this store at the AppBuilder layer — unchanged.
//!
//! Schema: two `_pg`-suffixed tables mirroring the SQLite ones; `recorded_at`
//! stays unix-seconds BIGINT (audit-only column).
//!
//! ## Feature gate + selection
//! Compiled only under `--features postgres` (default off). At runtime the
//! CLI selects this backend when `ONEAI_PG_DSN` is set (see
//! `examples/cli/src/pg_backends.rs`).

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use deadpool_postgres::Pool;
use oneai_core::HostAllowlistStore;

use crate::pg_common::{
    build_pool, ensure_schema as common_ensure_schema, pg_err, pool_err, ADVISORY_LOCK_BASE,
};

/// Catalog probe: TRUE when every object this store needs already exists.
const SCHEMA_EXISTS_SQL: &str = "SELECT to_regclass('host_allowlist_pg') IS NOT NULL \
     AND to_regclass('host_denylist_pg') IS NOT NULL";

/// DDL applied idempotently on first use (see `pg_common::ensure_schema`).
const SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS host_allowlist_pg (
    host        TEXT PRIMARY KEY,
    recorded_at BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS host_denylist_pg (
    host        TEXT PRIMARY KEY,
    recorded_at BIGINT NOT NULL
);
"#;

/// This store's advisory-lock key (see the registry in `pg_common`).
const LOCK_KEY: i64 = ADVISORY_LOCK_BASE + 3;

/// Postgres-backed, persistent host allow + deny store. See module docs.
pub struct PgHostAllowlist {
    pool: Pool,
    /// DDL applied at most once per store instance.
    schema_ready: tokio::sync::OnceCell<()>,
}

impl PgHostAllowlist {
    /// Connect to `dsn` (a libpq connection string) and build a pooled store.
    pub async fn connect(dsn: &str) -> Result<Self, oneai_core::error::OneAIError> {
        Self::connect_with_pool_size(dsn, 8).await
    }

    /// Like [`connect`](Self::connect) with an explicit pool size.
    pub async fn connect_with_pool_size(
        dsn: &str,
        max_size: usize,
    ) -> Result<Self, oneai_core::error::OneAIError> {
        let pool = build_pool(dsn, max_size)?;
        let store = Self::new(pool);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an externally built pool. Schema DDL runs lazily on first use.
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            schema_ready: tokio::sync::OnceCell::new(),
        }
    }

    /// The underlying pool (read-only introspection / ops tooling).
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Apply the schema DDL (idempotent, at most once per store instance).
    pub async fn ensure_schema(&self) -> Result<(), oneai_core::error::OneAIError> {
        common_ensure_schema(
            &self.pool,
            &self.schema_ready,
            LOCK_KEY,
            SCHEMA_EXISTS_SQL,
            SCHEMA_DDL,
        )
        .await
    }

    async fn client(&self) -> Result<deadpool_postgres::Client, oneai_core::error::OneAIError> {
        self.ensure_schema().await?;
        self.pool.get().await.map_err(pool_err)
    }

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

#[async_trait]
impl HostAllowlistStore for PgHostAllowlist {
    async fn is_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        match self.client().await {
            Ok(client) => client
                .query_opt("SELECT 1 FROM host_allowlist_pg WHERE host = $1", &[&host])
                .await
                .map(|r| r.is_some())
                .unwrap_or(false),
            Err(e) => {
                tracing::warn!("PgHostAllowlist::is_allowed failed: {e}");
                false // failing closed here *prompts* (not admits); the proxy
                      // falls through to its gate-prompt path — safe default.
            }
        }
    }

    async fn add(&self, host: String) {
        let host = host.to_ascii_lowercase();
        let at = Self::now_secs();
        match self.client().await {
            Ok(mut client) => {
                // One transaction: insert the admission AND clear any prior
                // denial, so a concurrent reader never sees both rows.
                let tx = match client.transaction().await {
                    Ok(tx) => tx,
                    Err(e) => {
                        tracing::warn!("PgHostAllowlist::add tx: {}", pg_err(e));
                        return;
                    }
                };
                let r1 = tx
                    .execute(
                        "INSERT INTO host_allowlist_pg (host, recorded_at) VALUES ($1, $2) \
                         ON CONFLICT (host) DO NOTHING",
                        &[&host, &at],
                    )
                    .await;
                let r2 = tx
                    .execute("DELETE FROM host_denylist_pg WHERE host = $1", &[&host])
                    .await;
                if let Err(e) = r1.and(r2).and(Ok(())) {
                    tracing::warn!("PgHostAllowlist::add: {}", pg_err(e));
                    return; // tx dropped → rollback
                }
                if let Err(e) = tx.commit().await {
                    tracing::warn!("PgHostAllowlist::add commit: {}", pg_err(e));
                }
            }
            Err(e) => tracing::warn!("PgHostAllowlist::add failed: {e}"),
        }
    }

    async fn is_denied(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        match self.client().await {
            Ok(client) => client
                .query_opt("SELECT 1 FROM host_denylist_pg WHERE host = $1", &[&host])
                .await
                .map(|r| r.is_some())
                .unwrap_or(false),
            Err(e) => {
                tracing::warn!("PgHostAllowlist::is_denied failed: {e}");
                false // fail open on deny read: don't block a host we couldn't
                      // look up — let the gate-prompt path decide.
            }
        }
    }

    async fn add_denied(&self, host: String) {
        let host = host.to_ascii_lowercase();
        let at = Self::now_secs();
        match self.client().await {
            Ok(mut client) => {
                let tx = match client.transaction().await {
                    Ok(tx) => tx,
                    Err(e) => {
                        tracing::warn!("PgHostAllowlist::add_denied tx: {}", pg_err(e));
                        return;
                    }
                };
                let r1 = tx
                    .execute(
                        "INSERT INTO host_denylist_pg (host, recorded_at) VALUES ($1, $2) \
                         ON CONFLICT (host) DO NOTHING",
                        &[&host, &at],
                    )
                    .await;
                // Mutually exclusive: a denied host is removed from the
                // allowlist so a stale admission can't silently re-admit it.
                let r2 = tx
                    .execute("DELETE FROM host_allowlist_pg WHERE host = $1", &[&host])
                    .await;
                if let Err(e) = r1.and(r2).and(Ok(())) {
                    tracing::warn!("PgHostAllowlist::add_denied: {}", pg_err(e));
                    return;
                }
                if let Err(e) = tx.commit().await {
                    tracing::warn!("PgHostAllowlist::add_denied commit: {}", pg_err(e));
                }
            }
            Err(e) => tracing::warn!("PgHostAllowlist::add_denied failed: {e}"),
        }
    }
}

// ─── Inherent CRUD (the `host/*` RPC surface) ───────────────────────────────
// Mirrors SqliteHostAllowlist's inherent API one-for-one so the app-server's
// HostAllowlistRpc adapter can wrap either backend.

impl PgHostAllowlist {
    /// All admitted hosts, ordered by host. Read failures return empty
    /// (errors swallowed + warned, matching the trait's fail modes).
    pub async fn list_allowed(&self) -> Vec<oneai_core::HostAllowEntry> {
        self.list_table("host_allowlist_pg").await
    }

    /// All denied hosts, ordered by host.
    pub async fn list_denied(&self) -> Vec<oneai_core::HostAllowEntry> {
        self.list_table("host_denylist_pg").await
    }

    /// Shared read for both tables. `recorded_at` is unix-seconds in the
    /// schema; ×1000 to epoch-millis on the wire (same as SQLite).
    async fn list_table(&self, table: &str) -> Vec<oneai_core::HostAllowEntry> {
        let Ok(client) = self.client().await else {
            return Vec::new();
        };
        // `table` is a static literal here (caller passes one of two
        // compile-time-known names), not user input — safe to format in.
        let sql = format!("SELECT host, recorded_at FROM {table} ORDER BY host ASC");
        match client.query(&sql, &[]).await {
            Ok(rows) => rows
                .iter()
                .map(|r| oneai_core::HostAllowEntry {
                    host: r.get(0),
                    recorded_at_ms: r.get::<_, i64>(1).max(0) as u64 * 1000,
                })
                .collect(),
            Err(e) => {
                tracing::warn!("PgHostAllowlist::list({table}): {}", pg_err(e));
                Vec::new()
            }
        }
    }

    /// Remove `host` from the allowlist (un-admit). Idempotent.
    pub async fn remove(&self, host: &str) {
        self.delete_from("host_allowlist_pg", host).await
    }

    /// Remove `host` from the denylist (un-deny). Idempotent.
    pub async fn remove_denied(&self, host: &str) {
        self.delete_from("host_denylist_pg", host).await
    }

    async fn delete_from(&self, table: &str, host: &str) {
        let host = host.to_ascii_lowercase();
        let Ok(client) = self.client().await else {
            return;
        };
        // `table` is a static literal (see list_table).
        let sql = format!("DELETE FROM {table} WHERE host = $1");
        if let Err(e) = client.execute(&sql, &[&host]).await {
            tracing::warn!("PgHostAllowlist::delete({table}): {}", pg_err(e));
        }
    }
}
