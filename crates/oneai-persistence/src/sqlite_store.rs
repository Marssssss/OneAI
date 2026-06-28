//! SQLite session store — unified persistence for conversations, STM, and LTM entries.
//!
//! The `SqliteSessionStore` provides a single SQLite database (`~/.oneai/oneai.db`)
//! that persists:
//! - **Conversations**: message history for multi-turn sessions
//! - **STM entries**: recent context window (sliding window state)
//! - **LTM entries**: long-term knowledge (content + optional embeddings)
//!
//! This addresses the critical gap where all memory was purely in-memory
//! (HashMap, VecDeque) and lost on application restart. With SQLite persistence,
//! sessions can be resumed and knowledge accumulates across restarts.
//!
//! The store implements the `MemoryPersistence` trait from `oneai-core`,
//! enabling seamless integration with the `MemoryManager`.
//!
//! **Design decisions**:
//! - Uses `rusqlite` with bundled SQLite (zero-config, works everywhere)
//! - Embeddings stored as JSON arrays (`Vec<f32>` serialized)
//! - Keyword search uses SQL `LIKE` (no FTS5 dependency)
//! - Embedding search uses brute-force cosine similarity in Rust
//!   (acceptable for <10K entries; future: use HNSW or FTS5 vector extension)
//! - One database file for all tables (shared with `SqliteCheckpointBackend`)

use std::path::PathBuf;
use std::collections::HashMap;

use async_trait::async_trait;
use oneai_core::{Conversation, MemoryEntry, MemoryFact, SessionInfo};
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::MemoryPersistence;

// ─── SqliteSessionStore ─────────────────────────────────────────────────────

/// SQLite-backed session store for conversations, STM, and LTM persistence.
///
/// Uses a single SQLite database file with three tables:
/// - `conversations`: message history (serialized as JSON)
/// - `stm_entries`: short-term memory entries (per session, ordered by position)
/// - `ltm_entries`: long-term memory entries (with optional embeddings)
///
/// All tables are created automatically on first use.
pub struct SqliteSessionStore {
    /// Path to the SQLite database file.
    db_path: PathBuf,
}

impl SqliteSessionStore {
    /// Create a new SQLite session store with the given database path.
    ///
    /// The database file will be created if it doesn't exist.
    /// The schema (tables + indexes) is auto-created on first connection.
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self { db_path: db_path.into() }
    }

    /// Create a SQLite session store with the default path (`~/.oneai/oneai.db`).
    ///
    /// Creates the `~/.oneai/` directory if it doesn't exist.
    pub fn with_defaults() -> Self {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        let dir = PathBuf::from(home).join(".oneai");
        let _ = std::fs::create_dir_all(&dir);
        Self::new(dir.join("oneai.db"))
    }

    /// Open a connection to the SQLite database and ensure the schema exists.
    ///
    /// Called internally by each method. Creates all tables and indexes
    /// automatically if they don't exist.
    fn open_connection(&self) -> std::result::Result<rusqlite::Connection, OneAIError> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| OneAIError::Persistence(
                format!("Failed to open SQLite database at {}: {}", self.db_path.display(), e)
            ))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversations (
                id TEXT PRIMARY KEY,
                messages_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_conv_updated ON conversations(updated_at);

            CREATE TABLE IF NOT EXISTS stm_entries (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                content TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                embedding_json TEXT,
                metadata_json TEXT NOT NULL,
                position INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_stm_session ON stm_entries(session_id);

            CREATE TABLE IF NOT EXISTS ltm_entries (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                embedding_json TEXT,
                metadata_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_ltm_timestamp ON ltm_entries(timestamp);

            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                fact_type TEXT NOT NULL,
                subject TEXT NOT NULL,
                predicate TEXT NOT NULL,
                content TEXT NOT NULL,
                embedding_json TEXT,
                metadata_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                version INTEGER NOT NULL DEFAULT 1
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_key ON memories(user_id, subject, predicate);
            CREATE INDEX IF NOT EXISTS idx_memories_user ON memories(user_id);
            CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);"
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to create session store schema: {}", e)
        ))?;

        Ok(conn)
    }

    /// Get the database path.
    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }
}

// ─── Helper functions ───────────────────────────────────────────────────────

/// Serialize a MemoryEntry's embedding as JSON.
fn serialize_embedding(embedding: &Option<Vec<f32>>) -> Option<String> {
    embedding.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default())
}

