//! Memory-backend selection for the standalone admin subcommands (`oneai
//! session list/resume/delete/info/export-hf`, `oneai memory search/list`)
//! — MVS3-C.
//!
//! These commands run WITHOUT an `App` (no builder pipeline to hook
//! `apply_pg_backends` into), so they need the same backend decision the
//! engine entry points make: when `ONEAI_PG_DSN` is set AND this binary was
//! compiled with the `postgres` feature, the durable session/memory store is
//! the shared Postgres (`PgMemoryStore`); otherwise it is the local
//! `~/.oneai/oneai.db` SQLite. A DSN that is set but unusable degrades
//! HONESTLY: loud warning, SQLite fallback — never a silent divergence
//! (Pg-managed cloud sessions must not appear "deleted" to admin tooling).
//!
//! Mirrors `working_state.rs::open_store` (the `tasks *` parallel).

use std::sync::Arc;

use oneai_core::traits::MemoryPersistence;
use oneai_persistence::SqliteSessionStore;

use crate::working_state::{pg_dsn, PG_DSN_ENV};

/// Open the memory backend the admin subcommands operate on: shared Pg when
/// selected, else the local SQLite default. See the module docs.
pub(crate) async fn open_memory_backend() -> Arc<dyn MemoryPersistence> {
    if let Some(dsn) = pg_dsn() {
        #[cfg(feature = "postgres")]
        {
            match oneai_persistence::PgMemoryStore::connect(&dsn).await {
                Ok(store) => {
                    eprintln!("   sessions: Postgres (shared)");
                    return Arc::new(store);
                }
                Err(e) => eprintln!(
                    "Warning: {PG_DSN_ENV} is set but PgMemoryStore connect failed: {e} \
                     — falling back to the local SQLite session store"
                ),
            }
        }
        #[cfg(not(feature = "postgres"))]
        {
            let _ = &dsn;
            eprintln!(
                "Warning: {PG_DSN_ENV} is set but this binary was built without the `postgres` \
                 feature — using the local SQLite session store"
            );
        }
    }
    Arc::new(SqliteSessionStore::with_defaults())
}
