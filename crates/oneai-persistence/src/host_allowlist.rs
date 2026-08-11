//! SQLite-backed host allow/deny store — the durable, crash-surviving
//! counterpart to [`oneai_tool::InMemoryHostAllowlist`] (#28 Stage 6).
//!
//! `SqliteHostAllowlist` implements [`HostAllowlistStore`] over the same
//! `~/.oneai/oneai.db` shared by `SqliteSessionStore` / `SqliteUsageTracker`,
//! so a host the user admitted (or blocked) in one session is honoured in the
//! next without re-prompting. The in-memory store remains the hot read path
//! (the proxy consults the store on every CONNECT); this durable layer is the
//! persist seam.
//!
//! Schema: two tables — `host_allowlist` (admitted hosts) and `host_denylist`
//! (blocked hosts). Both keyed by bare lower-cased hostname. `recorded_at` is
//! a unix-seconds audit column (no query relies on it today; it's there for a
//! future "expire stale denials" policy).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use oneai_core::error::OneAIError;
use oneai_core::HostAllowlistStore;

// ─── SqliteHostAllowlist ────────────────────────────────────────────────────

/// SQLite-backed, persistent host allow + deny store.
///
/// Shares `~/.oneai/oneai.db` with the session store (or a caller-supplied
/// path); tables are auto-created on first use. See module docs.
pub struct SqliteHostAllowlist {
    db_path: PathBuf,
}

impl SqliteHostAllowlist {
    /// Create with an explicit database path (created if absent).
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    /// Share the same database as a `SqliteSessionStore` — the common wiring
    /// (one db file for sessions + usage + host allowlist).
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

    /// Open a connection, apply WAL pragmas, and ensure both tables exist.
    fn open_connection(&self) -> std::result::Result<rusqlite::Connection, OneAIError> {
        let conn = rusqlite::Connection::open(&self.db_path).map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to open SQLite database at {}: {}",
                self.db_path.display(),
                e
            ))
        })?;

        // Same rationale as SqliteUsageTracker: the db is concurrently
        // written by the TUI / supervisor / gateway processes. WAL +
        // busy_timeout keeps concurrent writers from failing with
        // `database is locked`.
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| OneAIError::Persistence(format!("set busy_timeout: {e}")))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| OneAIError::Persistence(format!("set WAL pragma: {e}")))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS host_allowlist (
                host TEXT PRIMARY KEY,
                recorded_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS host_denylist (
                host TEXT PRIMARY KEY,
                recorded_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| OneAIError::Persistence(format!("create host allow/deny schema: {e}")))?;

        Ok(conn)
    }

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

#[async_trait]
impl HostAllowlistStore for SqliteHostAllowlist {
    async fn is_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        match self.open_connection() {
            Ok(conn) => {
                let exists: bool = conn
                    .query_row(
                        "SELECT 1 FROM host_allowlist WHERE host = ?1",
                        rusqlite::params![&host],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);
                exists
            }
            Err(e) => {
                tracing::warn!("SqliteHostAllowlist::is_allowed open failed: {e}");
                false // fail closed on the read path? No — failing closed here
                      // *prompts* (not admits); the proxy falls through to its
                      // gate-prompt path, which is the safe default.
            }
        }
    }

    async fn add(&self, host: String) {
        let host = host.to_ascii_lowercase();
        let at = Self::now_secs();
        match self.open_connection() {
            Ok(conn) => {
                // A host may be in the denylist from a prior denial — admitting
                // it removes the denial so a future connection isn't blocked by
                // a stale deny record.
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO host_allowlist (host, recorded_at) VALUES (?1, ?2)",
                    rusqlite::params![&host, at],
                );
                let _ = conn.execute(
                    "DELETE FROM host_denylist WHERE host = ?1",
                    rusqlite::params![&host],
                );
            }
            Err(e) => tracing::warn!("SqliteHostAllowlist::add open failed: {e}"),
        }
    }

    async fn is_denied(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        match self.open_connection() {
            Ok(conn) => conn
                .query_row(
                    "SELECT 1 FROM host_denylist WHERE host = ?1",
                    rusqlite::params![&host],
                    |_| Ok(true),
                )
                .unwrap_or(false),
            Err(e) => {
                tracing::warn!("SqliteHostAllowlist::is_denied open failed: {e}");
                false // fail open on deny read: don't block a host we couldn't
                      // look up — let the gate-prompt path decide.
            }
        }
    }

    async fn add_denied(&self, host: String) {
        let host = host.to_ascii_lowercase();
        let at = Self::now_secs();
        match self.open_connection() {
            Ok(conn) => {
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO host_denylist (host, recorded_at) VALUES (?1, ?2)",
                    rusqlite::params![&host, at],
                );
                // Mutually exclusive: a denied host is removed from the allowlist
                // so a stale admission can't silently re-admit it.
                let _ = conn.execute(
                    "DELETE FROM host_allowlist WHERE host = ?1",
                    rusqlite::params![&host],
                );
            }
            Err(e) => tracing::warn!("SqliteHostAllowlist::add_denied open failed: {e}"),
        }
    }
}

