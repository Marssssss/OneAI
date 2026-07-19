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
//! - One database file for all tables (sessions / STM / LTM / usage)

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
                updated_at TEXT NOT NULL,
                title TEXT,
                metadata_json TEXT
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
                version INTEGER NOT NULL DEFAULT 1,
                importance REAL NOT NULL DEFAULT 0.5,
                superseded INTEGER NOT NULL DEFAULT 0,
                superseded_at TEXT,
                pinned INTEGER NOT NULL DEFAULT 0
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_key ON memories(user_id, subject, predicate);
            CREATE INDEX IF NOT EXISTS idx_memories_user ON memories(user_id);
            CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);"
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to create session store schema: {}", e)
        ))?;

        // Best-effort migration for databases created before the `importance`
        // column existed. `ALTER TABLE ... ADD COLUMN` errors if the column is
        // already present; ignore that specific case so both fresh and legacy
        // databases end up with the column.
        let _ = conn.execute(
            "ALTER TABLE memories ADD COLUMN importance REAL NOT NULL DEFAULT 0.5",
            [],
        );
        // Soft-invalidation columns for the Mem0/Zep-style supersede path
        // (§12.2). `superseded` defaults to 0 (false); `superseded_at` is NULL
        // while the fact is the current truth.
        let _ = conn.execute(
            "ALTER TABLE memories ADD COLUMN superseded INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE memories ADD COLUMN superseded_at TEXT",
            [],
        );
        // Core-memory pin flag (folds the old process-local pin set onto the
        // fact so pin state survives a restart + SQLite round-trip). Defaults
        // to 0 (not pinned) for legacy rows.
        let _ = conn.execute(
            "ALTER TABLE memories ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Same pattern for the `title` column on `conversations` (added for
        // session-list previews). Legacy dbs get the column added as NULL.
        let _ = conn.execute(
            "ALTER TABLE conversations ADD COLUMN title TEXT",
            [],
        );
        // And the `metadata_json` column, added so a resumed conversation
        // retains its metadata — notably `metadata["title"]` set by group-chat
        // scenarios (e.g. "面试演练·前端工程师"). Without it, resume drops the
        // title and the next save falls back to first-user-message derivation,
        // clobbering the scenario name.
        let _ = conn.execute(
            "ALTER TABLE conversations ADD COLUMN metadata_json TEXT",
            [],
        );

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

/// Derive a short title from a conversation's first user message: take the
/// first `User` message's text content, collapse runs of whitespace into
/// single spaces, and truncate to `max` chars (appending "…" when truncated).
/// Returns `None` when the conversation has no user message. Used as the
/// `conversations.title` column so `list_conversations` can label rows without
/// loading full histories.
fn conversation_title(conversation: &Conversation, max: usize) -> Option<String> {
    // An explicit title (set e.g. by group-chat scenarios as
    // `metadata["title"] = "面试演练·前端工程师"`) wins over the default
    // first-user-message derivation — group chats rarely carry a user message
    // for the opener turn, so without this they fall back to "新对话".
    if let Some(title) = conversation.metadata.get("title") {
        let normalized = normalize_title(title, max);
        if normalized.is_empty() {
            return None;
        }
        return Some(normalized);
    }
    let first_user = conversation.messages.iter()
        .find(|m| matches!(m.role, oneai_core::Role::User))?;
    let text = first_user.text_content();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(normalize_title(trimmed, max))
}

/// Collapse any run of whitespace (incl. newlines) into a single space, then
/// truncate to `max` chars on a char boundary (appending an ellipsis).
fn normalize_title(text: &str, max: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        // Truncate on a char boundary to avoid splitting a multi-byte char.
        let end = collapsed.char_indices().nth(max).map(|(i, _)| i).unwrap_or(collapsed.len());
        format!("{}…", &collapsed[..end])
    }
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
        let metadata_json = serialize_metadata(&conversation.metadata);
        let now = chrono::Utc::now().to_rfc3339();
        let title = conversation_title(conversation, 80);

        // Check if conversation already exists
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM conversations WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get::<_, i64>(0).map(|c| c > 0),
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to check conversation existence: {}", e)
        ))?;

        if exists {
            // Recompute the title on update too — the first user message could
            // have changed (e.g. history rewritten by a compact). The metadata
            // (which may carry an explicit `title`) is persisted verbatim so a
            // resumed session keeps it; `conversation_title` still honors it.
            conn.execute(
                "UPDATE conversations SET messages_json = ?2, metadata_json = ?3, updated_at = ?4, title = ?5 WHERE id = ?1",
                rusqlite::params![id, messages_json, metadata_json, now, title],
            ).map_err(|e| OneAIError::Persistence(
                format!("Failed to update conversation '{}': {}", id, e)
            ))?;
        } else {
            conn.execute(
                "INSERT INTO conversations (id, messages_json, metadata_json, created_at, updated_at, title) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![id, messages_json, metadata_json, now, now, title],
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
            "SELECT messages_json, metadata_json, title FROM conversations WHERE id = ?1",
            rusqlite::params![id],
            |row| {
                let messages_json: String = row.get(0)?;
                // Legacy rows (pre-metadata_json column) return NULL → default "{}".
                let metadata_json: Option<String> = row.get(1).ok();
                let title: Option<String> = row.get(2).ok();
                Ok((messages_json, metadata_json, title))
            },
        );

        match result {
            Ok((messages_json, metadata_json, title)) => {
                let messages: Vec<oneai_core::Message> = serde_json::from_str(&messages_json)
                    .map_err(|e| OneAIError::Persistence(
                        format!("Failed to deserialize conversation '{}': {}", id, e)
                    ))?;
                let mut conversation = Conversation::with_id(id.to_string());
                conversation.messages = messages;
                // Restore metadata so a resumed session keeps its title
                // (and any other conversation-level metadata) across the next
                // save — otherwise `conversation_title` falls back to the
                // first-user-message derivation and clobbers the scenario name.
                if let Some(json) = metadata_json {
                    if !json.is_empty() {
                        conversation.metadata = deserialize_metadata(&json);
                    }
                }
                // Legacy fallback: rows saved before metadata_json existed have
                // no metadata, so the title column is the only record of the
                // scenario name. Promote it into metadata["title"] so the next
                // save preserves it instead of re-deriving from the first user
                // message (which would clobber "面试演练·前端工程师").
                if conversation.metadata.get("title").map(|s| s.is_empty()).unwrap_or(true) {
                    if let Some(t) = title {
                        if !t.is_empty() {
                            conversation
                                .metadata
                                .insert("title".to_string(), t);
                        }
                    }
                }
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
            "SELECT id, created_at, updated_at, messages_json, title FROM conversations ORDER BY updated_at DESC"
        ).map_err(|e| OneAIError::Persistence(
            format!("Failed to prepare conversation list query: {}", e)
        ))?;

        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let created_at: String = row.get(1)?;
            let updated_at: String = row.get(2)?;
            let messages_json: String = row.get(3)?;
            let title: Option<String> = row.get(4)?;
            // Count messages by parsing JSON
            let count = serde_json::from_str::<Vec<serde_json::Value>>(&messages_json)
                .map(|v| v.len())
                .unwrap_or(0);
            Ok((id, created_at, updated_at, count, title))
        }).map_err(|e| OneAIError::Persistence(
            format!("Failed to execute conversation list query: {}", e)
        ))?;

        let mut sessions = Vec::new();
        for row in rows {
            let (id, created_at_str, updated_at_str, message_count, title) = row
                .map_err(|e| OneAIError::Persistence(
                    format!("Failed to read conversation row: {}", e)
                ))?;
            let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());

            sessions.push(SessionInfo::with_title(id, created_at, updated_at, message_count, title));
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
        let superseded_at = fact.superseded_at.map(|t| t.to_rfc3339());

        // Conflict-resolved upsert: same (user_id, subject, predicate) → update
        // content/embedding/metadata/fact_type/updated_at and bump version,
        // preserving the original id/created_at. Mirrors the in-memory
        // MemoryFactStore's Mem0 invariant so persistence and runtime agree.
        // `superseded`/`superseded_at` flow through so a soft-invalidated fact
        // stays invalidated across resume (and a fresh write un-sets them).
        conn.execute(
            "INSERT INTO memories (id, user_id, session_id, fact_type, subject, predicate, \
             content, embedding_json, metadata_json, created_at, updated_at, version, importance, \
             superseded, superseded_at, pinned) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
             ON CONFLICT(user_id, subject, predicate) DO UPDATE SET \
             content = excluded.content, \
             embedding_json = excluded.embedding_json, \
             metadata_json = excluded.metadata_json, \
             fact_type = excluded.fact_type, \
             updated_at = excluded.updated_at, \
             version = memories.version + 1, \
             importance = excluded.importance, \
             superseded = excluded.superseded, \
             superseded_at = excluded.superseded_at, \
             pinned = excluded.pinned",
            rusqlite::params![
                fact.id, fact.user_id, fact.session_id, fact.fact_type.as_str(),
                fact.subject, fact.predicate, fact.content, embedding_json, metadata_json,
                created, updated, fact.version, fact.importance,
                fact.superseded, superseded_at, fact.pinned,
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
             embedding_json, metadata_json, created_at, updated_at, version, importance, \
             superseded, superseded_at, pinned \
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
            let superseded: i64 = row.get(13)?;
            let superseded_at = row.get::<_, Option<String>>(14)?
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|d| d.with_timezone(&chrono::Utc));
            let pinned: i64 = row.get(15)?;
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
                importance: row.get::<_, f64>(12)? as f32,
                created_at: created,
                updated_at: updated,
                version: row.get(11)?,
                superseded: superseded != 0,
                superseded_at,
                pinned: pinned != 0,
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
    async fn test_conversation_title_prefers_metadata_title() {
        // Group-chat scenarios set metadata["title"]; it must win over the
        // first-user-message derivation (which would be None for an opener-only
        // transcript → "新对话" in the UI).
        let (store, _dir) = make_store();

        let mut conv = Conversation::with_id("conv1".to_string());
        conv.metadata
            .insert("title".to_string(), "面试演练·前端工程师".to_string());
        // No user message — the default derivation would yield None.
        conv.add_message(oneai_core::Message::assistant("开场白".to_string()));
        store.save_conversation("conv1", &conv).await.unwrap();

        let sessions = store.list_conversations().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("面试演练·前端工程师"),
            "metadata.title must override the first-user-message derivation",
        );
    }

    #[tokio::test]
    async fn test_conversation_title_survives_resume_and_resave() {
        // Regression: a group-chat session is saved with metadata["title"].
        // It is then resumed (loaded) as a fresh conversation, a new user
        // message is appended, and it is re-saved. The scenario title must be
        // preserved — previously `load_conversation` dropped the metadata, so
        // the re-save fell back to the first-user-message derivation and
        // clobbered "面试演练·前端工程师" with the user's first answer.
        let (store, _dir) = make_store();

        let mut conv = Conversation::with_id("conv1".to_string());
        conv.metadata
            .insert("title".to_string(), "面试演练·前端工程师".to_string());
        conv.add_message(oneai_core::Message::assistant("开场白".to_string()));
        store.save_conversation("conv1", &conv).await.unwrap();

        // Resume → metadata (incl. title) must round-trip.
        let mut resumed = store.load_conversation("conv1").await.unwrap().unwrap();
        assert_eq!(
            resumed.metadata.get("title").map(|s| s.as_str()),
            Some("面试演练·前端工程师"),
            "metadata.title must be restored on load",
        );

        // Simulate the user coming back and sending a new message: append +
        // re-save (now via the single-agent path, which has no metadata of its
        // own). The title must NOT regress to the first user message.
        resumed.add_message(oneai_core::Message::user("我的自我介绍是…".to_string()));
        resumed.add_message(oneai_core::Message::assistant("好的".to_string()));
        store.save_conversation("conv1", &resumed).await.unwrap();

        let sessions = store.list_conversations().await.unwrap();
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("面试演练·前端工程师"),
            "resumed scenario title must survive a re-save, not be clobbered",
        );
    }

    #[tokio::test]
    async fn test_conversation_title_from_first_user_message() {
        let (store, _dir) = make_store();

        let mut conv = Conversation::with_id("conv1".to_string());
        conv.add_message(oneai_core::Message::system("system prompt".to_string()));
        conv.add_message(oneai_core::Message::user("How do I parse JSON in Rust?".to_string()));
        conv.add_message(oneai_core::Message::assistant("Use serde_json…".to_string()));
        store.save_conversation("conv1", &conv).await.unwrap();

        let sessions = store.list_conversations().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("How do I parse JSON in Rust?"),
            "title must be the first user message",
        );
    }

    #[tokio::test]
    async fn test_conversation_title_collapses_and_truncates() {
        let (store, _dir) = make_store();
        let long = "line one\nline two   with\ttabs and  many     spaces ".to_string()
            .repeat(20); // well over 80 chars, with embedded newlines/runs
        let mut conv = Conversation::with_id("c".to_string());
        conv.add_message(oneai_core::Message::user(long));
        store.save_conversation("c", &conv).await.unwrap();

        let title = store.list_conversations().await.unwrap()[0].title.clone().unwrap();
        assert!(!title.contains('\n'), "newlines must be collapsed: {title:?}");
        assert!(!title.contains("  "), "whitespace runs must be collapsed: {title:?}");
        assert!(title.ends_with('…'), "long title must be truncated with ellipsis: {title:?}");
        // Truncation targets 80 chars + ellipsis.
        assert!(title.chars().count() <= 81, "title too long: {} chars", title.chars().count());
    }

    #[tokio::test]
    async fn test_conversation_title_none_without_user_message() {
        let (store, _dir) = make_store();
        let mut conv = Conversation::with_id("c".to_string());
        conv.add_message(oneai_core::Message::assistant("hi".to_string()));
        store.save_conversation("c", &conv).await.unwrap();

        let sessions = store.list_conversations().await.unwrap();
        assert_eq!(sessions[0].title, None, "no user message → no title");
    }

    #[tokio::test]
    async fn test_conversation_title_migration_from_legacy_db() {
        // A legacy db (pre-title-column) has conversations without the `title`
        // column. Opening it must add the column via ALTER TABLE, and listing
        // must return title=None for the pre-existing row instead of erroring.
        let (store, dir) = make_store();
        // Build a legacy-style row by inserting via a raw connection that lacks
        // the title column, simulating an old database.
        let db_path = store.db_path().clone();
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            // Create the OLD schema (no title column) and insert a row.
            conn.execute_batch(
                "CREATE TABLE conversations (
                    id TEXT PRIMARY KEY,
                    messages_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_conv_updated ON conversations(updated_at);",
            ).unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let msgs = serde_json::to_string(&vec![oneai_core::Message::user("legacy".to_string())]).unwrap();
            conn.execute(
                "INSERT INTO conversations (id, messages_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["legacy_conv", msgs, now, now],
            ).unwrap();
        }
        // `store` was constructed over the same path; its open_connection runs
        // the ALTER migration. Re-list through the store. (`_dir` keeps the
        // tempdir alive for the duration of the test.)
        let _ = &dir;
        let sessions = store.list_conversations().await.unwrap();
        let legacy = sessions.iter().find(|s| s.id == "legacy_conv").expect("legacy row present");
        assert_eq!(legacy.title, None, "legacy row has no title until re-saved");
        assert_eq!(legacy.message_count, 1);
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
            importance: 0.5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version,
            superseded: false,
            superseded_at: None,
            pinned: false,
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

    /// The `pinned` flag (folded from CoreMemory's old process-local pin set
    /// onto the fact itself) must survive a SQLite round-trip — that's the
    /// whole point of moving pin state off the in-memory Vec: a pinned fact
    /// stays pinned across a restart.
    #[tokio::test]
    async fn pinned_flag_survives_sqlite_roundtrip() {
        let s = tmp_store();
        let mut pinned = fact("f1", "alice", "s1", "user.pm", "pnpm", 1);
        pinned.pinned = true;
        s.store_fact(&pinned).await.unwrap();
        // A non-pinned sibling for contrast.
        s.store_fact(&fact("f2", "alice", "s1", "user.runner", "vitest", 1)).await.unwrap();

        let loaded = s.load_facts("alice", "s1").await.unwrap();
        let pm = loaded.iter().find(|f| f.subject == "user.pm").unwrap();
        let runner = loaded.iter().find(|f| f.subject == "user.runner").unwrap();
        assert!(pm.pinned, "pinned flag lost across SQLite round-trip");
        assert!(!runner.pinned, "non-pinned flag flipped true");
    }
}
