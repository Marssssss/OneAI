//! Postgres-backed usage tracker (MVS3-B — storage externalization, see
//! `docs/cloud-orchestrator-design.md` §6/§7).
//!
//! Drop-in replacement for [`crate::SqliteUsageTracker`] behind the same
//! `UsageTracker` trait: N session containers sharing one Postgres see one
//! global usage ledger (per-session and cross-session aggregation), and the
//! record survives container loss.
//!
//! Semantics mirror the SQLite backend exactly: write = one INSERT per
//! recorded call; reads load the matching rows and aggregate in Rust via
//! `UsageSummary::from_records` (no SQL-side aggregation — keeps the two
//! backends byte-for-byte equivalent in what they report).
//!
//! Schema deltas vs SQLite:
//! - `timestamp` is `TIMESTAMPTZ` bound from `chrono::DateTime<Utc>` (the
//!   SQLite backend stores RFC3339 TEXT); the backends are mutually
//!   exclusive per engine instance — no cross-read.
//! - `is_estimated` gets a real column. The SQLite table never grew one
//!   (the flag is lost across a SQLite round-trip); Pg persists it, so
//!   `session_records`/`global_records` return faithful records.
//! - JSONB bound via the `$n::text::jsonb` double-cast, read via `::text`
//!   (same as the other Pg stores — no serde_json feature on the driver).
//!
//! ## Feature gate + selection
//! Compiled only under `--features postgres` (default off). At runtime the
//! CLI selects this backend when `ONEAI_PG_DSN` is set (see
//! `examples/cli/src/pg_backends.rs`).

use std::collections::HashMap;

use async_trait::async_trait;
use deadpool_postgres::Pool;
use oneai_core::error::{OneAIError, Result};
use oneai_core::usage::{UsageRecord, UsageSummary, UsageTracker};

use crate::pg_common::{
    build_pool, ensure_schema as common_ensure_schema, pg_err, pool_err, ADVISORY_LOCK_BASE,
};

/// Catalog probe: TRUE when every object this store needs already exists
/// (catalog-only reads — no relation locks on the steady-state boot path).
const SCHEMA_EXISTS_SQL: &str = "SELECT to_regclass('usage_records_pg') IS NOT NULL \
     AND to_regclass('idx_usage_pg_session') IS NOT NULL";

