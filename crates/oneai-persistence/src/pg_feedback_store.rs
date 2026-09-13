//! Postgres-backed per-message feedback store (MVS3-C — storage
//! externalization, see `docs/cloud-orchestrator-design.md` §6/§7).
//!
//! Mirrors the `message_feedback` table of [`crate::SqliteSessionStore`] so
//! webUI 👍/👎/note reactions survive container / volume loss and are shared
//! across containers like the other `_pg` stores.
//!
//! The `FeedbackStore` **trait** lives in `oneai-app-server` (the JSON-RPC
//! layer) and this crate must not depend on it — so the store exposes the
//! inherent [`record`](PgFeedbackStore::record) / [`list`](PgFeedbackStore::list)
//! surface and the CLI wraps it in a thin adapter implementing the trait
//! (the exact pattern of `PgHostAllowlistRpc` in
//! `examples/cli/src/pg_backends.rs`).
//!
//! Error contract mirrors the SQLite path: `record` swallows failures with a
//! `tracing::warn!` (feedback is non-critical UX state — never a panic, never
//! a turn failure) and `list` returns an empty vec on error.
//!
//! ## Feature gate + selection
//! Compiled only under `--features postgres` (default off). At runtime the
//! CLI selects this backend when `ONEAI_PG_DSN` is set (see
//! `examples/cli/src/pg_backends.rs`).

use std::time::{SystemTime, UNIX_EPOCH};

use deadpool_postgres::Pool;
use oneai_core::error::Result;
use oneai_core::FeedbackEntry;

use crate::pg_common::{
    build_pool, ensure_schema as common_ensure_schema, pg_err, pool_err, ADVISORY_LOCK_BASE,
};

/// Catalog probe: TRUE when every object this store needs already exists
/// (catalog-only reads — no relation locks on the steady-state boot path).
const SCHEMA_EXISTS_SQL: &str = "SELECT to_regclass('message_feedback_pg') IS NOT NULL \
     AND to_regclass('idx_feedback_pg_session') IS NOT NULL";

/// DDL applied idempotently on first use (see `pg_common::ensure_schema`).
/// Column-for-column the SQLite `message_feedback` table (`created_at_ms`
/// as BIGINT; the SQLite impl stores the same epoch-millis integer).
const SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS message_feedback_pg (
    id           TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL,
    turn_id      TEXT NOT NULL,
    message_role TEXT NOT NULL,
    kind         TEXT NOT NULL,
    text         TEXT,
    created_at_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_feedback_pg_session ON message_feedback_pg(session_id, created_at_ms);
"#;

/// This store's advisory-lock key (see the registry in `pg_common`).
const LOCK_KEY: i64 = ADVISORY_LOCK_BASE + 5;

/// Postgres-backed feedback store. See the module docs for the contract.
pub struct PgFeedbackStore {
    pool: Pool,
    /// DDL applied at most once per store instance.
    schema_ready: tokio::sync::OnceCell<()>,
}

impl PgFeedbackStore {
    /// Connect to `dsn` (a libpq connection string) and build a pooled
    /// store. Fails fast when the server is unreachable or the DDL can't be
    /// applied — the CLI selection layer then degrades loudly to the local
    /// SQLite feedback table.
    pub async fn connect(dsn: &str) -> Result<Self> {
        Self::connect_with_pool_size(dsn, 8).await
    }

    /// Like [`connect`](Self::connect) with an explicit pool size.
    pub async fn connect_with_pool_size(dsn: &str, max_size: usize) -> Result<Self> {
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
    pub async fn ensure_schema(&self) -> Result<()> {
        common_ensure_schema(
            &self.pool,
            &self.schema_ready,
            LOCK_KEY,
            SCHEMA_EXISTS_SQL,
            SCHEMA_DDL,
        )
        .await
    }

    async fn client(&self) -> Result<deadpool_postgres::Client> {
        self.ensure_schema().await?;
        self.pool.get().await.map_err(pool_err)
    }

    /// Record one feedback entry. Assigns `id` (`fb-{uuid-simple}`, same
    /// shape as the SQLite impl) + `created_at_ms`; a backend failure is a
    /// logged no-op (mirrors `SqliteSessionStore::record_feedback`'s
    /// best-effort contract). `text` is `Some` only for `note`-kind entries.
    pub async fn record(
        &self,
        session_id: &str,
        turn_id: &str,
        message_role: &str,
        kind: &str,
        text: Option<&str>,
    ) {
        let id = format!("fb-{}", uuid::Uuid::new_v4().simple());
        let created_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let write = async {
            let client = self.client().await?;
            client
                .execute(
                    "INSERT INTO message_feedback_pg \
                     (id, session_id, turn_id, message_role, kind, text, created_at_ms) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    &[
                        &id,
                        &session_id,
                        &turn_id,
                        &message_role,
                        &kind,
                        &text,
                        &created_at_ms,
                    ],
                )
                .await
                .map_err(pg_err)?;
            Ok::<(), oneai_core::error::OneAIError>(())
        };
        if let Err(e) = write.await {
            tracing::warn!(error = %e, "Pg feedback record failed (silent no-op)");
        }
    }

    /// All feedback entries for `session_id`, oldest first (same ordering as
    /// the SQLite `list_feedback`). A backend failure returns an empty vec —
    /// never a panic.
    pub async fn list(&self, session_id: &str) -> Vec<FeedbackEntry> {
        let read = async {
            let client = self.client().await?;
            let rows = client
                .query(
                    "SELECT id, session_id, turn_id, message_role, kind, text, created_at_ms \
                     FROM message_feedback_pg WHERE session_id = $1 ORDER BY created_at_ms ASC",
                    &[&session_id],
                )
                .await
                .map_err(pg_err)?;
            Ok::<_, oneai_core::error::OneAIError>(
                rows.iter()
                    .map(|r| FeedbackEntry {
                        id: r.get(0),
                        session_id: r.get(1),
                        turn_id: r.get(2),
                        message_role: r.get(3),
                        kind: r.get(4),
                        text: r.get(5),
                        created_at_ms: r.get::<_, i64>(6) as u64,
                    })
                    .collect(),
            )
        };
        match read.await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(error = %e, "Pg feedback list failed (returning empty)");
                Vec::new()
            }
        }
    }
}
