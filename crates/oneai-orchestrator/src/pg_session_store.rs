//! Postgres-backed session store (MVS4-A — multi-replica orchestrator).
//!
//! Drop-in replacement for [`crate::FileSessionStore`] behind the
//! [`SessionStore`](crate::store::SessionStore) trait: one row per session in
//! a shared Postgres, so N orchestrator replicas converge on one routing
//! table. Adds the two things a local file cannot give multiple writers:
//!
//! - **Cross-replica CAS**: state transitions / archive claims are
//!   conditional `UPDATE … WHERE state=… RETURNING` statements — the row
//!   lock arbitrates, exactly one concurrent writer wins (same contract as
//!   the MVS2 in-memory CAS, widened across processes).
//! - **Per-session leases**: `owner_replica` + `lease_expires_at` columns.
//!   Claim/renew/takeover are single-statement CAS updates against server
//!   time (`now() + make_interval(…)`) so replica clock skew is irrelevant.
//!
//! Conventions follow the six `oneai-persistence` Pg stores (MVS3): pooled
//! `deadpool-postgres` via `pg_common::build_pool` (NoTls — same-host/VPC
//! Postgres), advisory-lock-guarded idempotent DDL via `pg_common::
//! ensure_schema` (this store's key: `ADVISORY_LOCK_BASE + 6`), fail-fast
//! connect, JSONB params bound through `$n::text::jsonb` double casts and
//! read back via `::text` (the driver sees plain text — no serde_json
//! feature dependency).
//!
//! ## Feature gate + selection
//! Compiled only under `--features postgres` (default off). At runtime the
//! CLI selects this backend when `ONEAI_PG_DSN` is set (see
//! `examples/cli/src/cmd_orchestrator.rs`), mirroring `pg_backends.rs`.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::tokio_postgres::Row;
use deadpool_postgres::Pool;
use oneai_persistence::pg_common::{
    build_pool, ensure_schema as common_ensure_schema, pg_err, pool_err, ADVISORY_LOCK_BASE,
};

use crate::archive::ArchiveManifest;
use crate::error::{OrchestratorError, Result};
use crate::fsm::{validate_transition, PersistedEntry, SessionState};
use crate::runner::{ContainerHandle, SessionSpec};
use crate::store::{ClaimOutcome, LeaseInfo, SessionStore, StoredSession};

/// This store's advisory-lock key (see the registry in `pg_common`).
const LOCK_KEY: i64 = ADVISORY_LOCK_BASE + 6;

/// Catalog probe: TRUE when every object this store needs already exists.
const SCHEMA_EXISTS_SQL: &str = "SELECT to_regclass('orchestrator_sessions') IS NOT NULL \
     AND to_regclass('idx_orch_sess_lease') IS NOT NULL";

/// DDL applied idempotently on first use (see `pg_common::ensure_schema`).
const SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS orchestrator_sessions (
    session_id       TEXT PRIMARY KEY,
    spec             JSONB NOT NULL,
    handle           JSONB,
    state            TEXT NOT NULL,
    last_error       TEXT,
    updated_at       TIMESTAMPTZ NOT NULL,
    archived         JSONB,
    owner_replica    TEXT,
    lease_expires_at TIMESTAMPTZ,
    last_activity_ms BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_orch_sess_lease
    ON orchestrator_sessions (lease_expires_at)
    WHERE owner_replica IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_orch_sess_state_owner
    ON orchestrator_sessions (state, owner_replica);
"#;

/// Every column, in one place (JSONB read back as text — see module docs).
const COLS: &str = "session_id, spec::text, handle::text, state, last_error, updated_at, \
     archived::text, owner_replica, lease_expires_at, last_activity_ms";

/// Shared-Postgres session store. See the module docs for the contract.
pub struct PgSessionStore {
    pool: Pool,
    /// DDL applied at most once per store instance.
    schema_ready: tokio::sync::OnceCell<()>,
}

impl PgSessionStore {
    /// Connect to `dsn` (a libpq connection string) and build a pooled
    /// store. Fails fast when the server is unreachable or the DDL can't be
    /// applied (the CLI selection layer then degrades loudly to the file
    /// backend — a routing table that silently diverges across replicas is
    /// the worst failure mode).
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
        .await?;
        Ok(())
    }

    async fn client(&self) -> Result<deadpool_postgres::Client> {
        self.ensure_schema().await?;
        self.pool
            .get()
            .await
            .map_err(|e| OrchestratorError::from(pool_err(e)))
    }
}

