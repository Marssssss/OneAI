//! `VectorBackend` backed by [`sqlite-vec`] exact KNN.
//!
//! `sqlite-vec` ships only the C function `sqlite3_vec_init`; there is no Rust
//! wrapper. We static-register it as a SQLite auto extension via
//! `sqlite3_auto_extension` (one `Once` per process) so the `vec0` virtual
//! table is available on every connection without `load_extension`. This was
//! verified in the P0 spike — notably the rusqlite `bundled` feature alone
//! suffices (no `load_extension` feature is needed) because the extension is
//! linked into the same `SQLITE_CORE` build.
//!
//! Scale ceiling: `vec0` does **exact** brute-force KNN (not HNSW), so it is
//! the right pick for mobile / small persistent stores (~10⁵ vectors). For
//! desktop/server scale use [`UsearchBackend`](crate::UsearchBackend).
//!
//! Metadata filtering is **post-filter**: `vec0` KNN runs first over the full
//! set, then non-matching rows are dropped. To stay correct under selective
//! filters we over-fetch (`limit = top_k * OVER_FETCH_FACTOR`, capped) and
//! post-filter down to `top_k`. This is a documented trade-off of the exact-
//! KNN engine; for highly selective filters on large sets prefer `UsearchBackend`
//! which supports true pre-filter via a search callback.

use std::collections::HashMap;
use std::sync::Once;

use async_trait::async_trait;
use oneai_core::traits::{Filter, Metadata, VectorBackend, VectorHit};
use rusqlite::ffi::sqlite3_auto_extension;
use tokio::sync::Mutex;

/// Multiplier on `top_k` when a filter is present, so post-filtering still
/// yields enough hits. Capped to keep KNN bounded.
const OVER_FETCH_FACTOR: usize = 8;
/// Hard cap on rows fetched per KNN query (post-filter trades correctness for
/// memory/time at extreme scales).
const MAX_FETCH: usize = 512;

/// Registers `sqlite3_vec_init` as a SQLite auto extension, exactly once per
/// process. Safe to call repeatedly.
///
/// The `transmute` here is verbatim the registration pattern published by the
/// sqlite-vec crate itself (sqlite-vec-0.1.9/src/lib.rs:15) — `sqlite3_vec_init`
/// is an untyped `extern "C"` fn and must be widened to SQLite's
/// `sqlite3_callback` signature. `missing_transmute_annotations` is cosmetic
/// on this canonical FFI shim, so it's suppressed locally.
#[allow(clippy::missing_transmute_annotations)]
fn register_vec_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is the canonical entry point exported by
        // the sqlite-vec crate; registering it via `sqlite3_auto_extension`
        // makes the `vec0` module available on every subsequently opened
        // connection. The transmute is the documented registration pattern
        // (the C signature is `int sqlite3_vec_init(sqlite3*, char**, const
        // sqlite3_api_routines*)`, matching `sqlite3_auto_extension`'s
        // expected `sqlite3_callback`). No mutable state of ours is touched.
        unsafe {
            sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Exact-KNN vector backend over a `vec0` virtual table.
pub struct SqliteVecBackend {
    conn: Mutex<rusqlite::Connection>,
    dim: usize,
    /// rowid ↔ string id (vec0 keys by integer rowid; we map to the trait's
    /// string id).
    rowid_to_id: Mutex<HashMap<i64, String>>,
    id_to_rowid: Mutex<HashMap<String, i64>>,
    /// metadata keyed by string id (vec0 stores vectors; metadata lives here).
    meta: Mutex<HashMap<String, Metadata>>,
    next_rowid: Mutex<i64>,
}

impl SqliteVecBackend {
    /// Open (or create) a backend at `path` (`":memory:"` for in-RAM) with the
    /// given fixed dimension. The `vec0` virtual table is created if absent.
    pub fn open(path: &str, dim: usize) -> oneai_core::Result<Self> {
        register_vec_once();
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("sqlite-vec open {path}: {e}")))?;
        conn.execute_batch(&format!(
            // The embedding column is fixed-dim float; the side `document_id`
            // text column is unused (we map rowid↔id in Rust) but kept for
            // compatibility with the vec0 schema requirement that a virtual
            // table have a recognizable shape.
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(embedding float[{dim}], document_id text);",
        ))
        .map_err(|e| oneai_core::OneAIError::Rag(format!("create vec0: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
            dim,
            rowid_to_id: Mutex::new(HashMap::new()),
            id_to_rowid: Mutex::new(HashMap::new()),
            meta: Mutex::new(HashMap::new()),
            next_rowid: Mutex::new(1),
        })
    }

    /// In-RAM backend (shorthand for `open(":memory:", dim)`).
    pub fn in_memory(dim: usize) -> oneai_core::Result<Self> {
        Self::open(":memory:", dim)
    }
}