// ─── Result wrappers (the trait returns no Result, but the durable layer needs
//     an explicit fail signal for tests / introspection) ─────────────────────

impl SqliteHostAllowlist {
    /// The database path (for tests / diagnostics).
    pub fn db_path(&self) -> &std::path::Path {
        &self.db_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway db in a unique temp path per test (no shared ~/.oneai).
    fn tmp_store() -> SqliteHostAllowlist {
        let dir = std::env::temp_dir().join(format!(
            "oneai-host-allow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = std::fs::create_dir_all(&dir);
        SqliteHostAllowlist::new(dir.join("hosts.db"))
    }

    #[tokio::test]
    async fn add_then_allowed_persists() {
        let store = tmp_store();
        assert!(!store.is_allowed("example.com").await);
        store.add("Example.COM".to_string()).await; // case normalized
        assert!(store.is_allowed("example.com").await);
        assert!(store.is_allowed("EXAMPLE.com").await);
    }

    #[tokio::test]
    async fn add_denied_then_denied_persists() {
        let store = tmp_store();
        assert!(!store.is_denied("evil.example").await);
        store.add_denied("evil.example".to_string()).await;
        assert!(store.is_denied("evil.example").await);
    }

    #[tokio::test]
    async fn admit_removes_prior_denial() {
        // A host once denied, later admitted: the deny record is cleared so
        // the proxy's denylist short-circuit doesn't block it.
        let store = tmp_store();
        store.add_denied("flaky.example".to_string()).await;
        assert!(store.is_denied("flaky.example").await);
        store.add("flaky.example".to_string()).await;
        assert!(!store.is_denied("flaky.example").await);
        assert!(store.is_allowed("flaky.example").await);
    }

    #[tokio::test]
    async fn deny_removes_prior_admission() {
        let store = tmp_store();
        store.add("once-ok.example".to_string()).await;
        assert!(store.is_allowed("once-ok.example").await);
        store.add_denied("once-ok.example".to_string()).await;
        assert!(!store.is_allowed("once-ok.example").await);
        assert!(store.is_denied("once-ok.example").await);
    }

    #[tokio::test]
    async fn distinct_hosts_do_not_cross_contaminate() {
        let store = tmp_store();
        store.add("a.example".to_string()).await;
        store.add_denied("b.example".to_string()).await;
        assert!(store.is_allowed("a.example").await);
        assert!(!store.is_denied("a.example").await);
        assert!(store.is_denied("b.example").await);
        assert!(!store.is_allowed("b.example").await);
    }

    #[tokio::test]
    async fn survives_reopen() {
        // The whole point of the durable layer: a new store pointing at the
        // same db file sees the records written by the first.
        let path = std::env::temp_dir().join(format!(
            "oneai-host-reopen-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        {
            let s = SqliteHostAllowlist::new(&path);
            s.add("persisted.example".to_string()).await;
            s.add_denied("blocked.example".to_string()).await;
        }
        // A fresh handle to the same file.
        let s2 = SqliteHostAllowlist::new(&path);
        assert!(s2.is_allowed("persisted.example").await);
        assert!(s2.is_denied("blocked.example").await);
        let _ = std::fs::remove_file(&path);
    }
}
