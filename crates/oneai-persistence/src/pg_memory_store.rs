//! Postgres-backed memory store (MVS3-B — storage externalization, see
//! `docs/cloud-orchestrator-design.md` §6/§7 and `docs/memory-mechanism.md`).
//!
//! Drop-in replacement for [`crate::SqliteSessionStore`] behind the same
//! `MemoryPersistence` trait, for cloud deployments where N session
//! containers share one Postgres instead of N per-session Docker volumes:
//! memory (STM/LTM/facts) and conversation history survive container loss
//! entirely and are queryable across sessions.
//!
//! ## pgvector — HARD dependency
//! LTM embedding search runs server-side KNN (`ORDER BY embedding <=> $1`)
//! instead of the SQLite backend's in-Rust brute-force cosine over
//! JSON-serialized embeddings. `ensure_schema` therefore runs
//! `CREATE EXTENSION IF NOT EXISTS vector`; if the server lacks pgvector the
//! connect fails loudly and the CLI selection layer degrades to SQLite with a
//! warning (same contract as `PgWorkingStateStore`'s fail-fast schema). The
//! dev/acceptance Postgres must be a pgvector image
//! (`pgvector/pgvector:pg16`).
//!
//! ## Schema notes
//! - Tables carry a `_pg` suffix so a Pg database that also hosts other
//!   OneAI tables (working_state_*, usage_records_pg, …) stays unambiguous.
//! - Embeddings use an **unspecified-dimension `vector` column**: embedding
//!   models (and therefore dims) vary per provider/config, and one shared
//!   cloud DB may serve sessions using different models. Trade-off: no
//!   HNSW/IVFFlat index (those require fixed dims) — KNN is an exact scan,
//!   the same complexity class as the SQLite brute force it replaces, but
//!   without shipping every embedding to the client. If scale demands it,
//!   operators can partition by dim and add per-partition HNSW indexes.
//!   `search_ltm_embedding` filters `vector_dims(embedding) = vector_dims($1)`
//!   so rows written by a different model never poison (or error) a query —
//!   mirroring the SQLite backend's dimension-mismatch → score 0 behavior.
//! - Timestamps are `TIMESTAMPTZ` bound directly from `chrono::DateTime<Utc>`
//!   (the SQLite backend stores RFC3339 TEXT). The two backends are mutually
//!   exclusive per engine instance — there is no cross-read.
//! - JSONB columns are bound via the `$n::text::jsonb` double-cast trick and
//!   read back as `::text` (no serde_json feature needed on the driver), same
//!   as `PgWorkingStateStore`.
//! - `search_ltm_keyword` uses `ILIKE` — Postgres `LIKE` is case-sensitive
//!   while SQLite `LIKE` is case-insensitive for ASCII; ILIKE restores the
//!   SQLite semantics (and extends them to non-ASCII).
//!
//! ## Feature gate + selection
//! Compiled only under `--features postgres` (default off). At runtime the
//! CLI selects this backend when `ONEAI_PG_DSN` is set (see
//! `examples/cli/src/pg_backends.rs`).

use std::collections::HashMap;

use async_trait::async_trait;
use deadpool_postgres::Pool;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::MemoryPersistence;
use oneai_core::{Conversation, MemoryEntry, MemoryFact, SessionInfo};
use pgvector::Vector;

use crate::pg_common::{
    build_pool, ensure_schema as common_ensure_schema, pg_err, pool_err, ADVISORY_LOCK_BASE,
};
use crate::sqlite_store::{
    deserialize_metadata, first_user_message_title, folded_display_count, normalize_title,
    serialize_metadata,
};

/// Catalog probe: TRUE when every object this store needs already exists
/// (catalog-only reads — no relation locks on the steady-state boot path).
/// The pgvector extension is NOT probed: it is a prerequisite of the `vector`
/// columns, so if the tables exist the extension exists (an operator dropping
/// it afterwards breaks the columns outright — out of scope).
const SCHEMA_EXISTS_SQL: &str = "SELECT to_regclass('stm_entries_pg') IS NOT NULL \
     AND to_regclass('ltm_entries_pg') IS NOT NULL \
     AND to_regclass('conversations_pg') IS NOT NULL \
     AND to_regclass('memories_pg') IS NOT NULL \
     AND to_regclass('idx_memories_pg_key') IS NOT NULL";