// ─── Row mapping ──────────────────────────────────────────────────────────────

fn state_to_sql(state: SessionState) -> String {
    // SessionState serializes as a plain variant-name string ("Running").
    serde_json::to_string(&state)
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|_| format!("{state:?}"))
}

fn state_from_sql(s: &str) -> Result<SessionState> {
    serde_json::from_value::<SessionState>(serde_json::Value::String(s.to_string()))
        .map_err(|e| OrchestratorError::Pg(format!("unreadable state '{s}': {e}")))
}

fn json_opt<T: serde::de::DeserializeOwned>(raw: Option<String>, what: &str) -> Result<Option<T>> {
    match raw {
        None => Ok(None),
        Some(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| OrchestratorError::Pg(format!("unreadable {what}: {e}"))),
    }
}

fn to_json<T: serde::Serialize>(v: &T, what: &str) -> Result<String> {
    serde_json::to_string(v).map_err(|e| OrchestratorError::Pg(format!("bad {what}: {e}")))
}

fn row_to_stored(row: &Row) -> Result<StoredSession> {
    let spec_json: String = row.get(1);
    let spec: SessionSpec = serde_json::from_str(&spec_json)
        .map_err(|e| OrchestratorError::Pg(format!("unreadable spec: {e}")))?;
    let handle: Option<ContainerHandle> = json_opt(row.get(2), "handle")?;
    let state: String = row.get(3);
    let last_error: Option<String> = row.get(4);
    let updated_at: DateTime<Utc> = row.get(5);
    let archived: Option<ArchiveManifest> = json_opt(row.get(6), "archived manifest")?;
    let owner_replica: Option<String> = row.get(7);
    let lease_expires_at: Option<DateTime<Utc>> = row.get(8);
    let last_activity_ms: i64 = row.get(9);
    Ok(StoredSession {
        entry: PersistedEntry {
            spec,
            handle,
            state: state_from_sql(&state)?,
            last_error,
            updated_at,
            archived,
        },
        lease: owner_replica.zip(lease_expires_at).map(|(o, e)| LeaseInfo {
            owner_replica: o,
            expires_at: e,
        }),
        last_activity_ms: last_activity_ms.max(0) as u64,
    })
}

/// SQL-bound projections of a `PersistedEntry` (JSONB fields pre-serialized).
struct EntryParams {
    id: String,
    spec: String,
    handle: Option<String>,
    state: String,
    last_error: Option<String>,
    updated_at: DateTime<Utc>,
    archived: Option<String>,
}

fn entry_params(e: &PersistedEntry) -> Result<EntryParams> {
    Ok(EntryParams {
        id: e.spec.session_id.clone(),
        spec: to_json(&e.spec, "spec")?,
        handle: e
            .handle
            .as_ref()
            .map(|h| to_json(h, "handle"))
            .transpose()?,
        state: state_to_sql(e.state),
        last_error: e.last_error.clone(),
        updated_at: e.updated_at,
        archived: e
            .archived
            .as_ref()
            .map(|a| to_json(a, "archive manifest"))
            .transpose()?,
    })
}

// ─── SessionStore impl ────────────────────────────────────────────────────────

#[async_trait]
impl SessionStore for PgSessionStore {
    async fn load_all(&self) -> Result<Vec<StoredSession>> {
        let c = self.client().await?;
        let rows = c
            .query(
                &format!("SELECT {COLS} FROM orchestrator_sessions ORDER BY session_id"),
                &[],
            )
            .await
            .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        rows.iter().map(row_to_stored).collect()
    }

    async fn get(&self, id: &str) -> Result<Option<StoredSession>> {
        let c = self.client().await?;
        let row = c
            .query_opt(
                &format!("SELECT {COLS} FROM orchestrator_sessions WHERE session_id = $1"),
                &[&id],
            )
            .await
            .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        row.as_ref().map(row_to_stored).transpose()
    }