/// Deserialize a JSON string back to Vec<f32>.
fn deserialize_embedding(json: &str) -> Option<Vec<f32>> {
    if json.is_empty() {
        return None;
    }
    serde_json::from_str(json).ok()
}

/// Serialize a HashMap<String, String> as JSON.
fn serialize_metadata(metadata: &std::collections::HashMap<String, String>) -> String {
    serde_json::to_string(metadata).unwrap_or_default()
}

/// Deserialize a JSON string back to HashMap<String, String>.
fn deserialize_metadata(json: &str) -> std::collections::HashMap<String, String> {
    if json.is_empty() {
        return std::collections::HashMap::new();
    }
    serde_json::from_str(json).unwrap_or_default()
}

/// Compute cosine similarity between two vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

// ─── MemoryPersistence trait implementation ──────────────────────────────────

#[async_trait]
impl MemoryPersistence for SqliteSessionStore {
    // ─── STM operations ───────────────────────────────────────────────

    async fn save_stm(&self, session_id: &str, entries: &[MemoryEntry]) -> Result<()> {
        let conn = self.open_connection()?;

        // First, clear existing STM entries for this session
        conn.execute(
            "DELETE FROM stm_entries WHERE session_id = ?1",
            rusqlite::params![session_id],
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to clear STM entries for session '{}': {}", session_id, e)
        ))?;

