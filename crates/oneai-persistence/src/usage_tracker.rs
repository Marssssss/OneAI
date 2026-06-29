//! SQLite-backed usage tracker — persistent token-usage tracking across restarts.
//!
//! The `SqliteUsageTracker` provides a persistent usage tracking backend
//! using the same SQLite database as `SqliteSessionStore`. This enables:
//! - Per-session usage tracking that survives restarts
//! - Global usage aggregation across all sessions
//! - Per-model usage breakdowns
//! - Usage export (JSON/CSV) for analysis
//!
//! Uses a `usage_records` table in the same database file, sharing
//! the connection and schema auto-creation pattern with the session store.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};
use oneai_core::usage::{UsageRecord, UsageSummary, UsageTracker};

// ─── SqliteUsageTracker ──────────────────────────────────────────────────────

/// SQLite-backed usage tracker — persistent usage tracking across restarts.
///
/// Uses a `usage_records` table to store per-inference-call token-usage data.
/// Can share the same database file as `SqliteSessionStore` or use
/// a separate database file.
///
/// **Usage**:
/// ```ignore
/// // Share the same database as SqliteSessionStore
/// let store = SqliteSessionStore::with_defaults();
/// let usage_tracker = SqliteUsageTracker::from_store(&store);
///
/// // Or use a separate database
/// let usage_tracker = SqliteUsageTracker::new("/path/to/usage.db");
/// ```
pub struct SqliteUsageTracker {
    /// Path to the SQLite database file.
    db_path: PathBuf,
}

impl SqliteUsageTracker {
    /// Create a new SQLite usage tracker with the given database path.
    ///
    /// The database file will be created if it doesn't exist.
    /// The `usage_records` table is auto-created on first use.
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    /// Create a usage tracker sharing the same database as a SqliteSessionStore.
    ///
    /// This uses the same database file, adding the `usage_records` table
    /// alongside the existing session/STM/LTM tables.
    pub fn from_store(store: &crate::SqliteSessionStore) -> Self {
        Self::new(store.db_path().clone())
    }

    /// Use the default database path (`~/.oneai/oneai.db`).
    pub fn with_defaults() -> Self {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        let dir = PathBuf::from(home).join(".oneai");
        let _ = std::fs::create_dir_all(&dir);
        Self::new(dir.join("oneai.db"))
    }

    /// Open a connection to the SQLite database and ensure the schema exists.
    fn open_connection(&self) -> std::result::Result<rusqlite::Connection, OneAIError> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| OneAIError::Persistence(
                format!("Failed to open SQLite database at {}: {}", self.db_path.display(), e)
            ))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_records (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                model TEXT NOT NULL,
                provider TEXT NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                metadata_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_records(session_id);
            CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_records(model);
            CREATE INDEX IF NOT EXISTS idx_usage_timestamp ON usage_records(timestamp);"
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to create usage_records schema: {}", e)
        ))?;

        Ok(conn)
    }

    /// Insert a usage record into the database.
    fn insert_record(&self, conn: &rusqlite::Connection, record: &UsageRecord) -> std::result::Result<(), OneAIError> {
        let id = uuid::Uuid::new_v4().to_string();
        let timestamp = record.timestamp.to_rfc3339();
        let metadata_json = serde_json::to_string(&record.metadata)
            .unwrap_or_else(|_| "{}".to_string());

        conn.execute(
            "INSERT INTO usage_records (id, session_id, model, provider, prompt_tokens, completion_tokens, timestamp, metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![id, record.session_id, record.model, record.provider,
                record.prompt_tokens, record.completion_tokens,
                timestamp, metadata_json],
        ).map_err(|e| OneAIError::Usage(
            format!("Failed to insert usage record: {}", e)
        ))?;

        Ok(())
    }

    /// Load usage records from the database for a specific session.
    fn load_session_records(&self, conn: &rusqlite::Connection, session_id: &str) -> std::result::Result<Vec<UsageRecord>, OneAIError> {
        let mut stmt = conn.prepare(
            "SELECT session_id, model, provider, prompt_tokens, completion_tokens, timestamp, metadata_json
             FROM usage_records WHERE session_id = ?1 ORDER BY timestamp ASC"
        ).map_err(|e| OneAIError::Usage(
            format!("Failed to prepare query: {}", e)
        ))?;

        let records = stmt.query_map(rusqlite::params![session_id], |row| {
            let session_id: String = row.get(0)?;
            let model: String = row.get(1)?;
            let provider: String = row.get(2)?;
            let prompt_tokens: u32 = row.get(3)?;
            let completion_tokens: u32 = row.get(4)?;
            let timestamp_str: String = row.get(5)?;
            let metadata_json: String = row.get(6)?;

            let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());

            let metadata: HashMap<String, String> = serde_json::from_str(&metadata_json)
                .unwrap_or_default();

            let record = UsageRecord::with_timestamp(
                session_id,
                model,
                provider,
                prompt_tokens,
                completion_tokens,
                timestamp,
                metadata,
            );

            Ok(record)
        }).map_err(|e| OneAIError::Usage(
            format!("Failed to query usage records: {}", e)
        ))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(records)
    }

    /// Load all usage records from the database.
    fn load_all_records(&self, conn: &rusqlite::Connection) -> std::result::Result<Vec<UsageRecord>, OneAIError> {
        let mut stmt = conn.prepare(
            "SELECT session_id, model, provider, prompt_tokens, completion_tokens, timestamp, metadata_json
             FROM usage_records ORDER BY timestamp ASC"
        ).map_err(|e| OneAIError::Usage(
            format!("Failed to prepare query: {}", e)
        ))?;

        let records = stmt.query_map([], |row| {
            let session_id: String = row.get(0)?;
            let model: String = row.get(1)?;
            let provider: String = row.get(2)?;
            let prompt_tokens: u32 = row.get(3)?;
            let completion_tokens: u32 = row.get(4)?;
            let timestamp_str: String = row.get(5)?;
            let metadata_json: String = row.get(6)?;

            let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());

            let metadata: HashMap<String, String> = serde_json::from_str(&metadata_json)
                .unwrap_or_default();

            let record = UsageRecord::with_timestamp(
                session_id,
                model,
                provider,
                prompt_tokens,
                completion_tokens,
                timestamp,
                metadata,
            );

            Ok(record)
        }).map_err(|e| OneAIError::Usage(
            format!("Failed to query usage records: {}", e)
        ))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(records)
    }

    /// Get the database path.
    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }
}