#[async_trait]
impl VectorBackend for SqliteVecBackend {
    async fn upsert(
        &self,
        id: &str,
        embedding: &[f32],
        metadata: Metadata,
    ) -> oneai_core::Result<()> {
        if embedding.len() != self.dim {
            return Err(oneai_core::OneAIError::Rag(format!(
                "SqliteVecBackend: dim mismatch (got {}, expected {})",
                embedding.len(),
                self.dim
            )));
        }
        // vec0 stores embeddings as a JSON array string, e.g. "[0.1,0.2,...]".
        let emb_json = serde_json::to_string(embedding)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("embed json: {e}")))?;
        let conn = self.conn.lock().await;
        let mut id_to_rowid = self.id_to_rowid.lock().await;
        let mut rowid_to_id = self.rowid_to_id.lock().await;
        let mut meta = self.meta.lock().await;
        let mut next_rowid = self.next_rowid.lock().await;

        if let Some(&rowid) = id_to_rowid.get(id) {
            // Replace existing row by rowid.
            conn.execute(
                "DELETE FROM vec_chunks WHERE rowid = ?1",
                rusqlite::params![rowid],
            )
            .map_err(|e| oneai_core::OneAIError::Rag(format!("vec0 delete-row: {e}")))?;
            conn.execute(
                "INSERT INTO vec_chunks(rowid, embedding, document_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![rowid, emb_json, id],
            )
            .map_err(|e| oneai_core::OneAIError::Rag(format!("vec0 insert: {e}")))?;
        } else {
            let rowid = *next_rowid;
            *next_rowid += 1;
            conn.execute(
                "INSERT INTO vec_chunks(rowid, embedding, document_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![rowid, emb_json, id],
            )
            .map_err(|e| oneai_core::OneAIError::Rag(format!("vec0 insert: {e}")))?;
            id_to_rowid.insert(id.to_string(), rowid);
            rowid_to_id.insert(rowid, id.to_string());
        }
        meta.insert(id.to_string(), metadata);
        Ok(())
    }

    async fn search(
        &self,
        query: &[f32],
        top_k: usize,
        filter: Option<&Filter>,
    ) -> oneai_core::Result<Vec<VectorHit>> {
        if query.len() != self.dim {
            return Err(oneai_core::OneAIError::Rag(format!(
                "SqliteVecBackend: query dim mismatch (got {}, expected {})",
                query.len(),
                self.dim
            )));
        }
        let q_json = serde_json::to_string(query)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("query json: {e}")))?;
        let conn = self.conn.lock().await;
        let rowid_to_id = self.rowid_to_id.lock().await;
        let meta = self.meta.lock().await;

        let limit = match filter {
            Some(f) if !f.metadata_eq.is_empty() && !f.metadata_in.is_empty() => (top_k
                .saturating_mul(OVER_FETCH_FACTOR))
            .min(MAX_FETCH)
            .max(top_k),
            _ => top_k.max(1),
        };
        let sql = "SELECT rowid, distance FROM vec_chunks WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2";
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("vec0 prepare: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![q_json, limit as i64], |r| {
                let rowid: i64 = r.get(0)?;
                let dist: f32 = r.get(1)?;
                Ok((rowid, dist))
            })
            .map_err(|e| oneai_core::OneAIError::Rag(format!("vec0 query: {e}")))?;

        let mut hits: Vec<VectorHit> = Vec::new();
        for r in rows {
            let (rowid, dist) =
                r.map_err(|e| oneai_core::OneAIError::Rag(format!("vec0 row: {e}")))?;
            let Some(id) = rowid_to_id.get(&rowid) else {
                continue;
            };
            let metadata = meta.get(id).cloned().unwrap_or_default();
            if let Some(f) = filter {
                if !f.matches(&metadata) {
                    continue;
                }
            }
            // cosine distance from sqlite-vec is in [0,2]; similarity = 1 - dist.
            let score = (1.0 - dist).clamp(-1.0, 1.0);
            hits.push(VectorHit {
                id: id.clone(),
                score,
                metadata,
            });
            if filter.is_none() && hits.len() >= top_k {
                break;
            }
        }
        // Sort defensively (KNN ORDER BY distance already orders, but post-
        // filtering may have removed rows; re-sort by similarity desc).
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(top_k);
        Ok(hits)
    }

    async fn delete(&self, id: &str) -> oneai_core::Result<()> {
        let conn = self.conn.lock().await;
        let mut id_to_rowid = self.id_to_rowid.lock().await;
        let mut rowid_to_id = self.rowid_to_id.lock().await;
        let mut meta = self.meta.lock().await;
        if let Some(rowid) = id_to_rowid.remove(id) {
            conn.execute(
                "DELETE FROM vec_chunks WHERE rowid = ?1",
                rusqlite::params![rowid],
            )
            .map_err(|e| oneai_core::OneAIError::Rag(format!("vec0 delete: {e}")))?;
            rowid_to_id.remove(&rowid);
            meta.remove(id);
        }
        Ok(())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// NOTE: `vec0` computes distance natively (no `cosine` helper needed) and
// metadata is held in a side `HashMap` (no `meta_to_json`/`meta_from_json`
// in the vec0 table) — so the shared crate helpers are intentionally unused
// by this backend.

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn knn_upsert_search_filter_delete() {
        let be = SqliteVecBackend::in_memory(4).unwrap();
        be.upsert(
            "a",
            &[0.1, 0.2, 0.3, 0.4],
            Metadata::from([("k".into(), "x".into())]),
        )
        .await
        .unwrap();
        be.upsert(
            "b",
            &[0.11, 0.21, 0.31, 0.41],
            Metadata::from([("k".into(), "y".into())]),
        )
        .await
        .unwrap();
        be.upsert(
            "c",
            &[0.9, 0.8, 0.7, 0.6],
            Metadata::from([("k".into(), "x".into())]),
        )
        .await
        .unwrap();

        let hits = be.search(&[0.1, 0.2, 0.3, 0.4], 3, None).await.unwrap();
        assert_eq!(hits[0].id, "a");
        assert_eq!(hits[1].id, "b");
        assert!(hits[0].score > 0.99);

        let f = Filter::new().with_eq("k", "x");
        let hits = be.search(&[0.1, 0.2, 0.3, 0.4], 5, Some(&f)).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.metadata["k"] == "x"));

        be.delete("b").await.unwrap();
        let hits = be
            .search(&[0.11, 0.21, 0.31, 0.41], 10, None)
            .await
            .unwrap();
        assert!(!hits.iter().any(|h| h.id == "b"));
    }

    #[tokio::test]
    async fn upsert_replaces_same_id() {
        let be = SqliteVecBackend::in_memory(4).unwrap();
        be.upsert("a", &[0.1, 0.2, 0.3, 0.4], Metadata::new())
            .await
            .unwrap();
        be.upsert("a", &[0.9, 0.8, 0.7, 0.6], Metadata::new())
            .await
            .unwrap();
        let hits = be.search(&[0.9, 0.8, 0.7, 0.6], 1, None).await.unwrap();
        assert_eq!(hits[0].id, "a");
        assert!(hits[0].score > 0.99);
        // Only one row for 'a'.
        let hits = be.search(&[0.1, 0.2, 0.3, 0.4], 10, None).await.unwrap();
        assert_eq!(hits.iter().filter(|h| h.id == "a").count(), 1);
    }
}
