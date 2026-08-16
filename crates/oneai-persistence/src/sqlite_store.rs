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

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::MemoryPersistence;
use oneai_core::{Conversation, MemoryEntry, MemoryFact, SessionInfo};

/// Folded "visible bubble" count for a slice of stored messages, mirroring
/// the chat-view render fold (Swift `rebuildEntries` / Android `loadSession`
/// / Windows `LoadSession`) and the live-streaming fold in
/// `ChatViewModel::handle`.
///
/// A single user turn often persists SEVERAL assistant messages — each
/// tool-call iteration's prelude ("Let me search…") is its own stored message,
/// plus a final-answer message. The chat view renders the whole turn as ONE
/// bubble (live streaming folds every `streamChunk`/`toolCall`/`toolResult`/
/// `directAnswer` of the run into one `AssistantItem`; reload folds the same).
/// Counting each stored assistant message therefore made the sidebar "N 条"
/// exceed the visible bubble count — issue #17:
/// "一轮中的多次输出也单独计算了".
///
/// Consecutive assistant messages with the same speaker (no intervening user
/// message) form one bubble. `tool`/`system` messages between assistants do
/// NOT break a group (they belong to the same turn); an empty-text assistant
/// (tool-call-only, no prelude) is part of the current group but adds no
/// bubble; a speaker change (group chat) or a user message starts a new group.
/// Only non-empty-text `user` messages and assistant groups containing text
/// are counted — matching the render filter that drops `system` / `tool` /
/// empty rows.
fn folded_display_count(msgs: &[oneai_core::Message]) -> usize {
    use oneai_core::Role;
    let mut count = 0usize;
    let mut group_open = false; // currently building an assistant bubble
    let mut group_speaker: Option<String> = None;

    for m in msgs {
        match m.role {
            Role::User => {
                if group_open {
                    count += 1; // close the open assistant bubble
                    group_open = false;
                }
                if !m.text_content().trim().is_empty() {
                    count += 1; // the user bubble itself
                }
                group_speaker = None;
            }
            Role::Assistant => {
                if m.text_content().trim().is_empty() {
                    // Tool-call-only assistant (no prelude text) — belongs to
                    // the current turn but renders no bubble of its own; do
                    // not break the group.
                    continue;
                }
                let speaker = m.metadata.get("speaker").cloned();
                if !group_open {
                    group_open = true;
                    group_speaker = speaker;
                } else if speaker != group_speaker {
                    // Speaker changed (group chat) — flush the previous bubble
                    // and start a new one.
                    count += 1;
                    group_speaker = speaker;
                }
                // else: same speaker, same turn — accumulate into the open
                // bubble (its text was already counted on open).
            }
            // `tool` / `system` / unknown — neither break the group nor count.
            _ => {}
        }
    }
    if group_open {
        count += 1; // close a trailing assistant bubble
    }
    count
}

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
        Self {
            db_path: db_path.into(),
        }
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
        let conn = rusqlite::Connection::open(&self.db_path).map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to open SQLite database at {}: {}",
                self.db_path.display(),
                e
            ))
        })?;

        // Multi-process concurrency guard. OneAI runs the TUI, the supervisor
        // daemon, and the gateway as *separate processes* that all touch the
        // same `~/.oneai/oneai.db`. Without WAL, the default rollback-journal
        // mode takes an exclusive lock on write → concurrent writers get
        // `database is locked` (and there's no busy wait by default, so the
        // first contended write fails immediately).
        //
        // - `journal_mode=WAL` lets readers proceed while a write is in flight
        //   and lets multiple processes hold the db concurrently (WAL is a
        //   persistent db-level setting, but setting it per-connection is a
        //   harmless no-op once in effect). `synchronous=NORMAL` is the
        //   recommended companion for WAL (durable across crashes, faster).
        // - `busy_timeout=5000ms` makes a contended writer *wait* rather than
        //   fail instantly — per-connection, must be set on every open.
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| OneAIError::Persistence(format!("set busy_timeout: {e}")))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| OneAIError::Persistence(format!("set WAL pragma: {e}")))?;

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
            CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);

            CREATE TABLE IF NOT EXISTS message_feedback (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                message_role TEXT NOT NULL,
                kind TEXT NOT NULL,
                text TEXT,
                created_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_feedback_session ON message_feedback(session_id);"
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
        let _ = conn.execute("ALTER TABLE memories ADD COLUMN superseded_at TEXT", []);
        // Core-memory pin flag (folds the old process-local pin set onto the
        // fact so pin state survives a restart + SQLite round-trip). Defaults
        // to 0 (not pinned) for legacy rows.
        let _ = conn.execute(
            "ALTER TABLE memories ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Same pattern for the `title` column on `conversations` (added for
        // session-list previews). Legacy dbs get the column added as NULL.
        let _ = conn.execute("ALTER TABLE conversations ADD COLUMN title TEXT", []);
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

    /// Record one per-message feedback entry (§W4 B2). Assigns `id` +
    /// `created_at_ms`; errors are mapped to `OneAIError::Persistence` so the
    /// `App` passthrough can `unwrap_or_default` them into a silent no-op
    /// (mirrors `list_conversations`' error-swallow contract). `text` is
    /// `Some` only for `note`-kind feedback.
    pub async fn record_feedback(
        &self,
        session_id: &str,
        turn_id: &str,
        message_role: &str,
        kind: &str,
        text: Option<&str>,
    ) {
        // Best-effort: a write failure (corrupt db, disk full) surfaces as a
        // logged no-op rather than a panic — feedback is non-critical UX state.
        if let Ok(conn) = self.open_connection() {
            let id = format!("fb-{}", uuid::Uuid::new_v4().simple());
            let created_at_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let _ = conn.execute(
                "INSERT INTO message_feedback \
                 (id, session_id, turn_id, message_role, kind, text, created_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    id,
                    session_id,
                    turn_id,
                    message_role,
                    kind,
                    text,
                    created_at_ms as i64,
                ],
            );
        }
    }

    /// All feedback entries for `session_id` (§W4 B2). A read failure surfaces
    /// as an empty list — never a panic — so a failing backend doesn't hide
    /// the conversation from the frontend.
    pub async fn list_feedback(&self, session_id: &str) -> Vec<oneai_core::FeedbackEntry> {
        let Ok(conn) = self.open_connection() else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, turn_id, message_role, kind, text, created_at_ms \
             FROM message_feedback WHERE session_id = ?1 ORDER BY created_at_ms ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            let text: Option<String> = row.get(5)?;
            let created_at_ms: i64 = row.get(6)?;
            Ok(oneai_core::FeedbackEntry {
                id: row.get(0)?,
                session_id: row.get(1)?,
                turn_id: row.get(2)?,
                message_role: row.get(3)?,
                kind: row.get(4)?,
                text,
                created_at_ms: created_at_ms as u64,
            })
        });
        match rows {
            Ok(rs) => rs.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }
}

