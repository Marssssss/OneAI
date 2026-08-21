//! SQLite-backed thinking-effort store — the durable, crash-surviving
//! counterpart to the in-memory [`oneai_core::ThinkingEffortStore`] selection.
//!
//! `SqliteThinkingEffort` persists the user's chosen [`ThinkingEffort`] tier
//! in the same `~/.oneai/oneai.db` shared by `SqliteSessionStore` /
//! `SqliteHostAllowlist`, so a tier selected in the web UI is honoured in the
//! next session without re-prompting. The hot read path is the engine
//! (`AppSession` reads the tier each turn before building the
//! `AgentLoopConfig`); this durable layer is the persist seam behind the
//! `thinking/get`·`thinking/set` JSON-RPC methods.
//!
//! Schema: a generic `kv_settings(key TEXT PRIMARY KEY, value TEXT)` table.
//! `thinking_effort` is stored as one row (`key='thinking_effort'`,
//! `value`=the tier's lowercase serde form) so future per-app settings can
//! ride the same table without a new schema.

use std::path::PathBuf;

use async_trait::async_trait;
use oneai_core::error::OneAIError;
use oneai_core::{ThinkingEffort, ThinkingEffortStore};

/// The `kv_settings` row key for the thinking-effort selection.
const KEY_THINKING_EFFORT: &str = "thinking_effort";

/// SQLite-backed, persistent thinking-effort selection.
///
/// Shares `~/.oneai/oneai.db` with the session + host-allowlist stores (or a
/// caller-supplied path); the `kv_settings` table is auto-created on first
/// use. See module docs.
pub struct SqliteThinkingEffort {
    db_path: PathBuf,
}

impl SqliteThinkingEffort {
    /// Create with an explicit database path (created if absent).
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    /// Share the same database as a `SqliteSessionStore` — the common wiring
    /// (one db file for sessions + usage + host allowlist + settings).
    pub fn from_store(store: &crate::SqliteSessionStore) -> Self {
        Self::new(store.db_path().clone())
    }

    /// Default path: `~/.oneai/oneai.db` (same as `SqliteSessionStore`).
    pub fn with_defaults() -> Self {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        let dir = PathBuf::from(home).join(".oneai");
        let _ = std::fs::create_dir_all(&dir);
        Self::new(dir.join("oneai.db"))
    }

    /// The database path (for tests / diagnostics).
    pub fn db_path(&self) -> &std::path::Path {
        &self.db_path
    }

    /// Open a connection, apply WAL pragmas, and ensure the `kv_settings`
    /// table exists. Same rationale as `SqliteHostAllowlist`: the db is
    /// concurrently written by the TUI / supervisor / gateway processes, so
    /// WAL + busy_timeout keeps concurrent writers from failing with
    /// `database is locked`.
    fn open_connection(&self) -> std::result::Result<rusqlite::Connection, OneAIError> {
        let conn = rusqlite::Connection::open(&self.db_path).map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to open SQLite database at {}: {}",
                self.db_path.display(),
                e
            ))
        })?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| OneAIError::Persistence(format!("set busy_timeout: {e}")))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| OneAIError::Persistence(format!("set WAL pragma: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .map_err(|e| OneAIError::Persistence(format!("create kv_settings schema: {e}")))?;
        Ok(conn)
    }
}

#[async_trait]
impl ThinkingEffortStore for SqliteThinkingEffort {
    async fn get(&self) -> ThinkingEffort {
        // `get` has no persisted value yet → the default tier (Medium), so the
        // out-of-box experience is the speed-balanced default rather than the
        // 57s-max rumination.
        match self.open_connection() {
            Ok(conn) => match conn.query_row(
                "SELECT value FROM kv_settings WHERE key = ?1",
                rusqlite::params![KEY_THINKING_EFFORT],
                |row| row.get::<_, String>(0),
            ) {
                Ok(json) => serde_json::from_str::<ThinkingEffort>(&json).unwrap_or_default(),
                Err(rusqlite::Error::QueryReturnedNoRows) => ThinkingEffort::default(),
                Err(e) => {
                    tracing::warn!("SqliteThinkingEffort::get query failed: {e}");
                    ThinkingEffort::default()
                }
            },
            Err(e) => {
                tracing::warn!("SqliteThinkingEffort::get open failed: {e}");
                ThinkingEffort::default()
            }
        }
    }

    async fn set(&self, effort: ThinkingEffort) {
        let json = serde_json::to_string(&effort).unwrap_or_else(|_| "medium".to_string());
        match self.open_connection() {
            Ok(conn) => {
                let _ = conn.execute(
                    "INSERT INTO kv_settings (key, value) VALUES (?1, ?2) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![KEY_THINKING_EFFORT, &json],
                );
            }
            Err(e) => tracing::warn!("SqliteThinkingEffort::set open failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway db in a unique temp path per test (no shared ~/.oneai).
    fn tmp_store() -> SqliteThinkingEffort {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "oneai-thinking-effort-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            n,
        ));
        let _ = std::fs::create_dir_all(&dir);
        SqliteThinkingEffort::new(dir.join("settings.db"))
    }

    #[tokio::test]
    async fn default_is_medium_when_unset() {
        let store = tmp_store();
        assert_eq!(store.get().await, ThinkingEffort::Medium);
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let store = tmp_store();
        for tier in [
            ThinkingEffort::Off,
            ThinkingEffort::Low,
            ThinkingEffort::Medium,
            ThinkingEffort::High,
            ThinkingEffort::Max,
        ] {
            store.set(tier).await;
            assert_eq!(store.get().await, tier, "{tier:?} round-trips");
        }
    }

    #[tokio::test]
    async fn survives_reopen() {
        // The whole point of the durable layer: a fresh handle to the same db
        // file sees the tier written by the first.
        let path = std::env::temp_dir().join(format!(
            "oneai-thinking-reopen-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        {
            let s = SqliteThinkingEffort::new(&path);
            s.set(ThinkingEffort::High).await;
        }
        let s2 = SqliteThinkingEffort::new(&path);
        assert_eq!(s2.get().await, ThinkingEffort::High);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn set_overwrites_prior_value() {
        let store = tmp_store();
        store.set(ThinkingEffort::High).await;
        assert_eq!(store.get().await, ThinkingEffort::High);
        store.set(ThinkingEffort::Off).await;
        assert_eq!(store.get().await, ThinkingEffort::Off);
    }
}
