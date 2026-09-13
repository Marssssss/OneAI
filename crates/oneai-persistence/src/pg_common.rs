//! Shared plumbing for the Postgres-backed stores (feature `postgres`, MVS3).
//!
//! Every Pg store (`PgWorkingStateStore`, `PgMemoryStore`, `PgUsageTracker`,
//! `PgHostAllowlist`) uses the same recipe:
//!
//! 1. [`build_pool`] — deadpool-postgres, `NoTls`, `RecyclingMethod::Fast`,
//!    `Runtime::Tokio1`, default size 8.
//! 2. [`ensure_schema`] — steady-state boot takes NO relation locks
//!    (`to_regclass` catalog probe first); a cold DB serializes DDL under a
//!    per-store advisory lock. Concurrent `CREATE TABLE IF NOT EXISTS` is
//!    racy in Postgres (duplicate pg_type insert) and `CREATE INDEX IF NOT
//!    EXISTS` on an existing index still grabs a table-level ShareLock that
//!    deadlocks against concurrent DML (observed E40P01 with N engines
//!    booting against one shared DB — see MVS3-A acceptance).
//! 3. [`pool_err`] / [`pg_err`] — uniform `OneAIError::Persistence` mapping
//!    that surfaces the server-side SQLSTATE + detail.
//!
//! ## Advisory-lock key registry
//! Fixed app-level keys ("ONEAI" = 0x4F4E4149 = 1330538825 as the base).
//! Each store owns a distinct key so a cold boot of several stores in one
//! process doesn't serialize their independent DDL blocks:
//!
//! | key        | store                |
//! |------------|----------------------|
//! | 1330538825 | PgWorkingStateStore  |
//! | 1330538826 | PgMemoryStore        |
//! | 1330538827 | PgUsageTracker       |
//! | 1330538828 | PgHostAllowlist      |

use deadpool_postgres::tokio_postgres::NoTls;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use oneai_core::error::{OneAIError, Result};

/// Advisory-lock key base ("ONEAI" = 0x4F4E4149). `PgWorkingStateStore` uses
/// the base itself; later stores use base+1/+2/+3 (see the registry above).
pub(crate) const ADVISORY_LOCK_BASE: i64 = 1330538825;

/// Build the standard OneAI Pg pool from a libpq connection string (e.g.
/// `postgres://user:pass@host:5432/dbname`).
///
/// TLS is not negotiated (`NoTls`) — target a same-host / same-VPC Postgres,
/// or terminate TLS in front of it (pgbouncer / cloud proxy).
pub(crate) fn build_pool(dsn: &str, max_size: usize) -> Result<Pool> {
    // deadpool-postgres re-exports its tokio-postgres; parse the DSN with
    // the same `FromStr` impl libpq URLs use.
    let pg_config: deadpool_postgres::tokio_postgres::Config = dsn.parse().map_err(|e| {
        OneAIError::Persistence(format!("Invalid ONEAI_PG_DSN connection string: {}", e))
    })?;
    let manager = Manager::from_config(
        pg_config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(max_size)
        .runtime(Runtime::Tokio1)
        .build()
        .map_err(|e| OneAIError::Persistence(format!("Failed to build Pg pool: {}", e)))
}

pub(crate) fn pool_err(e: deadpool_postgres::PoolError) -> OneAIError {
    OneAIError::Persistence(format!("Pg pool error: {}", e))
}

pub(crate) fn pg_err(e: deadpool_postgres::tokio_postgres::Error) -> OneAIError {
    // Surface the server-side DbError detail (message + SQLSTATE) — the bare
    // Display of a wrapped db error is just "db error", useless for triage.
    if let Some(db) = e.as_db_error() {
        return OneAIError::Persistence(format!(
            "Pg error [{:?}]: {}{}",
            db.code(),
            db.message(),
            db.detail()
                .map(|d| format!(" — detail: {}", d))
                .unwrap_or_default()
        ));
    }
    OneAIError::Persistence(format!("Pg error: {}", e))
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Catalog-probe → advisory-lock → re-check → idempotent DDL, at most once
/// per `ready` cell (one cell per store instance).
///
/// - `exists_sql` must be a `SELECT <bool expression>` that is TRUE when
///   every object the store needs already exists (catalog-only reads — no
///   relation locks on the steady-state path).
/// - `ddl` is applied with `batch_execute`; it must be idempotent
///   (`CREATE ... IF NOT EXISTS`) and may include `CREATE EXTENSION`.
/// - `lock_key` is the store's entry in the advisory-lock registry above.
pub(crate) async fn ensure_schema(
    pool: &Pool,
    ready: &tokio::sync::OnceCell<()>,
    lock_key: i64,
    exists_sql: &str,
    ddl: &str,
) -> Result<()> {
    ready
        .get_or_try_init(|| async {
            let c = pool.get().await.map_err(pool_err)?;
            // Steady state: schema already there → skip DDL entirely.
            let exists: bool = c.query_one(exists_sql, &[]).await.map_err(pg_err)?.get(0);
            if exists {
                return Ok::<(), OneAIError>(());
            }
            // Cold DB: serialize DDL across ALL instances/processes.
            c.batch_execute(&format!("SELECT pg_advisory_lock({})", lock_key))
                .await
                .map_err(pg_err)?;
            // Re-check under the lock: another process may have just
            // finished the DDL while we waited.
            let exists: bool = c.query_one(exists_sql, &[]).await.map_err(pg_err)?.get(0);
            let ddl = if exists {
                Ok(())
            } else {
                c.batch_execute(ddl).await
            };
            let _ = c
                .batch_execute(&format!("SELECT pg_advisory_unlock({})", lock_key))
                .await;
            ddl.map_err(pg_err)?;
            Ok::<(), OneAIError>(())
        })
        .await
        .copied()
}
