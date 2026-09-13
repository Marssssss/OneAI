//! Postgres-backed working-state store (MVS3 — storage externalization, see
//! `docs/cloud-orchestrator-design.md` §6/§7 and `docs/working-state-mechanism.md`).
//!
//! Drop-in replacement for [`crate::FileWorkingStateStore`] behind the same
//! `WorkingStateStore` trait, for cloud deployments where N session containers
//! share one Postgres instead of N per-session Docker volumes:
//!
//! - **Transactional index** — the file backend's `tasks.index.json`
//!   read-modify-write is not multi-writer safe; here the brief row is
//!   upserted in the SAME transaction as the event INSERT.
//! - **Crash recovery lifeline** — the append-only event log survives
//!   container loss entirely (a fresh container with an empty volume still
//!   rehydrates unfinished tasks from Pg).
//! - **Cross-container queries** — ops can SQL over `working_state_briefs`
//!   directly.
//!
//! ## Contract adherence (docs/working-state-mechanism.md)
//! - Events are INSERT-only; the sole "rewrite" is compaction, done as
//!   DELETE-all + INSERT snapshot+tail inside one transaction (logically
//!   equivalent, idempotent — mirrors the file backend's full-log rewrite).
//! - A `Snapshot` is just another event row — there is NO parallel
//!   current-state table that could drift.
//! - `working_state_briefs` mirrors `TaskBrief` (a *derived* artifact) and is
//!   always updated in the same transaction as the event append.
//! - Ordering: explicit `seq BIGSERIAL` column.
//! - Lossless round-trip: the whole `TaskEvent` is stored as JSONB (bound via
//!   `$n::text::jsonb` double casts so the driver sees a plain text param),
//!   so `schema_version` and any payload survive verbatim — no per-field
//!   column mapping to drift.
//!
//! ## Deliberate deviation from the file backend
//! `archive_task` does NOT gzip-and-remove the log: events stay queryable for
//! audit; the task is simply marked `archived` in the brief index so
//! `list_open_tasks` excludes it (the mechanism doc explicitly allows DB
//! backends to archive by status flag). `TaskBrief.file` is always `""`.
//!
//! ## Feature gate + selection
//! Compiled only under `--features postgres` (default off). At runtime the
//! CLI selects this backend when `ONEAI_PG_DSN` is set (see
//! `examples/cli/src/cmd_app_server.rs`).

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use deadpool_postgres::Pool;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::WorkingStateStore;
use oneai_core::{
    TaskBrief, TaskEvent, TaskEventPayload, TaskEventType, TaskStatus, WorkingState,
    TASK_EVENT_SCHEMA_VERSION,
};

use crate::pg_common::{
    build_pool, ensure_schema as common_ensure_schema, now_rfc3339, pg_err, pool_err,
    ADVISORY_LOCK_BASE,
};
use crate::working_state_store::project;

/// Catalog probe: TRUE when every object this store needs already exists.
/// Checked BEFORE any DDL so the steady-state boot path takes no relation
/// locks at all — `CREATE INDEX IF NOT EXISTS` on an existing index still
/// grabs a table-level ShareLock, which deadlocks against concurrent DML
/// from other containers (observed: E40P01 with N engines booting against
/// one shared DB). Catalog-only reads (pg_class AccessShareLock) are safe.
const SCHEMA_EXISTS_SQL: &str = "SELECT to_regclass('working_state_events') IS NOT NULL \
     AND to_regclass('working_state_briefs') IS NOT NULL \
     AND to_regclass('idx_wse_task') IS NOT NULL \
     AND to_regclass('idx_wsb_open') IS NOT NULL";