    async fn insert(&self, entry: &PersistedEntry) -> Result<()> {
        let p = entry_params(entry)?;
        let c = self.client().await?;
        // `::text::jsonb` (not `::jsonb`): a bare cast makes Postgres
        // resolve the param as unknown/jsonb and the driver refuses; the
        // double cast pins it to text (MVS3-A pitfall, see pg_common docs).
        let n = c
            .execute(
                "INSERT INTO orchestrator_sessions \
                 (session_id, spec, handle, state, last_error, updated_at, archived) \
                 VALUES ($1, $2::text::jsonb, $3::text::jsonb, $4, $5, $6, $7::text::jsonb) \
                 ON CONFLICT (session_id) DO NOTHING",
                &[
                    &p.id,
                    &p.spec,
                    &p.handle,
                    &p.state,
                    &p.last_error,
                    &p.updated_at,
                    &p.archived,
                ],
            )
            .await
            .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        if n == 0 {
            return Err(OrchestratorError::AlreadyExists(p.id));
        }
        Ok(())
    }

    async fn cas_transition(
        &self,
        id: &str,
        expected: SessionState,
        to: SessionState,
        new_handle: Option<ContainerHandle>,
        last_error: Option<String>,
    ) -> Result<Option<PersistedEntry>> {
        // Contract order (mirrors the file store): unknown id → None; state
        // mismatch → None; illegal transition → Err. The SELECT pre-check
        // distinguishes the cases; the UPDATE's WHERE re-arms the CAS so a
        // race between the two degrades to a clean miss.
        let c = self.client().await?;
        let cur = c
            .query_opt(
                "SELECT state FROM orchestrator_sessions WHERE session_id = $1",
                &[&id],
            )
            .await
            .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        let Some(cur) = cur else {
            return Ok(None);
        };
        let cur_state: String = cur.get(0);
        if cur_state != state_to_sql(expected) {
            return Ok(None);
        }
        validate_transition(expected, to)?;

        let handle_json = new_handle
            .as_ref()
            .map(|h| to_json(h, "handle"))
            .transpose()?;
        let row = c
            .query_opt(
                &format!(
                    "UPDATE orchestrator_sessions SET \
                       state = $2, last_error = $3, \
                       handle = CASE WHEN $4::boolean THEN $5::text::jsonb ELSE handle END, \
                       updated_at = now() \
                     WHERE session_id = $1 AND state = $6 \
                     RETURNING {COLS}"
                ),
                &[
                    &id,
                    &state_to_sql(to),
                    &last_error,
                    &new_handle.is_some(),
                    &handle_json,
                    &state_to_sql(expected),
                ],
            )
            .await
            .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(row
            .map(|r| row_to_stored(&r).map(|s| s.entry))
            .transpose()?)
    }

    async fn cas_set_archived(
        &self,
        id: &str,
        manifest: Option<ArchiveManifest>,
    ) -> Result<Option<PersistedEntry>> {
        let c = self.client().await?;
        let row = match manifest {
            // Claim: only from Hibernating with no marker; clears the stale
            // handle (container+volumes die right after) and bumps updated_at.
            Some(m) => {
                let m_json = to_json(&m, "archive manifest")?;
                c.query_opt(
                    &format!(
                        "UPDATE orchestrator_sessions SET \
                           archived = $2::text::jsonb, handle = NULL, updated_at = now() \
                         WHERE session_id = $1 AND state = 'Hibernating' AND archived IS NULL \
                         RETURNING {COLS}"
                    ),
                    &[&id, &m_json],
                )
                .await
            }
            // Release (post successful restore+resume): only when a marker
            // is present; nothing else moves (mirrors the file store).
            None => {
                c.query_opt(
                    &format!(
                        "UPDATE orchestrator_sessions SET archived = NULL \
                         WHERE session_id = $1 AND archived IS NOT NULL \
                         RETURNING {COLS}"
                    ),
                    &[&id],
                )
                .await
            }
        }
        .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(row
            .map(|r| row_to_stored(&r).map(|s| s.entry))
            .transpose()?)
    }

    async fn force_update(&self, entry: &PersistedEntry) -> Result<()> {
        let p = entry_params(entry)?;
        let c = self.client().await?;
        c.execute(
            "UPDATE orchestrator_sessions SET \
               spec = $2::text::jsonb, handle = $3::text::jsonb, state = $4, \
               last_error = $5, updated_at = $6, archived = $7::text::jsonb \
             WHERE session_id = $1",
            &[
                &p.id,
                &p.spec,
                &p.handle,
                &p.state,
                &p.last_error,
                &p.updated_at,
                &p.archived,
            ],
        )
        .await
        .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(())
    }