/// DDL applied idempotently on first use (`CREATE ... IF NOT EXISTS` — no
/// migration framework; additive changes go through new statements). The
/// `CREATE EXTENSION` runs under the store's advisory lock (see pg_common).
const SCHEMA_DDL: &str = r#"
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS stm_entries_pg (
    id            TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL,
    content       TEXT NOT NULL,
    timestamp     TIMESTAMPTZ NOT NULL,
    embedding     vector,
    metadata_json JSONB NOT NULL DEFAULT '{}',
    position      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_stm_pg_session ON stm_entries_pg(session_id, position);

CREATE TABLE IF NOT EXISTS ltm_entries_pg (
    id            TEXT PRIMARY KEY,
    content       TEXT NOT NULL,
    timestamp     TIMESTAMPTZ NOT NULL,
    embedding     vector,
    metadata_json JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_ltm_pg_timestamp ON ltm_entries_pg(timestamp);

CREATE TABLE IF NOT EXISTS conversations_pg (
    id            TEXT PRIMARY KEY,
    messages_json JSONB NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL,
    title         TEXT,
    metadata_json JSONB
);
CREATE INDEX IF NOT EXISTS idx_conv_pg_updated ON conversations_pg(updated_at);
CREATE INDEX IF NOT EXISTS idx_conv_pg_created ON conversations_pg(created_at);

CREATE TABLE IF NOT EXISTS memories_pg (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL,
    session_id    TEXT NOT NULL,
    fact_type     TEXT NOT NULL,
    subject       TEXT NOT NULL,
    predicate     TEXT NOT NULL,
    content       TEXT NOT NULL,
    embedding     vector,
    metadata_json JSONB NOT NULL DEFAULT '{}',
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL,
    version       BIGINT NOT NULL DEFAULT 1,
    importance    DOUBLE PRECISION NOT NULL DEFAULT 0.5,
    superseded    BOOLEAN NOT NULL DEFAULT FALSE,
    superseded_at TIMESTAMPTZ,
    pinned        BOOLEAN NOT NULL DEFAULT FALSE,
    CONSTRAINT idx_memories_pg_key UNIQUE (user_id, subject, predicate)
);
CREATE INDEX IF NOT EXISTS idx_memories_pg_user ON memories_pg(user_id);
CREATE INDEX IF NOT EXISTS idx_memories_pg_session ON memories_pg(session_id);
"#;

/// This store's advisory-lock key (see the registry in `pg_common`).
const LOCK_KEY: i64 = ADVISORY_LOCK_BASE + 1;

/// Postgres-backed memory store. See the module docs for the contract.
pub struct PgMemoryStore {
    pool: Pool,
    /// DDL applied at most once per store instance (the DDL itself is
    /// idempotent, so concurrent stores across processes are safe).
    schema_ready: tokio::sync::OnceCell<()>,
}

impl PgMemoryStore {
    /// Connect to `dsn` (a libpq connection string) and build a pooled store.
    /// Fails fast when the server is unreachable or lacks pgvector.
    pub async fn connect(dsn: &str) -> Result<Self> {
        Self::connect_with_pool_size(dsn, 8).await
    }

    /// Like [`connect`](Self::connect) with an explicit pool size.
    pub async fn connect_with_pool_size(dsn: &str, max_size: usize) -> Result<Self> {
        let pool = build_pool(dsn, max_size)?;
        let store = Self::new(pool);
        // Fail fast: apply the schema DDL (incl. CREATE EXTENSION vector) NOW
        // so "backend selected" means the tables exist — a missing pgvector
        // surfaces at startup as a loud CLI warning + SQLite fallback instead
        // of mid-session on the first save.
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an externally built pool (tests / embedding apps that own the
    /// pool lifetime). Schema DDL runs lazily on the first operation.
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            schema_ready: tokio::sync::OnceCell::new(),
        }
    }

    /// The underlying pool (read-only introspection / ops tooling).
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Apply the schema DDL (idempotent, at most once per store instance).
    pub async fn ensure_schema(&self) -> Result<()> {
        common_ensure_schema(
            &self.pool,
            &self.schema_ready,
            LOCK_KEY,
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

    /// Read a conversation's `metadata_json` as a map (missing/legacy/corrupt
    /// metadata degrades to an empty map). Errors when no row matches `id` —
    /// rename/archive surface that as "session not found", mirroring
    /// `SqliteSessionStore::read_metadata_map`.
    async fn read_metadata_map(
        client: &deadpool_postgres::Client,
        id: &str,
    ) -> Result<HashMap<String, String>> {
        let row = client
            .query_opt(
                "SELECT metadata_json::text FROM conversations_pg WHERE id = $1",
                &[&id],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to read metadata for '{}': {}",
                    id,
                    pg_err(e)
                ))
            })?;
        match row {
            Some(r) => Ok(deserialize_metadata(
                &r.get::<_, Option<String>>(0).unwrap_or_default(),
            )),
            None => Err(OneAIError::Persistence(format!("session '{id}' not found"))),
        }
    }
}

// ─── Row decoding helpers ────────────────────────────────────────────────────

/// Decode an embedding column (`vector`, unspecified dim) into `Vec<f32>`.
fn row_embedding(row: &deadpool_postgres::tokio_postgres::Row, idx: usize) -> Option<Vec<f32>> {
    row.get::<_, Option<Vector>>(idx).map(|v| v.to_vec())
}

/// Decode a MemoryEntry-shaped row laid out as
/// `(id, content, timestamp, embedding, metadata_json::text)` — the column
/// order every STM/LTM reader in this module selects. Shared by all of them.
fn decode_memory_entry(row: &deadpool_postgres::tokio_postgres::Row) -> MemoryEntry {
    MemoryEntry {
        id: row.get(0),
        content: row.get(1),
        timestamp: row.get(2),
        embedding: row_embedding(row, 3),
        metadata: deserialize_metadata(&row.get::<_, String>(4)),
    }
}

#[async_trait]
impl MemoryPersistence for PgMemoryStore {
    // ─── STM operations ───────────────────────────────────────────────

    async fn save_stm(&self, session_id: &str, entries: &[MemoryEntry]) -> Result<()> {
        let mut client = self.client().await?;
        // Transactional clear + rewrite (the SQLite backend runs the same
        // DELETE-then-INSERTs sequence; the tx additionally makes the swap
        // atomic for concurrent readers).
        let tx = client.transaction().await.map_err(pg_err)?;
        tx.execute(
            "DELETE FROM stm_entries_pg WHERE session_id = $1",
            &[&session_id],
        )
        .await
        .map_err(pg_err)?;
        for (position, entry) in entries.iter().enumerate() {
            let embedding = entry.embedding.as_ref().map(|v| Vector::from(v.clone()));
            let metadata_json = serialize_metadata(&entry.metadata);
            tx.execute(
                "INSERT INTO stm_entries_pg \
                 (id, session_id, content, timestamp, embedding, metadata_json, position) \
                 VALUES ($1, $2, $3, $4, $5, $6::text::jsonb, $7)",
                &[
                    &entry.id,
                    &session_id,
                    &entry.content,
                    &entry.timestamp,
                    &embedding,
                    &metadata_json,
                    &(position as i32),
                ],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to save STM entry '{}': {}",
                    entry.id,
                    pg_err(e)
                ))
            })?;
        }
        tx.commit().await.map_err(pg_err)?;
        tracing::debug!(
            "Saved {} STM entries for session '{}' (pg)",
            entries.len(),
            session_id
        );
        Ok(())
    }

    async fn load_stm(&self, session_id: &str) -> Result<Vec<MemoryEntry>> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT id, content, timestamp, embedding, metadata_json::text \
                 FROM stm_entries_pg WHERE session_id = $1 ORDER BY position ASC",
                &[&session_id],
            )
            .await
            .map_err(pg_err)?;
        let entries: Vec<MemoryEntry> = rows.iter().map(decode_memory_entry).collect();
        tracing::debug!(
            "Loaded {} STM entries for session '{}' (pg)",
            entries.len(),
            session_id
        );
        Ok(entries)
    }

    async fn clear_stm(&self, session_id: &str) -> Result<()> {
        let client = self.client().await?;
        client
            .execute(
                "DELETE FROM stm_entries_pg WHERE session_id = $1",
                &[&session_id],
            )
            .await
            .map_err(pg_err)?;
        tracing::debug!("Cleared STM entries for session '{}' (pg)", session_id);
        Ok(())
    }

    // ─── LTM operations ───────────────────────────────────────────────

    async fn save_ltm(&self, entry: &MemoryEntry) -> Result<()> {
        let client = self.client().await?;
        let embedding = entry.embedding.as_ref().map(|v| Vector::from(v.clone()));
        let metadata_json = serialize_metadata(&entry.metadata);
        // INSERT OR REPLACE semantics (SQLite backend) → upsert on the PK.
        client
            .execute(
                "INSERT INTO ltm_entries_pg (id, content, timestamp, embedding, metadata_json) \
                 VALUES ($1, $2, $3, $4, $5::text::jsonb) \
                 ON CONFLICT (id) DO UPDATE SET \
                 content = excluded.content, timestamp = excluded.timestamp, \
                 embedding = excluded.embedding, metadata_json = excluded.metadata_json",
                &[
                    &entry.id,
                    &entry.content,
                    &entry.timestamp,
                    &embedding,
                    &metadata_json,
                ],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to save LTM entry '{}': {}",
                    entry.id,
                    pg_err(e)
                ))
            })?;
        tracing::debug!("Saved LTM entry '{}' (pg)", entry.id);
        Ok(())
    }

    async fn load_ltm(&self, id: &str) -> Result<Option<MemoryEntry>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT id, content, timestamp, embedding, metadata_json::text \
                 FROM ltm_entries_pg WHERE id = $1",
                &[&id],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to load LTM entry '{}': {}", id, pg_err(e)))
            })?;
        Ok(row.map(|r| decode_memory_entry(&r)))
    }

    async fn search_ltm_keyword(&self, keyword: &str, top_k: usize) -> Result<Vec<MemoryEntry>> {
        let client = self.client().await?;
        // ILIKE (not LIKE): Postgres LIKE is case-sensitive while SQLite LIKE
        // is case-insensitive for ASCII — ILIKE restores parity (see module
        // docs). `%`/`_` in the keyword stay wildcards, same as the SQLite
        // backend (session ids/keywords are trusted internal input).
        let pattern = format!("%{}%", keyword);
        let rows = client
            .query(
                "SELECT id, content, timestamp, embedding, metadata_json::text \
                 FROM ltm_entries_pg \
                 WHERE content ILIKE $1 OR metadata_json::text ILIKE $1 \
                 ORDER BY timestamp DESC LIMIT $2",
                &[&pattern, &(top_k as i64)],
            )
            .await
            .map_err(pg_err)?;
        let entries: Vec<MemoryEntry> = rows.iter().map(decode_memory_entry).collect();
        tracing::debug!(
            "Found {} LTM entries for keyword '{}' (pg)",
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
        // pgvector rejects empty vectors; the SQLite backend's
        // `cosine_similarity` returns 0.0 for an empty query (→ no results).
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.client().await?;
        let qv = Vector::from(query.to_vec());
        // Server-side exact KNN: `<=>` is cosine distance (1 - similarity).
        // The dim filter keeps rows written by a different embedding model
        // out of the ordering (and out of `<=>` evaluation, which errors on
        // dim mismatch) — mirroring the SQLite backend's mismatch → 0.0 →
        // filtered behavior. NaN distances (zero-norm vectors) sort last and
        // are dropped by the score > 0 filter, same as SQLite.
        //
        // No HNSW/IVFFlat index: those need a fixed dim, and this column is
        // deliberately dim-agnostic (module docs). The exact scan matches the
        // SQLite brute force's complexity class, minus the client transfer.
        let rows = client
            .query(
                "SELECT id, content, timestamp, embedding, metadata_json::text, \
                        1.0 - (embedding <=> $1) AS score \
                 FROM ltm_entries_pg \
                 WHERE embedding IS NOT NULL AND vector_dims(embedding) = vector_dims($1) \
                 ORDER BY embedding <=> $1 ASC \
                 LIMIT $2",
                &[&qv, &(top_k as i64)],
            )
            .await
            .map_err(pg_err)?;
        let mut scored: Vec<(MemoryEntry, f32)> = rows
            .iter()
            .map(|r| {
                let score = r.get::<_, f64>(5) as f32;
                (decode_memory_entry(r), score)
            })
            // The SQLite backend keeps only positive-similarity hits.
            .filter(|(_, score)| *score > 0.0)
            .collect();
        // Rows already arrive similarity-desc (distance-asc); the sort is a
        // no-op safety net for NaN/edge orderings.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        tracing::debug!(
            "Found {} LTM entries by embedding (top {}, pg KNN)",
            scored.len(),
            top_k
        );
        Ok(scored)
    }

    async fn delete_ltm(&self, id: &str) -> Result<()> {
        let client = self.client().await?;
        client
            .execute("DELETE FROM ltm_entries_pg WHERE id = $1", &[&id])
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to delete LTM entry '{}': {}",
                    id,
                    pg_err(e)
                ))
            })?;
        tracing::debug!("Deleted LTM entry '{}' (pg)", id);
        Ok(())
    }

    async fn clear_ltm(&self) -> Result<()> {
        let client = self.client().await?;
        client
            .execute("DELETE FROM ltm_entries_pg", &[])
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to clear LTM entries: {}", pg_err(e)))
            })?;
        tracing::debug!("Cleared all LTM entries (pg)");
        Ok(())
    }

    // ─── Conversation operations ──────────────────────────────────────

    async fn save_conversation(&self, id: &str, conversation: &Conversation) -> Result<()> {
        let messages_json = serde_json::to_string(&conversation.messages).map_err(|e| {
            OneAIError::Persistence(format!("Failed to serialize conversation '{}': {}", id, e))
        })?;
        let now = chrono::Utc::now();
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(pg_err)?;

        // Fetch existing metadata so a resave doesn't clobber override keys
        // the live conversation doesn't carry (`session/rename` writes
        // metadata["title"], `session/archive` writes metadata["archived"]
        // directly). `conversation.metadata` wins for keys it sets — it is
        // the authoritative live state. Mirrors SqliteSessionStore exactly.
        let existing = tx
            .query_opt(
                "SELECT metadata_json::text FROM conversations_pg WHERE id = $1",
                &[&id],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to read existing metadata for '{}': {}",
                    id,
                    pg_err(e)
                ))
            })?;
        let mut merged: HashMap<String, String> = existing
            .as_ref()
            .map(|r| deserialize_metadata(&r.get::<_, Option<String>>(0).unwrap_or_default()))
            .unwrap_or_default();
        for (k, v) in &conversation.metadata {
            merged.insert(k.clone(), v.clone());
        }
        let metadata_json = serialize_metadata(&merged);
        // Title: the merged override wins (so a rename persists across a
        // resave); else derive from the first user message.
        let title = merged
            .get("title")
            .map(|t| normalize_title(t, 80))
            .filter(|t| !t.is_empty())
            .or_else(|| first_user_message_title(conversation, 80));

        if existing.is_some() {
            // Recompute the title on update too — the first user message
            // could have changed (e.g. history rewritten by a compact).
            tx.execute(
                "UPDATE conversations_pg \
                 SET messages_json = $2::text::jsonb, metadata_json = $3::text::jsonb, \
                     updated_at = $4, title = $5 \
                 WHERE id = $1",
                &[&id, &messages_json, &metadata_json, &now, &title],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to update conversation '{}': {}",
                    id,
                    pg_err(e)
                ))
            })?;
        } else {
            tx.execute(
                "INSERT INTO conversations_pg \
                 (id, messages_json, metadata_json, created_at, updated_at, title) \
                 VALUES ($1, $2::text::jsonb, $3::text::jsonb, $4, $5, $6)",
                &[&id, &messages_json, &metadata_json, &now, &now, &title],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to insert conversation '{}': {}",
                    id,
                    pg_err(e)
                ))
            })?;
        }
        tx.commit().await.map_err(pg_err)?;
        tracing::debug!(
            "Saved conversation '{}' ({} messages, pg)",
            id,
            conversation.messages.len()
        );
        Ok(())
    }

    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT messages_json::text, metadata_json::text, title \
                 FROM conversations_pg WHERE id = $1",
                &[&id],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to load conversation '{}': {}",
                    id,
                    pg_err(e)
                ))
            })?;
        let Some(row) = row else { return Ok(None) };
        let messages_json = row.get::<_, String>(0);
        let metadata_json: Option<String> = row.get(1);
        let title: Option<String> = row.get(2);
        let messages: Vec<oneai_core::Message> =
            serde_json::from_str(&messages_json).map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to deserialize conversation '{}': {}",
                    id, e
                ))
            })?;
        let mut conversation = Conversation::with_id(id.to_string());
        conversation.messages = messages;
        // Restore metadata so a resumed session keeps its title (and any
        // other conversation-level metadata) across the next save.
        if let Some(json) = metadata_json {
            if !json.is_empty() {
                conversation.metadata = deserialize_metadata(&json);
            }
        }
        // Rows without metadata["title"]: promote the title column so the
        // next save preserves it instead of re-deriving from the first user
        // message (which would clobber e.g. a scenario name).
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

    /// Load a session's discarded-prefix archive snapshots, oldest-first.
    /// Mirrors `SqliteSessionStore::load_discarded_snapshots` — same
    /// `{session_id}{DISCARDED_SNAPSHOT_MARKER}{uuid}` id convention; the
    /// chronological order is what `full_transcript_messages` relies on.
    async fn load_discarded_snapshots(&self, session_id: &str) -> Result<Vec<Conversation>> {
        let client = self.client().await?;
        let pat = format!("{}{}%", session_id, oneai_core::DISCARDED_SNAPSHOT_MARKER);
        let rows = client
            .query(
                "SELECT id, messages_json::text FROM conversations_pg \
                 WHERE id LIKE $1 ORDER BY created_at ASC",
                &[&pat],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to query discarded snapshots for '{}': {}",
                    session_id,
                    pg_err(e)
                ))
            })?;
        let mut out = Vec::new();
        for row in rows {
            let id: String = row.get(0);
            let messages_json: String = row.get(1);
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

    /// Cheap per-snapshot non-`system` message count, oldest-first — the
    /// `jsonb_array_elements` counterpart of the SQLite `json_each` query
    /// (NULL-role elements are excluded by SQL's `<> 'system'` three-valued
    /// logic, exactly like SQLite's `!= 'system'`).
    async fn snapshot_display_counts(&self, session_id: &str) -> Result<Vec<(String, u32)>> {
        let client = self.client().await?;
        let pat = format!("{}{}%", session_id, oneai_core::DISCARDED_SNAPSHOT_MARKER);
        let rows = client
            .query(
                "SELECT s.id, (SELECT count(*) FROM jsonb_array_elements(s.messages_json) \
                     AS m(elem) WHERE m.elem->>'role' <> 'system') \
                 FROM conversations_pg s WHERE s.id LIKE $1 \
                 ORDER BY s.created_at ASC",
                &[&pat],
            )
            .await
            .map_err(pg_err)?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as u32))
            .collect())
    }

    async fn list_conversations(&self) -> Result<Vec<SessionInfo>> {
        let client = self.client().await?;
        // Same folded-display-count contract as the SQLite backend (sidebar
        // "N 条" = visible bubbles, live row + discarded snapshots summed;
        // see the long note in `SqliteSessionStore::list_conversations` —
        // issue #14/#17). Counting happens in Rust via the shared
        // `folded_display_count`.
        let rows = client
            .query(
                "SELECT id, created_at, updated_at, title, messages_json::text, metadata_json::text \
                 FROM conversations_pg ORDER BY updated_at DESC",
                &[],
            )
            .await
            .map_err(pg_err)?;

        // Bucket discarded-prefix snapshots under their parent session id.
        let mut snapshots: HashMap<String, Vec<Vec<oneai_core::Message>>> = HashMap::new();
        // Preserve SQL order (updated_at DESC) for the returned list.
        let mut tops: Vec<(
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
            Option<String>,
            usize,
            Option<String>,
            bool,
        )> = Vec::new();

        for row in rows {
            let id: String = row.get(0);
            let created_at = row.get(1);
            let updated_at = row.get(2);
            let title: Option<String> = row.get(3);
            let messages_json: String = row.get(4);
            let metadata_json: Option<String> = row.get(5);
            // Tolerate legacy/corrupt blobs: an unparseable row contributes 0
            // rather than hiding every conversation from the sidebar.
            let msgs: Vec<oneai_core::Message> =
                serde_json::from_str(&messages_json).unwrap_or_default();
            let workspace = metadata_json
                .as_deref()
                .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
                .and_then(|v| v.get("workspace").cloned())
                .and_then(|v| v.as_str().map(|s| s.to_string()));
            let archived = metadata_json
                .as_deref()
                .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
                .and_then(|v| v.get("archived").cloned())
                .and_then(|v| v.as_str().map(|s| s == "1"))
                .unwrap_or(false);
            if id.contains(oneai_core::DISCARDED_SNAPSHOT_MARKER) {
                if let Some(parent) = id.split(oneai_core::DISCARDED_SNAPSHOT_MARKER).next() {
                    snapshots.entry(parent.to_string()).or_default().push(msgs);
                }
            } else {
                let count = folded_display_count(&msgs);
                tops.push((
                    id, created_at, updated_at, title, count, workspace, archived,
                ));
            }
        }

        let mut sessions = Vec::with_capacity(tops.len());
        for (id, created_at, updated_at, title, mut count, workspace, archived) in tops {
            if let Some(children) = snapshots.get(&id) {
                for child in children {
                    count += folded_display_count(child);
                }
            }
            sessions.push(
                SessionInfo::with_title(id, created_at, updated_at, count, title)
                    .with_workspace(workspace)
                    .with_archived(archived),
            );
        }
        tracing::debug!("Listed {} conversations (pg)", sessions.len());
        Ok(sessions)
    }

    async fn delete_conversation(&self, id: &str) -> Result<()> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(pg_err)?;
        let discard_prefix = format!("{}{}%", id, oneai_core::DISCARDED_SNAPSHOT_MARKER);
        // Delete STM entries for the session and any of its discarded
        // snapshots, then the conversation row + cascade-delete the
        // discarded-prefix archive snapshots so they don't leak as orphans.
        tx.execute(
            "DELETE FROM stm_entries_pg WHERE session_id = $1 OR session_id LIKE $2",
            &[&id, &discard_prefix],
        )
        .await
        .map_err(pg_err)?;
        tx.execute(
            "DELETE FROM conversations_pg WHERE id = $1 OR id LIKE $2",
            &[&id, &discard_prefix],
        )
        .await
        .map_err(pg_err)?;
        tx.commit().await.map_err(pg_err)?;
        tracing::debug!("Deleted conversation '{}' and its STM entries (pg)", id);
        Ok(())
    }

    /// Targeted metadata-only rename (mirrors
    /// `SqliteSessionStore::rename_conversation`): reads + rewrites
    /// `metadata_json` (and the `title` column) WITHOUT touching
    /// `messages_json`, so it never races a concurrent turn's
    /// `save_conversation`.
    async fn rename_conversation(&self, id: &str, title: &str) -> Result<()> {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return Ok(()); // "keep current" — never write an empty title
        }
        let client = self.client().await?;
        let mut metadata = Self::read_metadata_map(&client, id).await?;
        metadata.insert("title".to_string(), trimmed.to_string());
        let metadata_json = serialize_metadata(&metadata);
        client
            .execute(
                "UPDATE conversations_pg SET title = $1, metadata_json = $2::text::jsonb \
                 WHERE id = $3",
                &[&trimmed, &metadata_json, &id],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to rename conversation '{}': {}",
                    id,
                    pg_err(e)
                ))
            })?;
        tracing::debug!("Renamed conversation '{}' → '{trimmed}' (pg)", id);
        Ok(())
    }

    /// Targeted metadata-only archive toggle (mirrors
    /// `SqliteSessionStore::set_conversation_archived`).
    async fn set_conversation_archived(&self, id: &str, archived: bool) -> Result<()> {
        let client = self.client().await?;
        let mut metadata = Self::read_metadata_map(&client, id).await?;
        if archived {
            metadata.insert("archived".to_string(), "1".to_string());
        } else {
            metadata.remove("archived");
        }
        let metadata_json = serialize_metadata(&metadata);
        client
            .execute(
                "UPDATE conversations_pg SET metadata_json = $1::text::jsonb WHERE id = $2",
                &[&metadata_json, &id],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to archive conversation '{}': {}",
                    id,
                    pg_err(e)
                ))
            })?;
        tracing::debug!("Set conversation '{}' archived={archived} (pg)", id);
        Ok(())
    }

    // ─── MemoryFact persistence ──────────────────────────────────────────────

    async fn store_fact(&self, fact: &MemoryFact) -> Result<()> {
        let client = self.client().await?;
        let embedding = fact.embedding.as_ref().map(|v| Vector::from(v.clone()));
        let metadata_json =
            serde_json::to_string(&fact.metadata).unwrap_or_else(|_| "{}".to_string());
        // Conflict-resolved upsert: same (user_id, subject, predicate) →
        // update content/embedding/metadata/fact_type/updated_at and bump
        // version, preserving the original id/created_at. Mirrors the SQLite
        // backend's Mem0 invariant so persistence and runtime agree.
        client
            .execute(
                "INSERT INTO memories_pg \
                 (id, user_id, session_id, fact_type, subject, predicate, content, embedding, \
                  metadata_json, created_at, updated_at, version, importance, superseded, \
                  superseded_at, pinned) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::text::jsonb, $10, $11, $12, $13, \
                         $14, $15, $16) \
                 ON CONFLICT (user_id, subject, predicate) DO UPDATE SET \
                 content = excluded.content, \
                 embedding = excluded.embedding, \
                 metadata_json = excluded.metadata_json, \
                 fact_type = excluded.fact_type, \
                 updated_at = excluded.updated_at, \
                 version = memories_pg.version + 1, \
                 importance = excluded.importance, \
                 superseded = excluded.superseded, \
                 superseded_at = excluded.superseded_at, \
                 pinned = excluded.pinned",
                &[
                    &fact.id,
                    &fact.user_id,
                    &fact.session_id,
                    &fact.fact_type.as_str(),
                    &fact.subject,
                    &fact.predicate,
                    &fact.content,
                    &embedding,
                    &metadata_json,
                    &fact.created_at,
                    &fact.updated_at,
                    &(fact.version as i64),
                    &(fact.importance as f64),
                    &fact.superseded,
                    &fact.superseded_at,
                    &fact.pinned,
                ],
            )
            .await
            .map_err(|e| OneAIError::Persistence(format!("Failed to store fact: {}", pg_err(e))))?;
        Ok(())
    }

    async fn load_facts(&self, user_id: &str, session_id: &str) -> Result<Vec<MemoryFact>> {
        let client = self.client().await?;
        // Empty session_id → all facts for the user (cross-session habits);
        // otherwise scope to that session.
        let rows = client
            .query(
                "SELECT id, user_id, session_id, fact_type, subject, predicate, content, \
                        embedding, metadata_json::text, created_at, updated_at, version, \
                        importance, superseded, superseded_at, pinned \
                 FROM memories_pg WHERE user_id = $1 AND ($2 = '' OR session_id = $2)",
                &[&user_id, &session_id],
            )
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!("Failed to query facts: {}", pg_err(e)))
            })?;
        let mut facts = Vec::with_capacity(rows.len());
        for row in rows {
            let metadata_json: String = row.get(8);
            facts.push(MemoryFact {
                id: row.get(0),
                user_id: row.get(1),
                session_id: row.get(2),
                fact_type: oneai_core::FactType::new(row.get::<_, String>(3)),
                subject: row.get(4),
                predicate: row.get(5),
                content: row.get(6),
                embedding: row_embedding(&row, 7),
                metadata: serde_json::from_str(&metadata_json).unwrap_or_default(),
                importance: row.get::<_, f64>(12) as f32,
                created_at: row.get(9),
                updated_at: row.get(10),
                version: row.get::<_, i64>(11) as u32,
                superseded: row.get(13),
                superseded_at: row.get(14),
                pinned: row.get(15),
            });
        }
        Ok(facts)
    }
}