/// DDL applied idempotently on first use (`CREATE ... IF NOT EXISTS` — no
/// migration framework; additive changes go through new statements).
const SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS working_state_events (
    seq     BIGSERIAL PRIMARY KEY,
    id      TEXT NOT NULL UNIQUE,
    task_id TEXT NOT NULL,
    event   JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_wse_task ON working_state_events(task_id, seq);
CREATE TABLE IF NOT EXISTS working_state_briefs (
    task_id            TEXT PRIMARY KEY,
    goal               TEXT NOT NULL DEFAULT '',
    status             TEXT NOT NULL DEFAULT 'active',
    open_step_count    INTEGER NOT NULL DEFAULT 0,
    open_blocker_count INTEGER NOT NULL DEFAULT 0,
    user_id            TEXT NOT NULL DEFAULT '',
    project            TEXT NOT NULL DEFAULT '',
    last_event_ts      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_wsb_open ON working_state_briefs(status, user_id, project);
"#;

/// Postgres-backed working-state store. See the module docs for the contract.
pub struct PgWorkingStateStore {
    pool: Pool,
    /// DDL applied at most once per store instance (cheap guard; the DDL
    /// itself is idempotent so concurrent stores across processes are safe).
    schema_ready: tokio::sync::OnceCell<()>,
    /// Compaction thresholds — same defaults + hot-swap semantics as
    /// `FileWorkingStateStore` (DomainPack hot-switch calls `set_compaction`).
    event_threshold: AtomicUsize,
    keep_recent: AtomicUsize,
}

impl PgWorkingStateStore {
    /// Connect to `dsn` (a libpq connection string, e.g.
    /// `postgres://user:pass@host:5432/dbname`) and build a pooled store.
    ///
    /// TLS is not negotiated (`NoTls`) — target a same-host / same-VPC
    /// Postgres, or terminate TLS in front of it (pgbouncer / cloud proxy).
    pub async fn connect(dsn: &str) -> Result<Self> {
        Self::connect_with_pool_size(dsn, 8).await
    }

    /// Like [`connect`](Self::connect) with an explicit pool size.
    pub async fn connect_with_pool_size(dsn: &str, max_size: usize) -> Result<Self> {
        let pool = build_pool(dsn, max_size)?;
        let store = Self::new(pool);
        // Fail fast: apply the schema DDL NOW so "backend selected" means the
        // tables exist (ops can query them immediately; an unreachable Pg or
        // missing DDL permission surfaces at startup with a loud warning
        // instead of mid-session on the first append — the event log is the
        // crash-recovery lifeline, silent late failure is the worst mode).
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an externally built pool (tests / embedding apps that own the
    /// pool lifetime). Schema DDL runs lazily on first operation.
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            schema_ready: tokio::sync::OnceCell::new(),
            event_threshold: AtomicUsize::new(200),
            keep_recent: AtomicUsize::new(50),
        }
    }

    /// Override compaction thresholds (builder-style).
    pub fn with_compaction(self, event_threshold: usize, keep_recent: usize) -> Self {
        self.set_compaction(event_threshold, keep_recent);
        self
    }

    /// Hot-swap the compaction thresholds (DomainPack hot-switch). Reads are
    /// `Relaxed` — a transient mixed threshold during a switch is harmless.
    pub fn set_compaction(&self, event_threshold: usize, keep_recent: usize) {
        self.event_threshold
            .store(event_threshold, Ordering::Relaxed);
        self.keep_recent.store(keep_recent, Ordering::Relaxed);
    }

    /// The underlying pool (read-only introspection / ops tooling).
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Apply the schema DDL (idempotent, at most once per store instance;
    /// `connect*` calls it eagerly, `new(pool)` users can call it manually —
    /// otherwise it runs lazily before the first operation).
    pub async fn ensure_schema(&self) -> Result<()> {
        // Catalog-probe → advisory-lock → re-check → DDL; shared with the
        // other Pg stores (see `pg_common`). This store owns the base lock
        // key ("ONEAI" = 0x4F4E4149).
        common_ensure_schema(
            &self.pool,
            &self.schema_ready,
            ADVISORY_LOCK_BASE,
            SCHEMA_EXISTS_SQL,
            SCHEMA_DDL,
        )
        .await
    }

    async fn client(&self) -> Result<deadpool_postgres::Client> {
        // Lazy path for `new(pool)` users — a no-op once ensure_schema ran.
        self.ensure_schema().await?;
        self.pool.get().await.map_err(pool_err)
    }

    /// Read every event of a task in insertion order. Mirrors
    /// `FileWorkingStateStore::read_events` (public for CLI exporters).
    pub async fn read_events(&self, task_id: &str) -> Result<Vec<TaskEvent>> {
        let client = self.client().await?;
        read_events_on(&client, task_id).await
    }
}

// ─── Helpers shared by inherent + trait methods ─────────────────────────────
// (pool construction, ensure_schema flow, pool_err/pg_err/now_rfc3339 live in
// `crate::pg_common` — shared with the MVS3-B Pg stores.)

/// TaskStatus ↔ its serde (snake_case) string form — round-trips through
/// serde so a future variant rename can't desync the SQL representation.
fn status_to_str(status: &TaskStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "active".to_string())
}

fn status_from_str(s: &str) -> TaskStatus {
    serde_json::from_value::<TaskStatus>(serde_json::Value::String(s.to_string()))
        .unwrap_or(TaskStatus::Active)
}

fn encode_event(ev: &TaskEvent) -> Result<String> {
    serde_json::to_string(ev)
        .map_err(|e| OneAIError::Serialization(format!("Failed to encode event: {}", e)))
}

fn open_step_count(state: &WorkingState) -> u32 {
    state
        .steps
        .iter()
        .filter(|s| !matches!(s.status, oneai_core::StepStatus::Completed))
        .count() as u32
}

fn open_blocker_count(state: &WorkingState) -> u32 {
    state
        .blockers
        .iter()
        .filter(|b| matches!(b.status, oneai_core::BlockerStatus::Open))
        .count() as u32
}

/// SELECT + decode a task's events in seq order (`C: ToSql`-agnostic over
/// Client/Transaction via the `deadpool_postgres::GenericClient` trait).
async fn read_events_on<C>(client: &C, task_id: &str) -> Result<Vec<TaskEvent>>
where
    C: deadpool_postgres::GenericClient + Sync,
{
    let rows = client
        .query(
            "SELECT event::text FROM working_state_events WHERE task_id = $1 ORDER BY seq",
            &[&task_id],
        )
        .await
        .map_err(pg_err)?;
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let json: String = row.get(0);
        match serde_json::from_str::<TaskEvent>(&json) {
            Ok(ev) => events.push(ev),
            // Un-deserializable row: skip like the file backend skips a
            // malformed JSONL line (forward-compat with newer schema_version
            // writers must not brick older readers).
            Err(e) => {
                tracing::warn!("Skipping unreadable working-state event row: {}", e);
            }
        }
    }
    Ok(events)
}

/// Take the per-task serialization lock: ensure a brief row exists, then
/// `SELECT ... FOR UPDATE` it. Concurrent `append_event`/`compact_if_needed`
/// on the SAME task then serialize, so the brief re-derivation below always
/// sees every committed event — without the lock, two interleaved txs under
/// READ COMMITTED could each miss the other's INSERT and leave a stale brief
/// (exactly the multi-writer hazard the file backend's index RMW has).
/// Different tasks lock different rows → fully parallel.
async fn lock_task_on<C>(client: &C, task_id: &str) -> Result<()>
where
    C: deadpool_postgres::GenericClient + Sync,
{
    client
        .execute(
            "INSERT INTO working_state_briefs (task_id) VALUES ($1) ON CONFLICT DO NOTHING",
            &[&task_id],
        )
        .await
        .map_err(pg_err)?;
    client
        .query_opt(
            "SELECT task_id FROM working_state_briefs WHERE task_id = $1 FOR UPDATE",
            &[&task_id],
        )
        .await
        .map_err(pg_err)?;
    Ok(())
}

/// Upsert the derived brief for a task. Mirrors the file backend's
/// `append_event` index update exactly:
/// - missing brief → created with empty goal/user/project (defaults),
/// - `goal` only filled when currently empty,
/// - status / counts re-derived from the projected state,
/// - `last_event_ts` always advanced.
async fn upsert_brief_on<C>(
    client: &C,
    task_id: &str,
    state: Option<&WorkingState>,
    last_event_ts: &str,
) -> Result<()>
where
    C: deadpool_postgres::GenericClient + Sync,
{
    let (goal, status, steps, blockers) = match state {
        Some(s) => (
            s.goal.clone(),
            status_to_str(&s.status),
            open_step_count(s) as i32,
            open_blocker_count(s) as i32,
        ),
        None => (String::new(), "active".to_string(), 0, 0),
    };
    client
        .execute(
            "INSERT INTO working_state_briefs
                 (task_id, goal, status, open_step_count, open_blocker_count, last_event_ts)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (task_id) DO UPDATE SET
                 goal = CASE WHEN working_state_briefs.goal = ''
                             THEN EXCLUDED.goal
                             ELSE working_state_briefs.goal END,
                 status = EXCLUDED.status,
                 open_step_count = EXCLUDED.open_step_count,
                 open_blocker_count = EXCLUDED.open_blocker_count,
                 last_event_ts = EXCLUDED.last_event_ts",
            &[&task_id, &goal, &status, &steps, &blockers, &last_event_ts],
        )
        .await
        .map_err(pg_err)?;
    Ok(())
}

async fn fetch_brief<C>(client: &C, task_id: &str) -> Result<Option<TaskBrief>>
where
    C: deadpool_postgres::GenericClient + Sync,
{
    let row = client
        .query_opt(
            "SELECT goal, status, open_step_count, open_blocker_count, user_id, project, last_event_ts
             FROM working_state_briefs WHERE task_id = $1",
            &[&task_id],
        )
        .await
        .map_err(pg_err)?;
    Ok(row.map(|r| TaskBrief {
        task_id: task_id.to_string(),
        goal: r.get(0),
        status: status_from_str(&r.get::<_, String>(1)),
        open_step_count: r.get::<_, i32>(2) as u32,
        open_blocker_count: r.get::<_, i32>(3) as u32,
        user_id: r.get(4),
        project: r.get(5),
        last_event_ts: r.get(6),
        file: String::new(), // Pg backend: no per-task file (see module docs).
    }))
}

#[async_trait]
impl WorkingStateStore for PgWorkingStateStore {
    async fn create_task(
        &self,
        user_id: &str,
        project: &str,
        goal: &str,
        intent: &str,
        session_id: &str,
    ) -> Result<String> {
        let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());
        let ev = TaskEvent {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task_id.clone(),
            session_id: session_id.to_string(),
            parent_event_id: None,
            event_type: TaskEventType::TaskCreated,
            payload: TaskEventPayload::Task {
                goal: goal.to_string(),
                intent: intent.to_string(),
            },
            schema_version: TASK_EVENT_SCHEMA_VERSION,
            ts: now_rfc3339(),
        };
        let json = encode_event(&ev)?;
        let mut client = self.client().await?;
        // Single transaction: event INSERT + brief INSERT (contract: the
        // index is updated atomically with the append).
        let tx = client.transaction().await.map_err(pg_err)?;
        tx.execute(
            // `::text::jsonb` (not `::jsonb`): a bare cast makes Postgres resolve the
            // parameter type AS jsonb, which tokio-postgres's `ToSql for String`
            // refuses; the double cast pins it to text (no serde_json feature
            // needed on the driver).
            "INSERT INTO working_state_events (id, task_id, event) VALUES ($1, $2, $3::text::jsonb)",
            &[&ev.id, &ev.task_id, &json],
        )
        .await
        .map_err(pg_err)?;
        // The TaskCreated event records session_id but the durable user/project
        // scoping lives in the brief (events don't all carry user/project).
        tx.execute(
            "INSERT INTO working_state_briefs
                 (task_id, goal, status, open_step_count, open_blocker_count, user_id, project, last_event_ts)
             VALUES ($1, $2, 'active', 0, 0, $3, $4, $5)
             ON CONFLICT (task_id) DO UPDATE SET
                 user_id = EXCLUDED.user_id,
                 project = EXCLUDED.project,
                 last_event_ts = EXCLUDED.last_event_ts",
            &[&task_id, &goal, &user_id, &project, &ev.ts],
        )
        .await
        .map_err(pg_err)?;
        tx.commit().await.map_err(pg_err)?;
        Ok(task_id)
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<WorkingState>> {
        let client = self.client().await?;
        let events = read_events_on(&client, task_id).await?;
        if events.is_empty() {
            return Ok(None);
        }
        let mut state = match project(&events) {
            Some(s) => s,
            None => return Ok(None),
        };
        // Backfill user_id / project from the brief (events don't carry them).
        if let Some(brief) = fetch_brief(&client, task_id).await? {
            state.user_id = brief.user_id;
            state.project = brief.project;
            if state.owner_session.is_empty() {
                state.owner_session = state.user_id.clone();
            }
        }
        Ok(Some(state))
    }

    async fn list_open_tasks(&self, user_id: &str, project: &str) -> Result<Vec<TaskBrief>> {
        let client = self.client().await?;
        // Filter semantics mirror FileWorkingStateStore::list_open_tasks:
        // empty user/project = unconstrained; is_open() = active|paused;
        // most-recently-touched first (RFC3339 text sort = chrono sort).
        let rows = client
            .query(
                "SELECT task_id, goal, status, open_step_count, open_blocker_count,
                        user_id, project, last_event_ts
                 FROM working_state_briefs
                 WHERE ($1 = '' OR user_id = $1)
                   AND ($2 = '' OR project = $2)
                 ORDER BY last_event_ts DESC",
                &[&user_id, &project],
            )
            .await
            .map_err(pg_err)?;
        let briefs = rows
            .into_iter()
            .map(|r| TaskBrief {
                task_id: r.get(0),
                goal: r.get(1),
                status: status_from_str(&r.get::<_, String>(2)),
                open_step_count: r.get::<_, i32>(3) as u32,
                open_blocker_count: r.get::<_, i32>(4) as u32,
                user_id: r.get(5),
                project: r.get(6),
                last_event_ts: r.get(7),
                file: String::new(),
            })
            .filter(|b| b.status.is_open())
            .collect();
        Ok(briefs)
    }

    async fn append_event(
        &self,
        task_id: &str,
        session_id: &str,
        parent_event_id: Option<&str>,
        event_type: TaskEventType,
        payload: TaskEventPayload,
    ) -> Result<String> {
        let ev = TaskEvent {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            parent_event_id: parent_event_id.map(str::to_string),
            event_type,
            payload,
            schema_version: TASK_EVENT_SCHEMA_VERSION,
            ts: now_rfc3339(),
        };
        let json = encode_event(&ev)?;
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(pg_err)?;
        // Per-task serialization lock FIRST, so the re-derivation below sees
        // every concurrently committed event (multi-writer safety — the whole
        // point of the transactional index).
        lock_task_on(&tx, task_id).await?;
        tx.execute(
            // `::text::jsonb` (not `::jsonb`): a bare cast makes Postgres resolve the
            // parameter type AS jsonb, which tokio-postgres's `ToSql for String`
            // refuses; the double cast pins it to text (no serde_json feature
            // needed on the driver).
            "INSERT INTO working_state_events (id, task_id, event) VALUES ($1, $2, $3::text::jsonb)",
            &[&ev.id, &ev.task_id, &json],
        )
        .await
        .map_err(pg_err)?;
        // Re-derive the brief from the full log inside the SAME transaction
        // (re-derive on demand is acceptable — append is not the hot path;
        // the per-turn hot path is the in-memory LoopState projection).
        let events = read_events_on(&tx, task_id).await?;
        let state = project(&events);
        upsert_brief_on(&tx, task_id, state.as_ref(), &ev.ts).await?;
        tx.commit().await.map_err(pg_err)?;
        Ok(ev.id)
    }

    async fn derive_state(&self, task_id: &str) -> Result<WorkingState> {
        match self.get_task(task_id).await? {
            Some(s) => Ok(s),
            None => Err(OneAIError::Persistence(format!(
                "No working state for task '{}'",
                task_id
            ))),
        }
    }

    async fn compact_if_needed(&self, task_id: &str) -> Result<()> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(pg_err)?;
        // Same per-task lock as append_event: the rewrite must not interleave
        // with a concurrent append.
        lock_task_on(&tx, task_id).await?;
        let events = read_events_on(&tx, task_id).await?;
        if events.len() < self.event_threshold.load(Ordering::Relaxed) {
            return Ok(()); // no-op; tx dropped = rollback (nothing written)
        }
        // Snapshot = state projected from events[..tail_start]; keep the tail.
        // Mirrors FileWorkingStateStore::compact_if_needed exactly, but the
        // rewrite is a transactional DELETE-all + INSERT snapshot+tail (new
        // seqs keep ordering monotonic; logically equivalent, idempotent).
        let tail_start = events
            .len()
            .saturating_sub(self.keep_recent.load(Ordering::Relaxed));
        let snapshot_state = match project(&events[..tail_start]) {
            Some(s) => s,
            None => return Ok(()),
        };
        let snapshot_event = TaskEvent {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            session_id: snapshot_state.owner_session.clone(),
            parent_event_id: events
                .get(tail_start.saturating_sub(1))
                .map(|e| e.id.clone()),
            event_type: TaskEventType::Snapshot,
            payload: TaskEventPayload::Snapshot {
                state: snapshot_state,
            },
            schema_version: TASK_EVENT_SCHEMA_VERSION,
            ts: now_rfc3339(),
        };
        let mut compacted = Vec::with_capacity(1 + (events.len() - tail_start));
        compacted.push(snapshot_event);
        compacted.extend_from_slice(&events[tail_start..]);

        tx.execute(
            "DELETE FROM working_state_events WHERE task_id = $1",
            &[&task_id],
        )
        .await
        .map_err(pg_err)?;
        for ev in &compacted {
            let json = encode_event(ev)?;
            tx.execute(
                // `::text::jsonb` (not `::jsonb`): a bare cast makes Postgres resolve the
            // parameter type AS jsonb, which tokio-postgres's `ToSql for String`
            // refuses; the double cast pins it to text (no serde_json feature
            // needed on the driver).
            "INSERT INTO working_state_events (id, task_id, event) VALUES ($1, $2, $3::text::jsonb)",
                &[&ev.id, &ev.task_id, &json],
            )
            .await
            .map_err(pg_err)?;
        }
        tx.commit().await.map_err(pg_err)?;
        tracing::info!(
            "Compacted task '{}' log (pg): {} -> {} events",
            task_id,
            events.len(),
            compacted.len()
        );
        Ok(())
    }

    async fn archive_task(&self, task_id: &str) -> Result<()> {
        // TaskArchived projects to status=archived, and append_event re-derives
        // the brief in-transaction — so this single call both marks the index
        // and keeps the log consistent. Events stay queryable (audit); see the
        // deliberate-deviation note in the module docs.
        self.append_event(
            task_id,
            "",
            None,
            TaskEventType::TaskArchived,
            TaskEventPayload::TaskStatus {},
        )
        .await?;
        Ok(())
    }

    fn set_compaction(&self, event_threshold: usize, keep_recent: usize) {
        // Disambiguate from the inherent method (same name) so the
        // trait-object path reaches the concrete hot-swap logic.
        PgWorkingStateStore::set_compaction(self, event_threshold, keep_recent);
    }
}