    async fn set_last_error(&self, id: &str, msg: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "UPDATE orchestrator_sessions SET last_error = $2 WHERE session_id = $1",
            &[&id, &msg],
        )
        .await
        .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(())
    }

    async fn touch_activity(&self, id: &str, activity_ms: u64) -> Result<()> {
        let c = self.client().await?;
        // GREATEST keeps the durable clock monotonic across replicas.
        c.execute(
            "UPDATE orchestrator_sessions \
             SET last_activity_ms = GREATEST(last_activity_ms, $2) \
             WHERE session_id = $1",
            &[&id, &(activity_ms.min(i64::MAX as u64) as i64)],
        )
        .await
        .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "DELETE FROM orchestrator_sessions WHERE session_id = $1",
            &[&id],
        )
        .await
        .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        Ok(()) // every mutation is already durable (row-oriented backend)
    }

    fn store_path(&self) -> Option<&Path> {
        None
    }

    fn supports_leasing(&self) -> bool {
        true
    }

    async fn try_claim_lease(
        &self,
        id: &str,
        replica_id: &str,
        ttl: Duration,
    ) -> Result<ClaimOutcome> {
        let c = self.client().await?;
        let ttl_secs = ttl.as_secs_f64();
        // Single-statement CAS: claim succeeds when unowned, expired, or
        // already ours (renewal). Server-side now() — replica clock skew is
        // irrelevant. Row lock arbitrates concurrent claimers.
        let row = c
            .query_opt(
                "UPDATE orchestrator_sessions SET \
                   owner_replica = $2, lease_expires_at = now() + make_interval(secs => $3) \
                 WHERE session_id = $1 \
                   AND (owner_replica IS NULL OR owner_replica = $2 \
                        OR lease_expires_at IS NULL OR lease_expires_at < now()) \
                 RETURNING lease_expires_at",
                &[&id, &replica_id, &ttl_secs],
            )
            .await
            .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        if let Some(row) = row {
            return Ok(ClaimOutcome::Owned {
                lease_expires_at: row.get(0),
            });
        }
        // Miss: distinguish gone vs held.
        let held = c
            .query_opt(
                "SELECT owner_replica, lease_expires_at FROM orchestrator_sessions \
                 WHERE session_id = $1",
                &[&id],
            )
            .await
            .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        match held {
            None => Ok(ClaimOutcome::NotFound),
            Some(row) => Ok(ClaimOutcome::HeldByOther {
                owner_replica: row.get::<_, Option<String>>(0).unwrap_or_default(),
                lease_expires_at: row
                    .get::<_, Option<DateTime<Utc>>>(1)
                    .unwrap_or_else(Utc::now),
            }),
        }
    }

    async fn renew_lease(
        &self,
        id: &str,
        replica_id: &str,
        ttl: Duration,
    ) -> Result<Option<DateTime<Utc>>> {
        let c = self.client().await?;
        let ttl_secs = ttl.as_secs_f64();
        let row = c
            .query_opt(
                "UPDATE orchestrator_sessions \
                 SET lease_expires_at = now() + make_interval(secs => $3) \
                 WHERE session_id = $1 AND owner_replica = $2 \
                 RETURNING lease_expires_at",
                &[&id, &replica_id, &ttl_secs],
            )
            .await
            .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(row.map(|r| r.get(0)))
    }

    async fn release_lease(&self, id: &str, replica_id: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "UPDATE orchestrator_sessions \
             SET owner_replica = NULL, lease_expires_at = NULL \
             WHERE session_id = $1 AND owner_replica = $2",
            &[&id, &replica_id],
        )
        .await
        .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(())
    }

    async fn release_all_leases(&self, replica_id: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "UPDATE orchestrator_sessions \
             SET owner_replica = NULL, lease_expires_at = NULL \
             WHERE owner_replica = $1",
            &[&replica_id],
        )
        .await
        .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(())
    }

    async fn list_owned_ids(&self, replica_id: &str) -> Result<Vec<String>> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT session_id FROM orchestrator_sessions \
                 WHERE owner_replica = $1 AND lease_expires_at > now() \
                 ORDER BY session_id",
                &[&replica_id],
            )
            .await
            .map_err(|e| OrchestratorError::from(pg_err(e)))?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }
}
