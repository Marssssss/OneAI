//! Postgres-backed session event store (MVS3-C — storage externalization,
//! see `docs/cloud-orchestrator-design.md` §6/§7).
//!
//! Drop-in replacement for [`crate::FileSessionEventStore`] behind the core
//! `SessionEventStore` trait: the per-session trajectory event log (issue #40
//! replay — `session/trajectory` RPC + web 泳道时间轴) survives container /
//! volume loss and is queryable cross-container from the ops side.
//!
//! Semantics mirror the file backend:
//! - `line` is an **opaque string** at this layer (the trait contract — the
//!   producer serializes `EngineYield`s, the consumer parses). The column is
//!   TEXT, not JSONB, so a round-trip is byte-exact (JSONB would normalize
//!   key order/whitespace and reject non-JSON lines the file backend happily
//!   stores). Ops-side JSON querying can add a generated `::jsonb` column +
//!   GIN index later without a migration of the payload itself.
//! - Append order is authoritative via `BIGSERIAL id` (the file backend's
//!   order is append order; concurrent appenders to one session get a
//!   deterministic, insertion-ordered replay).
//! - Absent session ⇒ empty vec (same as a missing JSONL file). The file
//!   backend's "corrupt trailing line skipped" policy has no Pg analogue —
//!   transactions make half-written lines impossible.
//!
//! ## Feature gate + selection
//! Compiled only under `--features postgres` (default off). At runtime the
//! CLI selects this backend when `ONEAI_PG_DSN` is set (see
//! `examples/cli/src/pg_backends.rs`).

use async_trait::async_trait;
use deadpool_postgres::Pool;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::SessionEventStore;

use crate::pg_common::{
    build_pool, ensure_schema as common_ensure_schema, pg_err, pool_err, ADVISORY_LOCK_BASE,
};

/// Catalog probe: TRUE when every object this store needs already exists
/// (catalog-only reads — no relation locks on the steady-state boot path).
const SCHEMA_EXISTS_SQL: &str = "SELECT to_regclass('session_events_pg') IS NOT NULL \
     AND to_regclass('idx_session_events_pg_session') IS NOT NULL";

/// DDL applied idempotently on first use (see `pg_common::ensure_schema`).
const SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS session_events_pg (
    id         BIGSERIAL PRIMARY KEY,
    session_id TEXT NOT NULL,
    line       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_session_events_pg_session ON session_events_pg(session_id, id);
"#;

/// This store's advisory-lock key (see the registry in `pg_common`).
const LOCK_KEY: i64 = ADVISORY_LOCK_BASE + 4;

/// Postgres-backed session event store. See the module docs for the contract.
pub struct PgSessionEventStore {
    pool: Pool,
    /// DDL applied at most once per store instance.
    schema_ready: tokio::sync::OnceCell<()>,
}

impl PgSessionEventStore {
    /// Connect to `dsn` (a libpq connection string) and build a pooled
    /// store. Fails fast when the server is unreachable or the DDL can't be
    /// applied (the CLI selection layer then degrades loudly to the file
    /// backend — a trajectory log that silently never persists is the worst
    /// failure mode).
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
}

#[async_trait]
impl SessionEventStore for PgSessionEventStore {
    async fn append(&self, session_id: &str, line: &str) -> Result<()> {
        let client = self.client().await?;
        client
            .execute(
                "INSERT INTO session_events_pg (session_id, line) VALUES ($1, $2)",
                &[&session_id, &line],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to append session event: {}", pg_err(e)))
            })?;
        Ok(())
    }

    async fn load(&self, session_id: &str) -> Result<Vec<String>> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT line FROM session_events_pg WHERE session_id = $1 ORDER BY id ASC",
                &[&session_id],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to load session events: {}", pg_err(e)))
            })?;
        Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
    }
}