#[async_trait]
impl UsageTracker for SqliteUsageTracker {
    async fn record_usage(&self, record: UsageRecord) -> Result<()> {
        let conn = self.open_connection()?;
        self.insert_record(&conn, &record)?;
        Ok(())
    }

    async fn session_usage(&self, session_id: &str) -> Result<UsageSummary> {
        let conn = self.open_connection()?;
        let records = self.load_session_records(&conn, session_id)?;
        Ok(UsageSummary::from_records(&records))
    }

    async fn global_usage(&self) -> Result<UsageSummary> {
        let conn = self.open_connection()?;
        let records = self.load_all_records(&conn)?;
        Ok(UsageSummary::from_records(&records))
    }

    async fn usage_by_model(&self, session_id: &str) -> Result<HashMap<String, UsageSummary>> {
        let conn = self.open_connection()?;
        let records = self.load_session_records(&conn, session_id)?;

        let mut by_model: HashMap<String, Vec<UsageRecord>> = HashMap::new();
        for record in records {
            by_model.entry(record.model.clone()).or_default().push(record);
        }

        Ok(by_model.into_iter()
            .map(|(model, records)| (model, UsageSummary::from_records(&records)))
            .collect())
    }

    async fn usage_by_model_global(&self) -> Result<HashMap<String, UsageSummary>> {
        let conn = self.open_connection()?;
        let records = self.load_all_records(&conn)?;

        let mut by_model: HashMap<String, Vec<UsageRecord>> = HashMap::new();
        for record in records {
            by_model.entry(record.model.clone()).or_default().push(record);
        }

        Ok(by_model.into_iter()
            .map(|(model, records)| (model, UsageSummary::from_records(&records)))
            .collect())
    }

    async fn session_records(&self, session_id: &str) -> Result<Vec<UsageRecord>> {
        let conn = self.open_connection()?;
        self.load_session_records(&conn, session_id)
    }

    async fn global_records(&self) -> Result<Vec<UsageRecord>> {
        let conn = self.open_connection()?;
        self.load_all_records(&conn)
    }

    async fn clear_session(&self, session_id: &str) -> Result<()> {
        let conn = self.open_connection()?;
        conn.execute(
            "DELETE FROM usage_records WHERE session_id = ?1",
            rusqlite::params![session_id],
        ).map_err(|e| OneAIError::Usage(
            format!("Failed to clear session usage data: {}", e)
        ))?;
        Ok(())
    }

    async fn clear_all(&self) -> Result<()> {
        let conn = self.open_connection()?;
        conn.execute("DELETE FROM usage_records", [])
            .map_err(|e| OneAIError::Usage(
                format!("Failed to clear all usage data: {}", e)
            ))?;
        Ok(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracker() -> (SqliteUsageTracker, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let tracker = SqliteUsageTracker::new(tmp.path().join("test_usage.db"));
        (tracker, tmp)
    }

    #[tokio::test]
    async fn test_sqlite_usage_tracker_record_and_session_usage() {
        let (tracker, _tmp) = make_tracker();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "claude-sonnet-4", "anthropic", 200, 100)).await.unwrap();

        let session_usage = tracker.session_usage("sess1").await.unwrap();
        assert_eq!(session_usage.call_count, 2);
        assert_eq!(session_usage.total_tokens, 450);
    }

    #[tokio::test]
    async fn test_sqlite_usage_tracker_global_usage() {
        let (tracker, _tmp) = make_tracker();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess2", "gpt-4o", "openai", 100, 50)).await.unwrap();

        let global = tracker.global_usage().await.unwrap();
        assert_eq!(global.call_count, 2);
        assert_eq!(global.total_tokens, 300);
    }

    #[tokio::test]
    async fn test_sqlite_usage_tracker_per_model_breakdown() {
        let (tracker, _tmp) = make_tracker();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "claude-sonnet-4", "anthropic", 200, 100)).await.unwrap();

        let by_model = tracker.usage_by_model("sess1").await.unwrap();
        assert_eq!(by_model.len(), 2);
        assert!(by_model.contains_key("gpt-4o"));
        assert!(by_model.contains_key("claude-sonnet-4"));
    }

    #[tokio::test]
    async fn test_sqlite_usage_tracker_clear() {
        let (tracker, _tmp) = make_tracker();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess2", "gpt-4o", "openai", 100, 50)).await.unwrap();

        tracker.clear_session("sess1").await.unwrap();
        let sess1 = tracker.session_usage("sess1").await.unwrap();
        assert_eq!(sess1.call_count, 0);

        tracker.clear_all().await.unwrap();
        let global = tracker.global_usage().await.unwrap();
        assert_eq!(global.call_count, 0);
    }

    #[tokio::test]
    async fn test_sqlite_usage_tracker_session_records() {
        let (tracker, _tmp) = make_tracker();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "claude-sonnet-4", "anthropic", 200, 100)).await.unwrap();

        let records = tracker.session_records("sess1").await.unwrap();
        assert_eq!(records.len(), 2);
    }
}
