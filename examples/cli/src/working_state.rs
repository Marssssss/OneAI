//! Working-state backend selection (MVS3 — storage externalization).
//!
//! One rule everywhere in the CLI: when `ONEAI_PG_DSN` is set AND this binary
//! was compiled with the `postgres` feature, the durable working state lives
//! in the shared Postgres (`PgWorkingStateStore` — transactional brief index,
//! crash-recovery lifeline across containers); otherwise it stays on the
//! file backend (`FileWorkingStateStore` at `<root>/tasks/*.jsonl`).
//!
//! Consumers:
//! - engine builders — [`apply_working_state`] (cmd_app_server's
//!   `build_engine_server` = the `oneai web`/app-server container path,
//!   cmd_serve, TUI, `tasks continue`)
//! - `oneai tasks list|show|archive` — [`open_store`]
//! - `oneai session export-hf --task <id>` — [`read_task_events`]
//!
//! The file root is ALWAYS configured on the builder even when Pg wins:
//! `AppBuilder::build()` derives the session-event store (`<root>/events/`)
//! and the InRepo skill-curator root from it, and the file store remains the
//! honest fallback when the DSN is set but unusable (loud warning, no silent
//! divergence).

use std::path::PathBuf;
use std::sync::Arc;

use oneai_app::AppBuilder;
use oneai_core::error::Result;
use oneai_core::traits::WorkingStateStore;
use oneai_core::TaskEvent;
use oneai_persistence::FileWorkingStateStore;

/// Env var selecting the shared Postgres backend. The orchestrator injects it
/// into every session container via `passthrough_env` (orchestrator.toml);
/// later Pg* stores (MVS3 follow-ups) will reuse the same DSN.
pub(crate) const PG_DSN_ENV: &str = "ONEAI_PG_DSN";

/// Non-empty `ONEAI_PG_DSN`, when set.
pub(crate) fn pg_dsn() -> Option<String> {
    std::env::var(PG_DSN_ENV).ok().filter(|s| !s.is_empty())
}

/// Connect the shared Postgres store when selected. `None` = not selected or
/// unusable (warning printed — honest degradation to the file backend). The
/// DSN is never echoed (it can carry a password).
pub(crate) async fn pg_store() -> Option<Arc<dyn WorkingStateStore>> {
    let dsn = pg_dsn()?;
    #[cfg(feature = "postgres")]
    {
        match oneai_persistence::PgWorkingStateStore::connect(&dsn).await {
            Ok(store) => {
                eprintln!("   working-state: Postgres (shared)");
                Some(Arc::new(store))
            }
            Err(e) => {
                eprintln!(
                    "Warning: {PG_DSN_ENV} is set but the Postgres connection failed: {e} \
                     — falling back to the file working-state"
                );
                None
            }
        }
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = &dsn;
        eprintln!(
            "Warning: {PG_DSN_ENV} is set but this binary was built without the `postgres` \
             feature — using the file working-state"
        );
        None
    }
}

/// Wire working-state onto an engine builder: always set the file root (see
/// module docs), then override the store with the shared Pg when selected.
pub(crate) async fn apply_working_state(
    builder: AppBuilder,
    root: impl Into<PathBuf>,
) -> AppBuilder {
    let builder = builder.working_state(root);
    match pg_store().await {
        Some(store) => builder.working_state_store(store),
        None => builder,
    }
}

/// Open the store for the `oneai tasks *` subcommands: shared Pg when
/// selected, else the file backend at `root`.
pub(crate) async fn open_store(root: Option<&str>) -> Arc<dyn WorkingStateStore> {
    if let Some(store) = pg_store().await {
        return store;
    }
    Arc::new(FileWorkingStateStore::new(PathBuf::from(
        root.unwrap_or(crate::cmd_tasks::DEFAULT_ROOT),
    )))
}

/// Read a task's raw event log, honoring the selected backend
/// (`session export-hf --task <id>`). `read_events` is an inherent method on
/// each concrete store, so this branches on the backend rather than going
/// through the trait.
pub(crate) async fn read_task_events(ws_root: PathBuf, task_id: &str) -> Result<Vec<TaskEvent>> {
    if let Some(dsn) = pg_dsn() {
        #[cfg(feature = "postgres")]
        {
            match oneai_persistence::PgWorkingStateStore::connect(&dsn).await {
                Ok(store) => return store.read_events(task_id).await,
                Err(e) => eprintln!(
                    "Warning: {PG_DSN_ENV} is set but the Postgres connection failed: {e} \
                     — reading task events from the file backend"
                ),
            }
        }
        #[cfg(not(feature = "postgres"))]
        {
            let _ = &dsn;
            eprintln!(
                "Warning: {PG_DSN_ENV} is set but this binary was built without the `postgres` \
                 feature — reading task events from the file backend"
            );
        }
    }
    FileWorkingStateStore::new(ws_root)
        .read_events(task_id)
        .await
}