// ─── Helper functions ───────────────────────────────────────────────────────

/// Serialize a MemoryEntry's embedding as JSON.
fn serialize_embedding(embedding: &Option<Vec<f32>>) -> Option<String> {
    embedding
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
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
    let first_user = conversation
        .messages
        .iter()
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
        let end = collapsed
            .char_indices()
            .nth(max)
            .map(|(i, _)| i)
            .unwrap_or(collapsed.len());
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
        )
        .map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to clear STM entries for session '{}': {}",
                session_id, e
            ))
        })?;

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

        tracing::debug!(
            "Saved {} STM entries for session '{}'",
            entries.len(),
            session_id
        );
        Ok(())
    }

    async fn load_stm(&self, session_id: &str) -> Result<Vec<MemoryEntry>> {
        let conn = self.open_connection()?;

        let mut stmt = conn
            .prepare(
                "SELECT id, content, timestamp, embedding_json, metadata_json \
             FROM stm_entries WHERE session_id = ?1 ORDER BY position ASC",
            )
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to prepare STM load query: {}", e))
            })?;

        let rows = stmt
            .query_map(rusqlite::params![session_id], |row| {
                let id: String = row.get(0)?;
                let content: String = row.get(1)?;
                let timestamp_str: String = row.get(2)?;
                let embedding_json: Option<String> = row.get(3)?;
                let metadata_json: String = row.get(4)?;
                Ok((id, content, timestamp_str, embedding_json, metadata_json))
            })
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to execute STM load query: {}", e))
            })?;

        let mut entries = Vec::new();
        for row in rows {
            let (id, content, timestamp_str, embedding_json, metadata_json) = row.map_err(|e| {
                OneAIError::Persistence(format!("Failed to read STM entry row: {}", e))
            })?;
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

        tracing::debug!(
            "Loaded {} STM entries for session '{}'",
            entries.len(),
            session_id
        );
        Ok(entries)
    }

    async fn clear_stm(&self, session_id: &str) -> Result<()> {
        let conn = self.open_connection()?;
        conn.execute(
            "DELETE FROM stm_entries WHERE session_id = ?1",
            rusqlite::params![session_id],
        )
        .map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to clear STM for session '{}': {}",
                session_id, e
            ))
        })?;

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
            Err(e) => Err(OneAIError::Persistence(format!(
                "Failed to load LTM entry '{}': {}",
                id, e
            ))),
        }
    }

    async fn search_ltm_keyword(&self, keyword: &str, top_k: usize) -> Result<Vec<MemoryEntry>> {
        let conn = self.open_connection()?;

        // Use LIKE for case-insensitive keyword search
        let pattern = format!("%{}%", keyword);
        let mut stmt = conn
            .prepare(
                "SELECT id, content, timestamp, embedding_json, metadata_json \
             FROM ltm_entries WHERE content LIKE ?1 OR metadata_json LIKE ?1 \
             ORDER BY timestamp DESC LIMIT ?2",
            )
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to prepare LTM keyword search: {}", e))
            })?;

        let rows = stmt
            .query_map(rusqlite::params![pattern, top_k], |row| {
                let id: String = row.get(0)?;
                let content: String = row.get(1)?;
                let timestamp_str: String = row.get(2)?;
                let embedding_json: Option<String> = row.get(3)?;
                let metadata_json: String = row.get(4)?;
                Ok((id, content, timestamp_str, embedding_json, metadata_json))
            })
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to execute LTM keyword search: {}", e))
            })?;

        let mut entries = Vec::new();
        for row in rows {
            let (id, content, timestamp_str, embedding_json, metadata_json) = row.map_err(|e| {
                OneAIError::Persistence(format!("Failed to read LTM entry row: {}", e))
            })?;
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

        tracing::debug!(
            "Found {} LTM entries for keyword '{}'",
            entries.len(),
            keyword
        );
        Ok(entries)
    }

    async fn search_ltm_embedding(
        &self,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<(MemoryEntry, f32)>> {
        let conn = self.open_connection()?;

        // Load all entries that have embeddings
        let mut stmt = conn
            .prepare(
                "SELECT id, content, timestamp, embedding_json, metadata_json \
             FROM ltm_entries WHERE embedding_json IS NOT NULL AND embedding_json != ''",
            )
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to prepare LTM embedding search: {}", e))
            })?;

        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let content: String = row.get(1)?;
                let timestamp_str: String = row.get(2)?;
                let embedding_json: Option<String> = row.get(3)?;
                let metadata_json: String = row.get(4)?;
                Ok((id, content, timestamp_str, embedding_json, metadata_json))
            })
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to execute LTM embedding search: {}", e))
            })?;

        // Compute cosine similarity for each entry
        let mut scored: Vec<(MemoryEntry, f32)> = Vec::new();
        for row in rows {
            let (id, content, timestamp_str, embedding_json, metadata_json) = row.map_err(|e| {
                OneAIError::Persistence(format!("Failed to read LTM entry row: {}", e))
            })?;
            let entry_embedding = embedding_json.and_then(|json| deserialize_embedding(&json));
            if let Some(entry_vec) = &entry_embedding {
                let score = cosine_similarity(query, entry_vec);
                if score > 0.0 {
                    let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());
                    let metadata = deserialize_metadata(&metadata_json);

                    scored.push((
                        MemoryEntry {
                            id,
                            content,
                            timestamp,
                            embedding: entry_embedding,
                            metadata,
                        },
                        score,
                    ));
                }
            }
        }

        // Sort by similarity descending and take top_k
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);

        tracing::debug!(
            "Found {} LTM entries by embedding (top {})",
            scored.len(),
            top_k
        );
        Ok(scored)
    }

    async fn delete_ltm(&self, id: &str) -> Result<()> {
        let conn = self.open_connection()?;
        conn.execute(
            "DELETE FROM ltm_entries WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| {
            OneAIError::Persistence(format!("Failed to delete LTM entry '{}': {}", id, e))
        })?;

        tracing::debug!("Deleted LTM entry '{}'", id);
        Ok(())
    }

    async fn clear_ltm(&self) -> Result<()> {
        let conn = self.open_connection()?;
        conn.execute("DELETE FROM ltm_entries", [])
            .map_err(|e| OneAIError::Persistence(format!("Failed to clear LTM entries: {}", e)))?;

        tracing::debug!("Cleared all LTM entries");
        Ok(())
    }

    // ─── Conversation operations ──────────────────────────────────────

    async fn save_conversation(&self, id: &str, conversation: &Conversation) -> Result<()> {
        let conn = self.open_connection()?;
        let messages_json = serde_json::to_string(&conversation.messages).map_err(|e| {
            OneAIError::Persistence(format!("Failed to serialize conversation '{}': {}", id, e))
        })?;
        let metadata_json = serialize_metadata(&conversation.metadata);
        let now = chrono::Utc::now().to_rfc3339();
        let title = conversation_title(conversation, 80);

        // Check if conversation already exists
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get::<_, i64>(0).map(|c| c > 0),
            )
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to check conversation existence: {}", e))
            })?;

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

        tracing::debug!(
            "Saved conversation '{}' ({} messages)",
            id,
            conversation.messages.len()
        );
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
                    .map_err(|e| {
                        OneAIError::Persistence(format!(
                            "Failed to deserialize conversation '{}': {}",
                            id, e
                        ))
                    })?;
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
                if conversation
                    .metadata
                    .get("title")
                    .map(|s| s.is_empty())
                    .unwrap_or(true)
                {
                    if let Some(t) = title {
                        if !t.is_empty() {
                            conversation.metadata.insert("title".to_string(), t);
                        }
                    }
                }
                Ok(Some(conversation))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(OneAIError::Persistence(format!(
                "Failed to load conversation '{}': {}",
                id, e
            ))),
        }
    }

    /// Load a session's discarded-prefix archive snapshots, ordered
    /// oldest-first by creation time. Each row's `messages_json` is parsed
    /// into a `Conversation` (messages only — archive segments carry no
    /// title/metadata worth restoring). Returns empty when SQLite
    /// persistence is not enabled or no snapshots exist. The order is the
    /// chronological compression order, which `full_transcript_messages`
    /// relies on to reconstruct a linear history.
    async fn load_discarded_snapshots(&self, session_id: &str) -> Result<Vec<Conversation>> {
        let conn = self.open_connection()?;
        let pat = format!("{}{}%", session_id, oneai_core::DISCARDED_SNAPSHOT_MARKER);
        let mut stmt = conn
            .prepare(
                "SELECT id, messages_json FROM conversations \
                 WHERE id LIKE ?1 \
                 ORDER BY created_at ASC",
            )
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to prepare discarded-snapshot query: {}",
                    e
                ))
            })?;

        let rows = stmt
            .query_map(rusqlite::params![pat], |row| {
                let id: String = row.get(0)?;
                let messages_json: String = row.get(1)?;
                Ok((id, messages_json))
            })
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to query discarded snapshots for '{}': {}",
                    session_id, e
                ))
            })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, messages_json) = row.map_err(|e| {
                OneAIError::Persistence(format!("Failed to read discarded-snapshot row: {}", e))
            })?;
            let messages: Vec<oneai_core::Message> =
                serde_json::from_str(&messages_json).map_err(|e| {
                    OneAIError::Persistence(format!(
                        "Failed to deserialize discarded snapshot '{}': {}",
                        id, e
                    ))
                })?;
            let mut conv = Conversation::with_id(id);
            conv.messages = messages;
            out.push(conv);
        }
        Ok(out)
    }

    /// Cheap per-snapshot non-`system` message count, oldest-first. No
    /// `messages_json` content is materialized — `json_each` counts in SQLite.
    async fn snapshot_display_counts(&self, session_id: &str) -> Result<Vec<(String, u32)>> {
        let conn = self.open_connection()?;
        let pat = format!("{}{}%", session_id, oneai_core::DISCARDED_SNAPSHOT_MARKER);
        let mut stmt = conn
            .prepare(
                "SELECT s.id, (SELECT count(*) FROM json_each(s.messages_json) \
                 WHERE json_extract(value, '$.role') != 'system') \
                 FROM conversations s WHERE s.id LIKE ?1 \
                 ORDER BY s.created_at ASC",
            )
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to prepare snapshot-count query: {}", e))
            })?;
        let rows = stmt
            .query_map(rusqlite::params![pat], |row| {
                let id: String = row.get(0)?;
                let count: i64 = row.get(1).unwrap_or(0);
                Ok((id, count as u32))
            })
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to query snapshot counts for '{}': {}",
                    session_id, e
                ))
            })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| {
                OneAIError::Persistence(format!("Failed to read snapshot-count row: {}", e))
            })?);
        }
        Ok(out)
    }

    async fn list_conversations(&self) -> Result<Vec<SessionInfo>> {
        let conn = self.open_connection()?;
        // Sidebar "N 条" must equal the number of bubbles the chat view
        // renders. A single user turn often spans several STORED assistant
        // messages — each tool-call iteration's prelude ("Let me search…") is
        // persisted as its own assistant message, plus a final-answer
        // message. The streaming UI folds all of them into ONE bubble per
        // turn (one `AssistantItem` accumulates every `streamChunk` /
        // `toolCall` / `toolResult` / `directAnswer` of the run — see
        // `ChatViewModel::handle`); the reload path folds the same way (Swift
        // `rebuildEntries`, Android `loadSession`, Windows `LoadSession`).
        // Counting each stored assistant message made the sidebar diverge
        // from the visible bubble count — issue #17:
        // "会话显示的消息条数错误，多于实际对话数，可能是将一轮中的
        //  多次输出也单独计算了".
        //
        // We therefore count in Rust by folding consecutive assistant
        // messages (same speaker, no intervening user) into one group via
        // `folded_display_count` — mirroring the render fold. (The prior
        // pure-SQL `json_each` count counted each assistant message
        // individually.)
        //
        // As in the #14 fix, a session's `messages_json` only holds the LIVE
        // (post-compression) tail; older turns live in discarded-prefix
        // snapshots (`{id}{DISCARDED_SNAPSHOT_MARKER}{uuid}` rows). The
        // displayed transcript merges live + snapshots, so the count must sum
        // the folded counts of the live row AND its snapshots.
        //
        // NOTE: this display count is distinct from `snapshot_display_counts`
        // (raw non-`system` message count) and `transcript_total` — both feed
        // paging offset arithmetic and must stay raw to align with message
        // positions; only the sidebar uses the folded count.
        let mut stmt = conn
            .prepare(
                "SELECT id, created_at, updated_at, title, messages_json, metadata_json \
                 FROM conversations ORDER BY updated_at DESC",
            )
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to prepare conversation list query: {}", e))
            })?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let created_at: String = row.get(1)?;
                let updated_at: String = row.get(2)?;
                let title: Option<String> = row.get(3)?;
                let messages_json: String = row.get(4)?;
                let metadata_json: Option<String> = row.get(5)?;
                Ok((
                    id,
                    created_at,
                    updated_at,
                    title,
                    messages_json,
                    metadata_json,
                ))
            })
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to execute conversation list query: {}", e))
            })?;

        // Bucket discarded-prefix snapshots under their parent session id.
        // A snapshot id is `{parent}{MARKER}{uuid}`; the parent is the prefix
        // before the first marker (session ids are uuids and never contain it).
        let mut snapshots: HashMap<String, Vec<Vec<oneai_core::Message>>> = HashMap::new();
        // Preserve SQL order (updated_at DESC) for the returned list.
        let mut tops: Vec<(
            String,
            String,
            String,
            Option<String>,
            usize,
            Option<String>,
        )> = Vec::new();

        for row in rows {
            let (id, created_at_str, updated_at_str, title, messages_json, metadata_json) = row
                .map_err(|e| {
                    OneAIError::Persistence(format!("Failed to read conversation row: {}", e))
                })?;
            // Tolerate legacy/corrupt blobs: an unparseable row contributes
            // 0 rather than hiding every conversation from the sidebar.
            let msgs: Vec<oneai_core::Message> =
                serde_json::from_str(&messages_json).unwrap_or_default();
            // Derive the workspace label the frontend groups by, from the
            // persisted `conversation.metadata["workspace"]` (set at
            // session/create). Tolerate missing/legacy/corrupt metadata.
            let workspace = metadata_json
                .as_deref()
                .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
                .and_then(|v| v.get("workspace").cloned())
                .and_then(|v| v.as_str().map(|s| s.to_string()));
            if id.contains(oneai_core::DISCARDED_SNAPSHOT_MARKER) {
                if let Some(parent) = id.split(oneai_core::DISCARDED_SNAPSHOT_MARKER).next() {
                    snapshots.entry(parent.to_string()).or_default().push(msgs);
                }
            } else {
                let count = folded_display_count(&msgs);
                tops.push((id, created_at_str, updated_at_str, title, count, workspace));
            }
        }

        let mut sessions = Vec::with_capacity(tops.len());
        for (id, created_at_str, updated_at_str, title, mut count, workspace) in tops {
            if let Some(children) = snapshots.get(&id) {
                for child in children {
                    count += folded_display_count(child);
                }
            }
            let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            sessions.push(
                SessionInfo::with_title(id, created_at, updated_at, count, title)
                    .with_workspace(workspace),
            );
        }

        tracing::debug!("Listed {} conversations", sessions.len());
        Ok(sessions)
    }

    async fn delete_conversation(&self, id: &str) -> Result<()> {
        let conn = self.open_connection()?;
        let discard_prefix = format!("{}{}%", id, oneai_core::DISCARDED_SNAPSHOT_MARKER);

        // Delete STM entries for the session and any of its discarded snapshots.
        conn.execute(
            "DELETE FROM stm_entries WHERE session_id = ?1 OR session_id LIKE ?2",
            rusqlite::params![id, discard_prefix],
        )
        .map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to delete STM entries for session '{}': {}",
                id, e
            ))
        })?;
        // Delete the conversation row and cascade-delete its discarded-prefix
        // archive snapshots (`{id}{DISCARDED_SNAPSHOT_MARKER}{uuid}`) so they
        // don't outlive the parent chat and leak as orphan rows.
        conn.execute(
            "DELETE FROM conversations WHERE id = ?1 OR id LIKE ?2",
            rusqlite::params![id, discard_prefix],
        )
        .map_err(|e| {
            OneAIError::Persistence(format!("Failed to delete conversation '{}': {}", id, e))
        })?;

        tracing::debug!("Deleted conversation '{}' and its STM entries", id);
        Ok(())
    }

    // ─── MemoryFact persistence ──────────────────────────────────────────────

    async fn store_fact(&self, fact: &MemoryFact) -> Result<()> {
        let conn = self.open_connection()?;
        let embedding_json = fact
            .embedding
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let metadata_json =
            serde_json::to_string(&fact.metadata).unwrap_or_else(|_| "{}".to_string());
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
                fact.id,
                fact.user_id,
                fact.session_id,
                fact.fact_type.as_str(),
                fact.subject,
                fact.predicate,
                fact.content,
                embedding_json,
                metadata_json,
                created,
                updated,
                fact.version,
                fact.importance,
                fact.superseded,
                superseded_at,
                fact.pinned,
            ],
        )
        .map_err(|e| OneAIError::Persistence(format!("Failed to store fact: {}", e)))?;
        Ok(())
    }

    async fn load_facts(&self, user_id: &str, session_id: &str) -> Result<Vec<MemoryFact>> {
        let conn = self.open_connection()?;
        // Empty session_id → all facts for the user (cross-session habits);
        // otherwise scope to that session.
        let mut stmt = conn
            .prepare(
                "SELECT id, user_id, session_id, fact_type, subject, predicate, content, \
             embedding_json, metadata_json, created_at, updated_at, version, importance, \
             superseded, superseded_at, pinned \
             FROM memories WHERE user_id = ?1 AND (?2 = '' OR session_id = ?2)",
            )
            .map_err(|e| OneAIError::Persistence(format!("Failed to prepare fact query: {}", e)))?;

        let rows = stmt
            .query_map(rusqlite::params![user_id, session_id], |row| {
                let embedding_json: Option<String> = row.get(7)?;
                let metadata_json: String = row.get(8)?;
                let embedding =
                    embedding_json.and_then(|s| serde_json::from_str::<Vec<f32>>(&s).ok());
                let metadata: HashMap<String, String> =
                    serde_json::from_str(&metadata_json).unwrap_or_default();
                let created = chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(9)?)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                let updated = chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(10)?)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                let superseded: i64 = row.get(13)?;
                let superseded_at = row
                    .get::<_, Option<String>>(14)?
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
            })
            .map_err(|e| OneAIError::Persistence(format!("Failed to query facts: {}", e)))?;

        let mut facts = Vec::new();
        for row in rows {
            facts.push(
                row.map_err(|e| {
                    OneAIError::Persistence(format!("Failed to read fact row: {}", e))
                })?,
            );
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

    #[tokio::test]
    async fn wal_mode_enabled_for_concurrent_processes() {
        // Regression for the TUI/gateway DB-lock bug: the store must open in
        // WAL mode so separate processes can write the same oneai.db.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wal_test.db");
        let store = SqliteSessionStore::new(&db_path);
        // Trigger open_connection so the WAL pragma is applied.
        store.save_stm("s0", &[]).await.unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn concurrent_writes_across_stores_do_not_busy() {
        // Two independent stores (separate connection opens) over the same
        // file — mirrors the TUI + gateway two-process case. Pre-WAL the
        // second writer failed with SQLITE_BUSY; WAL + busy_timeout serializes
        // the writers instead of rejecting the second.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("conc.db");
        let entries_a = vec![make_entry("e_a", "x", None)];
        let entries_b = vec![make_entry("e_b", "y", None)];
        let store_a = SqliteSessionStore::new(&db_path);
        let store_b = SqliteSessionStore::new(&db_path);
        let (ra, rb) = tokio::join!(
            store_a.save_stm("sess_a", &entries_a),
            store_b.save_stm("sess_b", &entries_b),
        );
        ra.unwrap();
        rb.unwrap();
        assert_eq!(store_a.load_stm("sess_a").await.unwrap().len(), 1);
        assert_eq!(store_b.load_stm("sess_b").await.unwrap().len(), 1);
    }

    // ─── STM tests ────────────────────────────────────────────────────────────

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
        let entry = make_entry(
            "ltm1",
            "Rust programming language",
            Some(vec![0.1, 0.2, 0.3]),
        );

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

        store
            .save_ltm(&make_entry("ltm1", "Rust programming language", None))
            .await
            .unwrap();
        store
            .save_ltm(&make_entry("ltm2", "Python programming language", None))
            .await
            .unwrap();
        store
            .save_ltm(&make_entry("ltm3", "The weather is sunny", None))
            .await
            .unwrap();

        let results = store.search_ltm_keyword("programming", 10).await.unwrap();
        assert_eq!(results.len(), 2);

        let results = store.search_ltm_keyword("rust", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "Rust programming language");
    }

    #[tokio::test]
    async fn test_ltm_embedding_search() {
        let (store, _dir) = make_store();

        store
            .save_ltm(&make_entry("ltm1", "Rust doc", Some(vec![0.1, 0.2, 0.3])))
            .await
            .unwrap();
        store
            .save_ltm(&make_entry("ltm2", "Python doc", Some(vec![0.4, 0.5, 0.6])))
            .await
            .unwrap();
        store
            .save_ltm(&make_entry("ltm3", "No embedding doc", None))
            .await
            .unwrap(); // No embedding

        let results = store
            .search_ltm_embedding(&[0.1, 0.2, 0.35], 2)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        // "Rust doc" should be most similar to the query
        assert!(results[0].0.content.contains("Rust"));
        assert!(results[0].1 > results[1].1); // Higher similarity score
    }

    #[tokio::test]
    async fn test_ltm_delete() {
        let (store, _dir) = make_store();
        store
            .save_ltm(&make_entry("ltm1", "Test content", None))
            .await
            .unwrap();

        store.delete_ltm("ltm1").await.unwrap();
        let loaded = store.load_ltm("ltm1").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_ltm_clear() {
        let (store, _dir) = make_store();
        store
            .save_ltm(&make_entry("ltm1", "First", None))
            .await
            .unwrap();
        store
            .save_ltm(&make_entry("ltm2", "Second", None))
            .await
            .unwrap();

        store.clear_ltm().await.unwrap();
        let results = store.search_ltm_keyword("First", 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_ltm_overwrite() {
        let (store, _dir) = make_store();
        store
            .save_ltm(&make_entry("ltm1", "Original content", None))
            .await
            .unwrap();
        store
            .save_ltm(&make_entry("ltm1", "Updated content", Some(vec![0.5])))
            .await
            .unwrap();

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
    async fn test_conversation_list_folds_multi_output_round() {
        // Regression for issue #17: a single user turn persists several
        // assistant messages (tool-call preludes + final answer, with tool
        // results between). The chat view folds the whole turn into ONE
        // assistant bubble, so the sidebar "N 条" must say 2 (1 user + 1
        // assistant), not 4 (1 user + 3 assistant messages).
        use std::collections::HashMap;
        let (store, _dir) = make_store();
        let mk_asst_with_call = |text: &str, call_id: &str| oneai_core::Message {
            role: oneai_core::Role::Assistant,
            content: vec![
                oneai_core::ContentBlock::Text {
                    text: text.to_string(),
                },
                oneai_core::ContentBlock::ToolCall {
                    id: call_id.to_string(),
                    name: "search".to_string(),
                    args: "{}".to_string(),
                },
            ],
            metadata: HashMap::new(),
        };
        let mk_asst_empty_call = |call_id: &str| oneai_core::Message {
            role: oneai_core::Role::Assistant,
            content: vec![oneai_core::ContentBlock::ToolCall {
                id: call_id.to_string(),
                name: "search".to_string(),
                args: "{}".to_string(),
            }],
            metadata: HashMap::new(),
        };

        // Round 1: user + two text preludes (with tool calls) + final answer,
        // tool results between. Renders as 1 user + 1 assistant bubble = 2.
        let mut conv = Conversation::with_id("sess17".to_string());
        conv.add_message(oneai_core::Message::user("please summarize X".to_string()));
        conv.add_message(mk_asst_with_call("Let me look that up.", "c1"));
        conv.add_message(oneai_core::Message::tool_result("c1".into(), "...".into()));
        conv.add_message(mk_asst_with_call("Checking another source.", "c2"));
        conv.add_message(oneai_core::Message::tool_result("c2".into(), "...".into()));
        conv.add_message(oneai_core::Message::assistant(
            "Here is the summary.".to_string(),
        ));
        // Round 2: a tool-call-only (empty-text) prelude before the final
        // answer — must not open an extra bubble.
        conv.add_message(oneai_core::Message::user("and Y?".to_string()));
        conv.add_message(mk_asst_empty_call("c3"));
        conv.add_message(oneai_core::Message::tool_result("c3".into(), "...".into()));
        conv.add_message(oneai_core::Message::assistant("Y summary".to_string()));
        store.save_conversation("sess17", &conv).await.unwrap();

        let sessions = store.list_conversations().await.unwrap();
        assert_eq!(sessions.len(), 1);
        // 2 rounds × (1 user + 1 folded assistant) = 4, NOT the 8 stored
        // assistant/user messages.
        assert_eq!(
            sessions[0].message_count, 4,
            "one round's multiple assistant outputs must fold into one bubble"
        );

        // Direct unit check of the fold helper.
        assert_eq!(folded_display_count(&conv.messages), 4);
    }

    #[test]
    fn folded_display_count_groups_by_speaker() {
        // Group chat: same-speaker consecutive assistants merge; a speaker
        // change opens a new bubble (mirrors live `ChatViewModel::handle`
        // creating a new `AssistantItem` on speaker change).
        use std::collections::HashMap;
        let asst = |speaker: Option<&str>, text: &str| oneai_core::Message {
            role: oneai_core::Role::Assistant,
            content: vec![oneai_core::ContentBlock::Text {
                text: text.to_string(),
            }],
            metadata: match speaker {
                Some(s) => HashMap::from([("speaker".to_string(), s.to_string())]),
                None => HashMap::new(),
            },
        };
        // user, A, B, A  → 1 user + 3 assistant bubbles = 4
        let msgs = vec![
            oneai_core::Message::user("topic".to_string()),
            asst(Some("A"), "idea"),
            asst(Some("B"), "critique"),
            asst(Some("A"), "revise"),
        ];
        assert_eq!(folded_display_count(&msgs), 4);

        // user, A, A, A (same speaker) → 1 user + 1 assistant bubble = 2
        let msgs2 = vec![
            oneai_core::Message::user("topic".to_string()),
            asst(Some("A"), "p1"),
            asst(Some("A"), "p2"),
            asst(Some("A"), "p3"),
        ];
        assert_eq!(folded_display_count(&msgs2), 2);
    }

    #[tokio::test]
    async fn test_conversation_list_includes_discarded_snapshot_counts() {
        // Regression for issue #14: after context compression, a session's
        // `messages_json` holds only the live tail; the discarded prefix is
        // archived as a `{id}::discarded::{uuid}` snapshot row. The sidebar
        // count must sum the live bubbles AND the snapshot bubbles so the
        // "N 条" matches the bubbles the transcript view renders (live +
        // snapshots merged in `transcript_page`).
        let (store, _dir) = make_store();

        // Live tail: 2 bubbles (user + assistant). Plus a leading system
        // message and an empty-text assistant tool-call turn — both must be
        // excluded from the count (render filter).
        let mut conv = Conversation::with_id("sess1".to_string());
        conv.add_message(oneai_core::Message::system("sys prompt".to_string()));
        conv.add_message(oneai_core::Message::user("latest question".to_string()));
        conv.add_message(oneai_core::Message::assistant("latest answer".to_string()));
        // Empty-text assistant (tool-call-only) turn — must NOT count.
        conv.add_message(oneai_core::Message {
            role: oneai_core::Role::Assistant,
            content: vec![oneai_core::ContentBlock::ToolCall {
                id: "tc1".into(),
                name: "search".into(),
                args: "{}".into(),
            }],
            metadata: Default::default(),
        });
        store.save_conversation("sess1", &conv).await.unwrap();

        // Two discarded snapshots (older turns), oldest-first by created_at.
        let mk_snap = |suffix: &str, msgs: Vec<oneai_core::Message>| {
            let id = format!("sess1{}{}", oneai_core::DISCARDED_SNAPSHOT_MARKER, suffix);
            let mut s = Conversation::with_id(id);
            for m in msgs {
                s.add_message(m);
            }
            s
        };
        store
            .save_conversation(
                &format!("sess1{}u1", oneai_core::DISCARDED_SNAPSHOT_MARKER),
                &mk_snap(
                    "u1",
                    vec![
                        oneai_core::Message::user("old q1".to_string()),
                        oneai_core::Message::assistant("old a1".to_string()),
                        oneai_core::Message::user("old q2".to_string()),
                        oneai_core::Message::assistant("old a2".to_string()),
                    ],
                ),
            )
            .await
            .unwrap();
        store
            .save_conversation(
                &format!("sess1{}u2", oneai_core::DISCARDED_SNAPSHOT_MARKER),
                &mk_snap(
                    "u2",
                    vec![
                        oneai_core::Message::user("mid q".to_string()),
                        oneai_core::Message::assistant("mid a".to_string()),
                    ],
                ),
            )
            .await
            .unwrap();

        let sessions = store.list_conversations().await.unwrap();
        // The two discarded snapshots must NOT appear as their own sessions.
        assert_eq!(sessions.len(), 1);
        // 2 (live bubbles) + 4 (snap u1) + 2 (snap u2) = 8. The system
        // message and empty-text assistant turn are excluded.
        assert_eq!(sessions[0].message_count, 8);
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
        conv.add_message(oneai_core::Message::user(
            "How do I parse JSON in Rust?".to_string(),
        ));
        conv.add_message(oneai_core::Message::assistant(
            "Use serde_json…".to_string(),
        ));
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
        let long = "line one\nline two   with\ttabs and  many     spaces "
            .to_string()
            .repeat(20); // well over 80 chars, with embedded newlines/runs
        let mut conv = Conversation::with_id("c".to_string());
        conv.add_message(oneai_core::Message::user(long));
        store.save_conversation("c", &conv).await.unwrap();

        let title = store.list_conversations().await.unwrap()[0]
            .title
            .clone()
            .unwrap();
        assert!(
            !title.contains('\n'),
            "newlines must be collapsed: {title:?}"
        );
        assert!(
            !title.contains("  "),
            "whitespace runs must be collapsed: {title:?}"
        );
        assert!(
            title.ends_with('…'),
            "long title must be truncated with ellipsis: {title:?}"
        );
        // Truncation targets 80 chars + ellipsis.
        assert!(
            title.chars().count() <= 81,
            "title too long: {} chars",
            title.chars().count()
        );
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
            )
            .unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let msgs =
                serde_json::to_string(&vec![oneai_core::Message::user("legacy".to_string())])
                    .unwrap();
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
        let legacy = sessions
            .iter()
            .find(|s| s.id == "legacy_conv")
            .expect("legacy row present");
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

    #[tokio::test]
    async fn test_discarded_snapshot_hidden_from_list_and_cascade_deleted() {
        // Regression: context compression archives the summarized-away prefix
        // as a conversation row whose id is `{session}{DISCARDED_SNAPSHOT_MARKER}{uuid}`
        // (see MemoryManager::archive_discarded_snapshot). These rows are
        // internal archive artifacts, NOT user-facing sessions. Two contracts:
        //  1. list_conversations MUST hide them — otherwise every compression
        //     on a long chat spawns a phantom "new session" in the sidebar
        //     showing the discarded prefix (an early user turn), which the user
        //     reads as a brand-new conversation that stole their first question.
        //  2. delete_conversation MUST cascade-delete a session's discarded
        //     snapshots so they don't leak as orphans after the parent chat is
        //     deleted.
        let (store, _dir) = make_store();

        // The live conversation.
        let mut conv = Conversation::with_id("sessA".to_string());
        conv.add_message(oneai_core::Message::user(
            "我需要面试Android Framework".to_string(),
        ));
        conv.add_message(oneai_core::Message::assistant("好的,我来介绍".to_string()));
        store.save_conversation("sessA", &conv).await.unwrap();

        // Two discarded-prefix snapshots, as the compressor would write them.
        let snap1 = format!("sessA{}{}", oneai_core::DISCARDED_SNAPSHOT_MARKER, "u1");
        let snap2 = format!("sessA{}{}", oneai_core::DISCARDED_SNAPSHOT_MARKER, "u2");
        let mut disc = Conversation::with_id(snap1.clone());
        disc.add_message(oneai_core::Message::user("进入下一个主题吧".to_string()));
        store.save_conversation(&snap1, &disc).await.unwrap();
        store.save_conversation(&snap2, &disc).await.unwrap();

        // (1) Hidden from the listing — only the live session shows.
        let sessions = store.list_conversations().await.unwrap();
        assert_eq!(
            sessions.len(),
            1,
            "discarded snapshots must not appear in list_conversations"
        );
        assert_eq!(sessions[0].id, "sessA");

        // `load_conversation` still resolves a discarded snapshot by exact id
        // (audit / memory_search fallback path must keep working).
        assert!(store.load_conversation(&snap1).await.unwrap().is_some());

        // (2) Cascade-delete — removing the parent also removes its snapshots.
        store.delete_conversation("sessA").await.unwrap();
        assert!(store.load_conversation("sessA").await.unwrap().is_none());
        assert!(store.load_conversation(&snap1).await.unwrap().is_none());
        assert!(store.load_conversation(&snap2).await.unwrap().is_none());
        assert!(store.list_conversations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_load_discarded_snapshots_returns_owning_session_ordered() {
        // `load_discarded_snapshots(sid)` must return ONLY this session's
        // snapshots (not another session's), oldest-first by created_at — the
        // merge in `full_transcript_messages` relies on chronological order to
        // rebuild a linear transcript.
        let (store, _dir) = make_store();

        // Another session's snapshot must not leak in.
        let mut other =
            Conversation::with_id(format!("other{}", oneai_core::DISCARDED_SNAPSHOT_MARKER) + "x");
        other.add_message(oneai_core::Message::user("other session".to_string()));
        store.save_conversation(&other.id, &other).await.unwrap();

        // Save two snapshots for sessA in order, with distinguishable content.
        let mk = |suffix: &str, text: &str| {
            let id = format!("sessA{}{}", oneai_core::DISCARDED_SNAPSHOT_MARKER, suffix);
            let mut c = Conversation::with_id(id);
            c.add_message(oneai_core::Message::assistant(text.to_string()));
            c
        };
        let s1 = mk("u1", "first segment");
        let s2 = mk("u2", "second segment");
        store.save_conversation(&s1.id, &s1).await.unwrap();
        store.save_conversation(&s2.id, &s2).await.unwrap();

        let loaded = store.load_discarded_snapshots("sessA").await.unwrap();
        assert_eq!(loaded.len(), 2, "only sessA's snapshots, not other's");
        // Oldest-first: u1 before u2.
        assert!(loaded[0].id.contains("u1"));
        assert!(loaded[1].id.contains("u2"));
        // Messages round-tripped intact.
        assert_eq!(loaded[0].messages.len(), 1);
        assert!(loaded[0].messages[0]
            .text_content()
            .contains("first segment"));
    }

    #[tokio::test]
    async fn test_snapshot_display_counts_excludes_system_and_orders_oldest_first() {
        // `snapshot_display_counts` returns only the non-`system` message count
        // per snapshot (so pagination can size segments without loading
        // content), oldest-first. System messages (base prompt + per-compression
        // summaries) must NOT be counted.
        let (store, _dir) = make_store();

        let mk = |suffix: &str, msgs: &[(oneai_core::Role, &str)]| {
            let id = format!("sess{}{}", oneai_core::DISCARDED_SNAPSHOT_MARKER, suffix);
            let mut c = Conversation::with_id(id);
            for (role, text) in msgs {
                c.add_message(oneai_core::Message::text(*role, *text));
            }
            c
        };
        use oneai_core::Role;
        // snap1: system + user + assistant  → 2 display msgs
        let s1 = mk(
            "u1",
            &[
                (Role::System, "base"),
                (Role::User, "q1"),
                (Role::Assistant, "a1"),
            ],
        );
        // snap2: system + user  → 1 display msg
        let s2 = mk("u2", &[(Role::System, "summary"), (Role::User, "q2")]);
        store.save_conversation(&s1.id, &s1).await.unwrap();
        store.save_conversation(&s2.id, &s2).await.unwrap();

        let counts = store.snapshot_display_counts("sess").await.unwrap();
        assert_eq!(counts.len(), 2);
        assert!(counts[0].0.contains("u1"), "oldest-first");
        assert!(counts[1].0.contains("u2"));
        assert_eq!(counts[0].1, 2, "system excluded");
        assert_eq!(counts[1].1, 1, "system excluded");
    }

    // ─── Persistence across restarts ──────────────────────────────────

    #[tokio::test]
    async fn test_persistence_across_restart_simulation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("persistent_test.db");

        // First "session" — save data
        let store1 = SqliteSessionStore::new(&db_path);
        let entry = make_entry(
            "ltm_persist",
            "Important knowledge about Rust",
            Some(vec![0.1, 0.2]),
        );
        store1.save_ltm(&entry).await.unwrap();

        let mut conv = Conversation::with_id("persist_conv".to_string());
        conv.add_message(oneai_core::Message::user("What is Rust?".to_string()));
        store1
            .save_conversation("persist_conv", &conv)
            .await
            .unwrap();

        // Second "session" (simulates restart — new SqliteSessionStore instance)
        let store2 = SqliteSessionStore::new(&db_path);
        let loaded_ltm = store2.load_ltm("ltm_persist").await.unwrap().unwrap();
        assert_eq!(loaded_ltm.content, "Important knowledge about Rust");

        let loaded_conv = store2
            .load_conversation("persist_conv")
            .await
            .unwrap()
            .unwrap();
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

    fn fact(
        id: &str,
        user: &str,
        sess: &str,
        subject: &str,
        content: &str,
        version: u32,
    ) -> MemoryFact {
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
        s.store_fact(&fact("f1", "alice", "s1", "user.pm", "npm", 1))
            .await
            .unwrap();
        // Same key, new content → update, version bump (version field is ignored
        // on update path; DB bumps memories.version).
        s.store_fact(&fact("f1b", "alice", "s1", "user.pm", "pnpm", 1))
            .await
            .unwrap();
        let loaded = s.load_facts("alice", "s1").await.unwrap();
        assert_eq!(loaded.len(), 1); // not duplicated
        assert_eq!(loaded[0].content, "pnpm");
        assert_eq!(loaded[0].version, 2);
    }

    #[tokio::test]
    async fn load_facts_cross_session_for_user() {
        let s = tmp_store();
        s.store_fact(&fact("f1", "alice", "s1", "user.pm", "pnpm", 1))
            .await
            .unwrap();
        s.store_fact(&fact("f2", "alice", "s2", "user.runner", "vitest", 1))
            .await
            .unwrap();
        s.store_fact(&fact("f3", "bob", "s1", "user.pm", "npm", 1))
            .await
            .unwrap();
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
        s.store_fact(&fact("f2", "alice", "s1", "user.runner", "vitest", 1))
            .await
            .unwrap();

        let loaded = s.load_facts("alice", "s1").await.unwrap();
        let pm = loaded.iter().find(|f| f.subject == "user.pm").unwrap();
        let runner = loaded.iter().find(|f| f.subject == "user.runner").unwrap();
        assert!(pm.pinned, "pinned flag lost across SQLite round-trip");
        assert!(!runner.pinned, "non-pinned flag flipped true");
    }

    /// §W4 B2 — per-message feedback round-trips through SQLite and is scoped
    /// per-session (the `feedback/list` query must not leak across sessions).
    #[tokio::test]
    async fn feedback_record_then_list_round_trips_and_scopes_by_session() {
        let s = tmp_store();
        s.record_feedback("s1", "t1", "assistant", "up", None).await;
        s.record_feedback("s1", "t2", "assistant", "note", Some("nice"))
            .await;
        s.record_feedback("s2", "t9", "assistant", "down", None)
            .await;

        let s1 = s.list_feedback("s1").await;
        assert_eq!(s1.len(), 2, "two entries for s1");
        assert!(s1.iter().any(|e| e.turn_id == "t1" && e.kind == "up"));
        assert!(s1
            .iter()
            .any(|e| e.turn_id == "t2" && e.text.as_deref() == Some("nice")));
        assert_eq!(s.list_feedback("s2").await.len(), 1);
        assert!(s.list_feedback("s3").await.is_empty());
    }
}