/// DDL applied idempotently on first use (see `pg_common::ensure_schema`).
const SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS usage_records_pg (
    id                    TEXT PRIMARY KEY,
    session_id            TEXT NOT NULL,
    model                 TEXT NOT NULL,
    provider              TEXT NOT NULL,
    prompt_tokens         BIGINT NOT NULL,
    completion_tokens     BIGINT NOT NULL,
    cache_read_tokens     BIGINT NOT NULL DEFAULT 0,
    cache_creation_tokens BIGINT NOT NULL DEFAULT 0,
    timestamp             TIMESTAMPTZ NOT NULL,
    is_estimated          BOOLEAN NOT NULL DEFAULT FALSE,
    metadata_json         JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_usage_pg_session ON usage_records_pg(session_id);
CREATE INDEX IF NOT EXISTS idx_usage_pg_model ON usage_records_pg(model);
CREATE INDEX IF NOT EXISTS idx_usage_pg_timestamp ON usage_records_pg(timestamp);
"#;

/// This store's advisory-lock key (see the registry in `pg_common`).
const LOCK_KEY: i64 = ADVISORY_LOCK_BASE + 2;

/// Postgres-backed usage tracker. See the module docs for the contract.
pub struct PgUsageTracker {
    pool: Pool,
    /// DDL applied at most once per store instance.
    schema_ready: tokio::sync::OnceCell<()>,
}

impl PgUsageTracker {
    /// Connect to `dsn` (a libpq connection string) and build a pooled
    /// tracker. Fails fast when the server is unreachable or the DDL can't
    /// be applied.
    pub async fn connect(dsn: &str) -> Result<Self> {
        Self::connect_with_pool_size(dsn, 8).await
    }

    /// Like [`connect`](Self::connect) with an explicit pool size.
    pub async fn connect_with_pool_size(dsn: &str, max_size: usize) -> Result<Self> {
        let pool = build_pool(dsn, max_size)?;
        let tracker = Self::new(pool);
        tracker.ensure_schema().await?;
        Ok(tracker)
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

    /// Shared row projection — every read query selects this column order.
    /// `UsageRecord` is `#[non_exhaustive]`, so it is rebuilt through the
    /// public constructors + a field assignment for `is_estimated`.
    fn decode_record(row: &deadpool_postgres::tokio_postgres::Row) -> UsageRecord {
        let metadata_json = row.get::<_, String>(8);
        let metadata: HashMap<String, String> =
            serde_json::from_str(&metadata_json).unwrap_or_default();
        let mut record = UsageRecord::with_timestamp(
            row.get::<_, String>(0),
            row.get::<_, String>(1),
            row.get::<_, String>(2),
            row.get::<_, i64>(3) as u32,
            row.get::<_, i64>(4) as u32,
            row.get::<_, chrono::DateTime<chrono::Utc>>(7),
            metadata,
        )
        .with_cache_tokens(row.get::<_, i64>(5) as u32, row.get::<_, i64>(6) as u32);
        record.is_estimated = row.get(9);
        record
    }

    /// The read projection shared by the session/global queries.
    const SELECT_COLUMNS: &str = "SELECT session_id, model, provider, prompt_tokens, \
         completion_tokens, cache_read_tokens, cache_creation_tokens, timestamp, \
         metadata_json::text, is_estimated FROM usage_records_pg";

    async fn load_records(&self, session_id: Option<&str>) -> Result<Vec<UsageRecord>> {
        let client = self.client().await?;
        let sql = match session_id {
            Some(_) => format!(
                "{} WHERE session_id = $1 ORDER BY timestamp ASC",
                Self::SELECT_COLUMNS
            ),
            None => format!("{} ORDER BY timestamp ASC", Self::SELECT_COLUMNS),
        };
        let rows = match session_id {
            Some(sid) => client.query(&sql, &[&sid]).await,
            None => client.query(&sql, &[]).await,
        }
        .map_err(|e| OneAIError::Usage(format!("Failed to query usage records: {}", pg_err(e))))?;
        Ok(rows.iter().map(Self::decode_record).collect())
    }
}

#[async_trait]
impl UsageTracker for PgUsageTracker {
    async fn record_usage(&self, record: UsageRecord) -> Result<()> {
        let client = self.client().await?;
        let id = uuid::Uuid::new_v4().to_string();
        let metadata_json =
            serde_json::to_string(&record.metadata).unwrap_or_else(|_| "{}".to_string());
        client
            .execute(
                "INSERT INTO usage_records_pg \
                 (id, session_id, model, provider, prompt_tokens, completion_tokens, \
                  cache_read_tokens, cache_creation_tokens, timestamp, is_estimated, \
                  metadata_json) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::text::jsonb)",
                &[
                    &id,
                    &record.session_id,
                    &record.model,
                    &record.provider,
                    &(record.prompt_tokens as i64),
                    &(record.completion_tokens as i64),
                    &(record.cache_read_tokens as i64),
                    &(record.cache_creation_tokens as i64),
                    &record.timestamp,
                    &record.is_estimated,
                    &metadata_json,
                ],
            )
            .await
            .map_err(|e| {
                OneAIError::Usage(format!("Failed to insert usage record: {}", pg_err(e)))
            })?;
        Ok(())
    }

    async fn session_usage(&self, session_id: &str) -> Result<UsageSummary> {
        let records = self.load_records(Some(session_id)).await?;
        Ok(UsageSummary::from_records(&records))
    }

    async fn global_usage(&self) -> Result<UsageSummary> {
        let records = self.load_records(None).await?;
        Ok(UsageSummary::from_records(&records))
    }

    async fn usage_by_model(&self, session_id: &str) -> Result<HashMap<String, UsageSummary>> {
        let records = self.load_records(Some(session_id)).await?;
        Ok(group_by_model(records))
    }

    async fn usage_by_model_global(&self) -> Result<HashMap<String, UsageSummary>> {
        let records = self.load_records(None).await?;
        Ok(group_by_model(records))
    }

    async fn session_records(&self, session_id: &str) -> Result<Vec<UsageRecord>> {
        self.load_records(Some(session_id)).await
    }

    async fn global_records(&self) -> Result<Vec<UsageRecord>> {
        self.load_records(None).await
    }

    async fn clear_session(&self, session_id: &str) -> Result<()> {
        let client = self.client().await?;
        client
            .execute(
                "DELETE FROM usage_records_pg WHERE session_id = $1",
                &[&session_id],
            )
            .await
            .map_err(|e| {
                OneAIError::Usage(format!("Failed to clear session usage data: {}", pg_err(e)))
            })?;
        Ok(())
    }

    async fn clear_all(&self) -> Result<()> {
        let client = self.client().await?;
        client
            .execute("DELETE FROM usage_records_pg", &[])
            .await
            .map_err(|e| {
                OneAIError::Usage(format!("Failed to clear all usage data: {}", pg_err(e)))
            })?;
        Ok(())
    }
}

/// Per-model breakdown — same in-Rust grouping as the SQLite backend.
fn group_by_model(records: Vec<UsageRecord>) -> HashMap<String, UsageSummary> {
    let mut by_model: HashMap<String, Vec<UsageRecord>> = HashMap::new();
    for record in records {
        by_model
            .entry(record.model.clone())
            .or_default()
            .push(record);
    }
    by_model
        .into_iter()
        .map(|(model, records)| (model, UsageSummary::from_records(&records)))
        .collect()
}