        // Insert new entries with position ordering
        for (position, entry) in entries.iter().enumerate() {
            let embedding_json = serialize_embedding(&entry.embedding);
            let metadata_json = serialize_metadata(&entry.metadata);
            let timestamp = entry.timestamp.to_rfc3339();

            conn.execute(
                "INSERT INTO stm_entries (id, session_id, content, timestamp, embedding_json, metadata_json, position) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    entry.id,
                    session_id,
                    entry.content,
                    timestamp,
                    embedding_json,
                    metadata_json,
                    position,
                ],
            ).map_err(|e| OneAIError::Persistence(
                format!("Failed to save STM entry '{}': {}", entry.id, e)
            ))?;
        }

        tracing::debug!("Saved {} STM entries for session '{}'", entries.len(), session_id);
        Ok(())
    }

    async fn load_stm(&self, session_id: &str) -> Result<Vec<MemoryEntry>> {
        let conn = self.open_connection()?;

        let mut stmt = conn.prepare(
            "SELECT id, content, timestamp, embedding_json, metadata_json \
             FROM stm_entries WHERE session_id = ?1 ORDER BY position ASC"
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to prepare STM load query: {}", e)
        ))?;

        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            let timestamp_str: String = row.get(2)?;
            let embedding_json: Option<String> = row.get(3)?;
            let metadata_json: String = row.get(4)?;
            Ok((id, content, timestamp_str, embedding_json, metadata_json))
        }).map_err(|e| OneAIError::Persistence(
            format!("Failed to execute STM load query: {}", e)
        ))?;

        let mut entries = Vec::new();
        for row in rows {
            let (id, content, timestamp_str, embedding_json, metadata_json) = row
                .map_err(|e| OneAIError::Persistence(
                    format!("Failed to read STM entry row: {}", e)
                ))?;
            let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            let embedding = embedding_json.and_then(|json| deserialize_embedding(&json));
            let metadata = deserialize_metadata(&metadata_json);

            entries.push(MemoryEntry {
                id,
                content,
                timestamp,
                embedding,
                metadata,
            });
        }

        tracing::debug!("Loaded {} STM entries for session '{}'", entries.len(), session_id);
        Ok(entries)
    }

    async fn clear_stm(&self, session_id: &str) -> Result<()> {
        let conn = self.open_connection()?;
        conn.execute(
            "DELETE FROM stm_entries WHERE session_id = ?1",
            rusqlite::params![session_id],
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to clear STM for session '{}': {}", session_id, e)
        ))?;

        tracing::debug!("Cleared STM entries for session '{}'", session_id);
        Ok(())
    }

    // ─── LTM operations ───────────────────────────────────────────────

    async fn save_ltm(&self, entry: &MemoryEntry) -> Result<()> {
        let conn = self.open_connection()?;
        let embedding_json = serialize_embedding(&entry.embedding);
        let metadata_json = serialize_metadata(&entry.metadata);
        let timestamp = entry.timestamp.to_rfc3339();

        conn.execute(
            "INSERT OR REPLACE INTO ltm_entries (id, content, timestamp, embedding_json, metadata_json) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![entry.id, entry.content, timestamp, embedding_json, metadata_json],
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to save LTM entry '{}': {}", entry.id, e)
        ))?;

        tracing::debug!("Saved LTM entry '{}'", entry.id);
        Ok(())
    }

    async fn load_ltm(&self, id: &str) -> Result<Option<MemoryEntry>> {
        let conn = self.open_connection()?;
        let result = conn.query_row(
            "SELECT content, timestamp, embedding_json, metadata_json FROM ltm_entries WHERE id = ?1",
            rusqlite::params![id],
            |row| {
                let content: String = row.get(0)?;
                let timestamp_str: String = row.get(1)?;
                let embedding_json: Option<String> = row.get(2)?;
                let metadata_json: String = row.get(3)?;
                Ok((content, timestamp_str, embedding_json, metadata_json))
            },
        );

        match result {
            Ok((content, timestamp_str, embedding_json, metadata_json)) => {
                let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                let embedding = embedding_json.and_then(|json| deserialize_embedding(&json));
                let metadata = deserialize_metadata(&metadata_json);

                Ok(Some(MemoryEntry {
                    id: id.to_string(),
                    content,
                    timestamp,
                    embedding,
                    metadata,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(OneAIError::Persistence(
                format!("Failed to load LTM entry '{}': {}", id, e)
            )),
        }
    }

    async fn search_ltm_keyword(&self, keyword: &str, top_k: usize) -> Result<Vec<MemoryEntry>> {
        let conn = self.open_connection()?;

        // Use LIKE for case-insensitive keyword search
        let pattern = format!("%{}%", keyword);
        let mut stmt = conn.prepare(
            "SELECT id, content, timestamp, embedding_json, metadata_json \
             FROM ltm_entries WHERE content LIKE ?1 OR metadata_json LIKE ?1 \
             ORDER BY timestamp DESC LIMIT ?2"
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to prepare LTM keyword search: {}", e)
        ))?;

        let rows = stmt.query_map(rusqlite::params![pattern, top_k], |row| {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            let timestamp_str: String = row.get(2)?;
            let embedding_json: Option<String> = row.get(3)?;
            let metadata_json: String = row.get(4)?;
            Ok((id, content, timestamp_str, embedding_json, metadata_json))
        }).map_err(|e| OneAIError::Persistence(
            format!("Failed to execute LTM keyword search: {}", e)
        ))?;

        let mut entries = Vec::new();
        for row in rows {
            let (id, content, timestamp_str, embedding_json, metadata_json) = row
                .map_err(|e| OneAIError::Persistence(
                    format!("Failed to read LTM entry row: {}", e)
                ))?;
            let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            let embedding = embedding_json.and_then(|json| deserialize_embedding(&json));
            let metadata = deserialize_metadata(&metadata_json);

            entries.push(MemoryEntry {
                id,
                content,
                timestamp,
                embedding,
                metadata,
            });
        }

        tracing::debug!("Found {} LTM entries for keyword '{}'", entries.len(), keyword);
        Ok(entries)
    }

    async fn search_ltm_embedding(&self, query: &[f32], top_k: usize) -> Result<Vec<(MemoryEntry, f32)>> {
        let conn = self.open_connection()?;

        // Load all entries that have embeddings
        let mut stmt = conn.prepare(
            "SELECT id, content, timestamp, embedding_json, metadata_json \
             FROM ltm_entries WHERE embedding_json IS NOT NULL AND embedding_json != ''"
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to prepare LTM embedding search: {}", e)
        ))?;

        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            let timestamp_str: String = row.get(2)?;
            let embedding_json: Option<String> = row.get(3)?;
            let metadata_json: String = row.get(4)?;
            Ok((id, content, timestamp_str, embedding_json, metadata_json))
        }).map_err(|e| OneAIError::Persistence(
            format!("Failed to execute LTM embedding search: {}", e)
        ))?;

        // Compute cosine similarity for each entry
        let mut scored: Vec<(MemoryEntry, f32)> = Vec::new();
        for row in rows {
            let (id, content, timestamp_str, embedding_json, metadata_json) = row
                .map_err(|e| OneAIError::Persistence(
                    format!("Failed to read LTM entry row: {}", e)
                ))?;
            let entry_embedding = embedding_json.and_then(|json| deserialize_embedding(&json));
            if let Some(entry_vec) = &entry_embedding {
                let score = cosine_similarity(query, entry_vec);
                if score > 0.0 {
                    let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());
                    let metadata = deserialize_metadata(&metadata_json);

                    scored.push((MemoryEntry {
                        id,
                        content,
                        timestamp,
                        embedding: entry_embedding,
                        metadata,
                    }, score));
                }
            }
        }

        // Sort by similarity descending and take top_k
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);

        tracing::debug!("Found {} LTM entries by embedding (top {})", scored.len(), top_k);
        Ok(scored)
    }

    async fn delete_ltm(&self, id: &str) -> Result<()> {
        let conn = self.open_connection()?;
        conn.execute(
            "DELETE FROM ltm_entries WHERE id = ?1",
            rusqlite::params![id],
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to delete LTM entry '{}': {}", id, e)
        ))?;

        tracing::debug!("Deleted LTM entry '{}'", id);
        Ok(())
    }

    async fn clear_ltm(&self) -> Result<()> {
        let conn = self.open_connection()?;
        conn.execute("DELETE FROM ltm_entries", [])
            .map_err(|e| OneAIError::Persistence(
                format!("Failed to clear LTM entries: {}", e)
            ))?;

        tracing::debug!("Cleared all LTM entries");
        Ok(())
    }

    // ─── Conversation operations ──────────────────────────────────────

    async fn save_conversation(&self, id: &str, conversation: &Conversation) -> Result<()> {
        let conn = self.open_connection()?;
        let messages_json = serde_json::to_string(&conversation.messages)
            .map_err(|e| OneAIError::Persistence(
                format!("Failed to serialize conversation '{}': {}", id, e)
            ))?;
        let now = chrono::Utc::now().to_rfc3339();

        // Check if conversation already exists
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM conversations WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get::<_, i64>(0).map(|c| c > 0),
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to check conversation existence: {}", e)
        ))?;

        if exists {
            conn.execute(
                "UPDATE conversations SET messages_json = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, messages_json, now],
            ).map_err(|e| OneAIError::Persistence(
                format!("Failed to update conversation '{}': {}", id, e)
            ))?;
        } else {
            conn.execute(
                "INSERT INTO conversations (id, messages_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, messages_json, now, now],
            ).map_err(|e| OneAIError::Persistence(
                format!("Failed to insert conversation '{}': {}", id, e)
            ))?;
        }

        tracing::debug!("Saved conversation '{}' ({} messages)", id, conversation.messages.len());
        Ok(())
    }

    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>> {
        let conn = self.open_connection()?;
        let result = conn.query_row(
            "SELECT messages_json FROM conversations WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get::<_, String>(0),
        );

        match result {
            Ok(messages_json) => {
                let messages: Vec<oneai_core::Message> = serde_json::from_str(&messages_json)
                    .map_err(|e| OneAIError::Persistence(
                        format!("Failed to deserialize conversation '{}': {}", id, e)
                    ))?;
                let mut conversation = Conversation::with_id(id.to_string());
                conversation.messages = messages;
                Ok(Some(conversation))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(OneAIError::Persistence(
                format!("Failed to load conversation '{}': {}", id, e)
            )),
        }
    }

    async fn list_conversations(&self) -> Result<Vec<SessionInfo>> {
        let conn = self.open_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, created_at, updated_at, messages_json FROM conversations ORDER BY updated_at DESC"
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to prepare conversation list query: {}", e)
        ))?;

        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let created_at: String = row.get(1)?;
            let updated_at: String = row.get(2)?;
            let messages_json: String = row.get(3)?;
            // Count messages by parsing JSON
            let count = serde_json::from_str::<Vec<serde_json::Value>>(&messages_json)
                .map(|v| v.len())
                .unwrap_or(0);
            Ok((id, created_at, updated_at, count))
        }).map_err(|e| OneAIError::Persistence(
            format!("Failed to execute conversation list query: {}", e)
        ))?;

        let mut sessions = Vec::new();
        for row in rows {
            let (id, created_at_str, updated_at_str, message_count) = row
                .map_err(|e| OneAIError::Persistence(
                    format!("Failed to read conversation row: {}", e)
                ))?;
            let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());

            sessions.push(SessionInfo::new(id, created_at, updated_at, message_count));
        }

        tracing::debug!("Listed {} conversations", sessions.len());
        Ok(sessions)
    }

    async fn delete_conversation(&self, id: &str) -> Result<()> {
        let conn = self.open_connection()?;

        // Delete conversation and its STM entries
        conn.execute("DELETE FROM stm_entries WHERE session_id = ?1", rusqlite::params![id])
            .map_err(|e| OneAIError::Persistence(
                format!("Failed to delete STM entries for session '{}': {}", id, e)
            ))?;
        conn.execute("DELETE FROM conversations WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| OneAIError::Persistence(
                format!("Failed to delete conversation '{}': {}", id, e)
            ))?;

        tracing::debug!("Deleted conversation '{}' and its STM entries", id);
        Ok(())
    }

    // ─── MemoryFact persistence ──────────────────────────────────────────────

    async fn store_fact(&self, fact: &MemoryFact) -> Result<()> {
        let conn = self.open_connection()?;
        let embedding_json = fact.embedding.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default());
        let metadata_json = serde_json::to_string(&fact.metadata).unwrap_or_else(|_| "{}".to_string());
        let created = fact.created_at.to_rfc3339();
        let updated = fact.updated_at.to_rfc3339();

        // Conflict-resolved upsert: same (user_id, subject, predicate) → update
        // content/embedding/metadata/fact_type/updated_at and bump version,
        // preserving the original id/created_at. Mirrors the in-memory
        // MemoryFactStore's Mem0 invariant so persistence and runtime agree.
        conn.execute(
            "INSERT INTO memories (id, user_id, session_id, fact_type, subject, predicate, \
             content, embedding_json, metadata_json, created_at, updated_at, version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT(user_id, subject, predicate) DO UPDATE SET \
             content = excluded.content, \
             embedding_json = excluded.embedding_json, \
             metadata_json = excluded.metadata_json, \
             fact_type = excluded.fact_type, \
             updated_at = excluded.updated_at, \
             version = memories.version + 1",
            rusqlite::params![
                fact.id, fact.user_id, fact.session_id, fact.fact_type.as_str(),
                fact.subject, fact.predicate, fact.content, embedding_json, metadata_json,
                created, updated, fact.version,
            ],
        ).map_err(|e| OneAIError::Persistence(format!("Failed to store fact: {}", e)))?;
        Ok(())
    }

    async fn load_facts(&self, user_id: &str, session_id: &str) -> Result<Vec<MemoryFact>> {
        let conn = self.open_connection()?;
        // Empty session_id → all facts for the user (cross-session habits);
        // otherwise scope to that session.
        let mut stmt = conn.prepare(
            "SELECT id, user_id, session_id, fact_type, subject, predicate, content, \
             embedding_json, metadata_json, created_at, updated_at, version \
             FROM memories WHERE user_id = ?1 AND (?2 = '' OR session_id = ?2)"
        ).map_err(|e| OneAIError::Persistence(format!("Failed to prepare fact query: {}", e)))?;

        let rows = stmt.query_map(rusqlite::params![user_id, session_id], |row| {
            let embedding_json: Option<String> = row.get(7)?;
            let metadata_json: String = row.get(8)?;
            let embedding = embedding_json
                .and_then(|s| serde_json::from_str::<Vec<f32>>(&s).ok());
            let metadata: HashMap<String, String> = serde_json::from_str(&metadata_json)
                .unwrap_or_default();
            let created = chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(9)?)
                .map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_else(|_| chrono::Utc::now());
            let updated = chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(10)?)
                .map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_else(|_| chrono::Utc::now());
            Ok(MemoryFact {
                id: row.get(0)?,
                user_id: row.get(1)?,
                session_id: row.get(2)?,
                fact_type: oneai_core::FactType::new(row.get::<_, String>(3)?),
                subject: row.get(4)?,
                predicate: row.get(5)?,
                content: row.get(6)?,
                embedding,
                metadata,
                created_at: created,
                updated_at: updated,
                version: row.get(11)?,
            })
        }).map_err(|e| OneAIError::Persistence(format!("Failed to query facts: {}", e)))?;

        let mut facts = Vec::new();
        for row in rows {
            facts.push(row.map_err(|e| OneAIError::Persistence(format!("Failed to read fact row: {}", e)))?);
        }
        Ok(facts)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_entry(id: &str, content: &str, embedding: Option<Vec<f32>>) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            content: content.to_string(),
            timestamp: chrono::Utc::now(),
            embedding,
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        }
    }

    fn make_store() -> (SqliteSessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_oneai.db");
        let store = SqliteSessionStore::new(&db_path);
        (store, dir)
    }

    // ─── STM tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stm_save_load() {
        let (store, _dir) = make_store();
        let entries = vec![
            make_entry("stm1", "First message", None),
            make_entry("stm2", "Second message", Some(vec![0.1, 0.2, 0.3])),
        ];

        store.save_stm("session1", &entries).await.unwrap();
        let loaded = store.load_stm("session1").await.unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "stm1");
        assert_eq!(loaded[0].content, "First message");
        assert_eq!(loaded[1].id, "stm2");
        assert_eq!(loaded[1].embedding, Some(vec![0.1, 0.2, 0.3]));
    }

    #[tokio::test]
    async fn test_stm_overwrite() {
        let (store, _dir) = make_store();
        let entries1 = vec![make_entry("stm1", "First", None)];
        let entries2 = vec![
            make_entry("stm3", "Third", None),
            make_entry("stm4", "Fourth", None),
        ];

        store.save_stm("session1", &entries1).await.unwrap();
        store.save_stm("session1", &entries2).await.unwrap(); // Overwrites

        let loaded = store.load_stm("session1").await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "stm3");
    }

    #[tokio::test]
    async fn test_stm_clear() {
        let (store, _dir) = make_store();
        let entries = vec![make_entry("stm1", "First", None)];
        store.save_stm("session1", &entries).await.unwrap();

        store.clear_stm("session1").await.unwrap();
        let loaded = store.load_stm("session1").await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn test_stm_multiple_sessions() {
        let (store, _dir) = make_store();
        let entries_s1 = vec![make_entry("stm_s1", "Session 1 msg", None)];
        let entries_s2 = vec![make_entry("stm_s2", "Session 2 msg", None)];

        store.save_stm("session1", &entries_s1).await.unwrap();
        store.save_stm("session2", &entries_s2).await.unwrap();

        let s1 = store.load_stm("session1").await.unwrap();
        let s2 = store.load_stm("session2").await.unwrap();
        assert_eq!(s1.len(), 1);
        assert_eq!(s2.len(), 1);
        assert_eq!(s1[0].content, "Session 1 msg");
        assert_eq!(s2[0].content, "Session 2 msg");
    }

    // ─── LTM tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_ltm_save_load() {
        let (store, _dir) = make_store();
        let entry = make_entry("ltm1", "Rust programming language", Some(vec![0.1, 0.2, 0.3]));

        store.save_ltm(&entry).await.unwrap();
        let loaded = store.load_ltm("ltm1").await.unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, "ltm1");
        assert_eq!(loaded.content, "Rust programming language");
        assert_eq!(loaded.embedding, Some(vec![0.1, 0.2, 0.3]));
    }

    #[tokio::test]
    async fn test_ltm_load_nonexistent() {
        let (store, _dir) = make_store();
        let loaded = store.load_ltm("nonexistent").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_ltm_keyword_search() {
        let (store, _dir) = make_store();

        store.save_ltm(&make_entry("ltm1", "Rust programming language", None)).await.unwrap();
        store.save_ltm(&make_entry("ltm2", "Python programming language", None)).await.unwrap();
        store.save_ltm(&make_entry("ltm3", "The weather is sunny", None)).await.unwrap();

        let results = store.search_ltm_keyword("programming", 10).await.unwrap();
        assert_eq!(results.len(), 2);

        let results = store.search_ltm_keyword("rust", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "Rust programming language");
    }

    #[tokio::test]
    async fn test_ltm_embedding_search() {
        let (store, _dir) = make_store();

        store.save_ltm(&make_entry("ltm1", "Rust doc", Some(vec![0.1, 0.2, 0.3]))).await.unwrap();
        store.save_ltm(&make_entry("ltm2", "Python doc", Some(vec![0.4, 0.5, 0.6]))).await.unwrap();
        store.save_ltm(&make_entry("ltm3", "No embedding doc", None)).await.unwrap(); // No embedding

        let results = store.search_ltm_embedding(&[0.1, 0.2, 0.35], 2).await.unwrap();
        assert_eq!(results.len(), 2);
        // "Rust doc" should be most similar to the query
        assert!(results[0].0.content.contains("Rust"));
        assert!(results[0].1 > results[1].1); // Higher similarity score
    }

    #[tokio::test]
    async fn test_ltm_delete() {
        let (store, _dir) = make_store();
        store.save_ltm(&make_entry("ltm1", "Test content", None)).await.unwrap();

        store.delete_ltm("ltm1").await.unwrap();
        let loaded = store.load_ltm("ltm1").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_ltm_clear() {
        let (store, _dir) = make_store();
        store.save_ltm(&make_entry("ltm1", "First", None)).await.unwrap();
        store.save_ltm(&make_entry("ltm2", "Second", None)).await.unwrap();

        store.clear_ltm().await.unwrap();
        let results = store.search_ltm_keyword("First", 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_ltm_overwrite() {
        let (store, _dir) = make_store();
        store.save_ltm(&make_entry("ltm1", "Original content", None)).await.unwrap();
        store.save_ltm(&make_entry("ltm1", "Updated content", Some(vec![0.5]))).await.unwrap();

        let loaded = store.load_ltm("ltm1").await.unwrap().unwrap();
        assert_eq!(loaded.content, "Updated content");
        assert_eq!(loaded.embedding, Some(vec![0.5]));
    }

    // ─── Conversation tests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_conversation_save_load() {
        let (store, _dir) = make_store();
        let mut conv = Conversation::with_id("conv1".to_string());
        conv.add_message(oneai_core::Message::user("Hello".to_string()));
        conv.add_message(oneai_core::Message::assistant("Hi there".to_string()));

        store.save_conversation("conv1", &conv).await.unwrap();
        let loaded = store.load_conversation("conv1").await.unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].text_content(), "Hello");
    }

    #[tokio::test]
    async fn test_conversation_update() {
        let (store, _dir) = make_store();
        let mut conv = Conversation::with_id("conv1".to_string());
        conv.add_message(oneai_core::Message::user("First".to_string()));

        store.save_conversation("conv1", &conv).await.unwrap();

        conv.add_message(oneai_core::Message::assistant("Response".to_string()));
        store.save_conversation("conv1", &conv).await.unwrap();

        let loaded = store.load_conversation("conv1").await.unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    #[tokio::test]
    async fn test_conversation_list() {
        let (store, _dir) = make_store();

        let mut conv1 = Conversation::with_id("conv1".to_string());
        conv1.add_message(oneai_core::Message::user("Hello".to_string()));
        store.save_conversation("conv1", &conv1).await.unwrap();

        let mut conv2 = Conversation::with_id("conv2".to_string());
        conv2.add_message(oneai_core::Message::user("Hi".to_string()));
        conv2.add_message(oneai_core::Message::assistant("Hey".to_string()));
        store.save_conversation("conv2", &conv2).await.unwrap();

        let sessions = store.list_conversations().await.unwrap();
        assert_eq!(sessions.len(), 2);
        // Most recently updated should be first
        assert_eq!(sessions[0].message_count, 2);
    }

    #[tokio::test]
    async fn test_conversation_delete() {
        let (store, _dir) = make_store();

        // Save conversation + STM entries
        let mut conv = Conversation::with_id("conv1".to_string());
        conv.add_message(oneai_core::Message::user("Hello".to_string()));
        conv.add_message(oneai_core::Message::user("Hello".to_string()));
        store.save_conversation("conv1", &conv).await.unwrap();

        let stm = vec![make_entry("stm1", "STM entry", None)];
        store.save_stm("conv1", &stm).await.unwrap();

        // Delete — should remove both conversation and STM
        store.delete_conversation("conv1").await.unwrap();
        assert!(store.load_conversation("conv1").await.unwrap().is_none());
        assert!(store.load_stm("conv1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_conversation_load_nonexistent() {
        let (store, _dir) = make_store();
        let loaded = store.load_conversation("nonexistent").await.unwrap();
        assert!(loaded.is_none());
    }

    // ─── Persistence across restarts ──────────────────────────────────

    #[tokio::test]
    async fn test_persistence_across_restart_simulation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("persistent_test.db");

        // First "session" — save data
        let store1 = SqliteSessionStore::new(&db_path);
        let entry = make_entry("ltm_persist", "Important knowledge about Rust", Some(vec![0.1, 0.2]));
        store1.save_ltm(&entry).await.unwrap();

        let mut conv = Conversation::with_id("persist_conv".to_string());
        conv.add_message(oneai_core::Message::user("What is Rust?".to_string()));
        store1.save_conversation("persist_conv", &conv).await.unwrap();

        // Second "session" (simulates restart — new SqliteSessionStore instance)
        let store2 = SqliteSessionStore::new(&db_path);
        let loaded_ltm = store2.load_ltm("ltm_persist").await.unwrap().unwrap();
        assert_eq!(loaded_ltm.content, "Important knowledge about Rust");

        let loaded_conv = store2.load_conversation("persist_conv").await.unwrap().unwrap();
        assert_eq!(loaded_conv.messages[0].text_content(), "What is Rust?");
    }

    // ─── Embedding helpers ────────────────────────────────────────────

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 1.0);

        let c = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &c);
        assert!((sim - 0.0).abs() < 0.001);

        let d = vec![-1.0, 0.0, 0.0];
        let sim2 = cosine_similarity(&a, &d);
        assert!((sim2 - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn test_serialize_deserialize_embedding() {
        let embedding = Some(vec![0.1, 0.2, 0.3]);
        let json = serialize_embedding(&embedding);
        assert!(json.is_some());
        let parsed = deserialize_embedding(&json.unwrap());
        assert_eq!(parsed, embedding);

        let none: Option<Vec<f32>> = None;
        let json_none = serialize_embedding(&none);
        assert!(json_none.is_none());
    }

    #[test]
    fn test_serialize_deserialize_metadata() {
        let metadata = HashMap::from([
            ("role".to_string(), "user".to_string()),
            ("source".to_string(), "conversation".to_string()),
        ]);
        let json = serialize_metadata(&metadata);
        let parsed = deserialize_metadata(&json);
        assert_eq!(parsed, metadata);
    }
}

#[cfg(test)]
mod fact_tests {
    use super::*;
    use oneai_core::{FactType, MemoryFact};

    fn fact(id: &str, user: &str, sess: &str, subject: &str, content: &str, version: u32) -> MemoryFact {
        MemoryFact {
            id: id.to_string(),
            user_id: user.to_string(),
            session_id: sess.to_string(),
            fact_type: FactType::new("user_tooling_pref"),
            subject: subject.to_string(),
            predicate: "prefers".to_string(),
            content: content.to_string(),
            embedding: None,
            metadata: HashMap::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version,
        }
    }

    fn tmp_store() -> SqliteSessionStore {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oneai_fact_test_{}.db", uuid::Uuid::new_v4()));
        SqliteSessionStore::new(path)
    }

    #[tokio::test]
    async fn store_fact_upserts_on_conflict() {
        let s = tmp_store();
        s.store_fact(&fact("f1", "alice", "s1", "user.pm", "npm", 1)).await.unwrap();
        // Same key, new content → update, version bump (version field is ignored
        // on update path; DB bumps memories.version).
        s.store_fact(&fact("f1b", "alice", "s1", "user.pm", "pnpm", 1)).await.unwrap();
        let loaded = s.load_facts("alice", "s1").await.unwrap();
        assert_eq!(loaded.len(), 1); // not duplicated
        assert_eq!(loaded[0].content, "pnpm");
        assert_eq!(loaded[0].version, 2);
    }

    #[tokio::test]
    async fn load_facts_cross_session_for_user() {
        let s = tmp_store();
        s.store_fact(&fact("f1", "alice", "s1", "user.pm", "pnpm", 1)).await.unwrap();
        s.store_fact(&fact("f2", "alice", "s2", "user.runner", "vitest", 1)).await.unwrap();
        s.store_fact(&fact("f3", "bob", "s1", "user.pm", "npm", 1)).await.unwrap();
        // Empty session → all of alice's facts across sessions.
        let alice_all = s.load_facts("alice", "").await.unwrap();
        assert_eq!(alice_all.len(), 2);
        // Scoped to s1 only.
        let alice_s1 = s.load_facts("alice", "s1").await.unwrap();
        assert_eq!(alice_s1.len(), 1);
        assert_eq!(alice_s1[0].content, "pnpm");
        // Bob is separate.
        assert_eq!(s.load_facts("bob", "").await.unwrap().len(), 1);
    }
}
