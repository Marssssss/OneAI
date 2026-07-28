//! `VectorBackend` backed by [`usearch`] HNSW ANN.
//!
//! Verified API surface (P0 spike): `Index::new(&IndexOptions{dimensions,
//! metric, quantization, connectivity, expansion_add, expansion_search,
//! multi})`, `add(key: u64, &[f32])`, `search(&[f32], k) -> Matches{keys,
//! distances}`, `save(path)` / `load(path)` (`load` is an `&self` method —
//! construct with `Index::new` first), and `filtered_search(&[f32], k, |key:
//! u64| bool)` which does **pre-filter** search — exactly what metadata
//! filtering needs.
//!
//! `usearch` stores only `(u64 key, Vec<f32>)`, so the string id + metadata
//! live in side maps. Upsert assigns a fresh key per write; stale vectors from
//! a previous write of the same id stay in the HNSW graph (cheap removal isn't
//! generally supported) but are filtered out at search time via the `key→id`
//! map (only the *current* key per id is mapped, so stale keys map to `None`
//! and are skipped). For the desktop/server scale `usearch` targets this is a
//! fine trade-off; a periodic rebuild reclaims graph space.
//!
//! Persistence: [`UsearchBackend::save`] writes both the usearch index and a
//! `{path}.meta.json` sidecar (id↔key + metadata); [`UsearchBackend::open`]
//! loads both. `mmap`-backed read-only views (`Index::view`) are left to a
//! future enhancement — `load` (full read) is the default here.

use std::collections::HashMap;

use async_trait::async_trait;
use oneai_core::traits::{Filter, Metadata, VectorBackend, VectorHit};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

/// Default HNSW parameters (matching the spike): connectivity 16,
/// expansion_add 128, expansion_search 64. Tunable via [`UsearchBackendConfig`].
const DEFAULT_CONNECTIVITY: usize = 16;
const DEFAULT_EXPANSION_ADD: usize = 128;
const DEFAULT_EXPANSION_SEARCH: usize = 64;
/// Initial reserved capacity (usearch requires `reserve()` before `add()`).
/// Grown geometrically as the index fills.
const INITIAL_CAPACITY: usize = 1024;

/// Configuration for [`UsearchBackend`].
#[derive(Debug, Clone, Copy)]
pub struct UsearchBackendConfig {
    pub connectivity: usize,
    pub expansion_add: usize,
    pub expansion_search: usize,
}

impl Default for UsearchBackendConfig {
    fn default() -> Self {
        Self {
            connectivity: DEFAULT_CONNECTIVITY,
            expansion_add: DEFAULT_EXPANSION_ADD,
            expansion_search: DEFAULT_EXPANSION_SEARCH,
        }
    }
}

fn build_index(dim: usize, cfg: UsearchBackendConfig) -> oneai_core::Result<Index> {
    let opts = IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: cfg.connectivity,
        expansion_add: cfg.expansion_add,
        expansion_search: cfg.expansion_search,
        multi: false,
    };
    Index::new(&opts).map_err(|e| oneai_core::OneAIError::Rag(format!("usearch new: {e}")))
}

/// Reserve `cap` slots on a freshly-built index. Called once at construction
/// (and again from `upsert` to grow when capacity is exhausted).
fn reserve_initial(index: &Index, cap: usize) -> oneai_core::Result<()> {
    index
        .reserve(cap)
        .map_err(|e| oneai_core::OneAIError::Rag(format!("usearch reserve {cap}: {e}")))
}

/// Sidecar persisted alongside the usearch index file.
#[derive(Serialize, Deserialize)]
struct Sidecar {
    dim: usize,
    key_to_id: Vec<(u64, String)>,
    id_to_key: Vec<(String, u64)>,
    meta: Vec<(String, HashMap<String, String>)>,
    next_key: u64,
}

/// HNSW vector backend.
pub struct UsearchBackend {
    index: Mutex<Index>,
    dim: usize,
    cfg: UsearchBackendConfig,
    key_to_id: Mutex<HashMap<u64, String>>,
    id_to_key: Mutex<HashMap<String, u64>>,
    meta: Mutex<HashMap<String, Metadata>>,
    next_key: Mutex<u64>,
    /// Reserved HNSW capacity (usearch requires `reserve()` before `add()`;
    /// we grow it geometrically as `next_key` approaches it).
    reserved: Mutex<usize>,
}

impl UsearchBackend {
    /// Create an in-memory HNSW index with default parameters.
    pub fn new(dim: usize) -> oneai_core::Result<Self> {
        Self::with_config(dim, UsearchBackendConfig::default())
    }

    /// Create with custom HNSW parameters.
    pub fn with_config(dim: usize, cfg: UsearchBackendConfig) -> oneai_core::Result<Self> {
        let index = build_index(dim, cfg)?;
        reserve_initial(&index, INITIAL_CAPACITY)?;
        Ok(Self {
            index: Mutex::new(index),
            dim,
            cfg,
            key_to_id: Mutex::new(HashMap::new()),
            id_to_key: Mutex::new(HashMap::new()),
            meta: Mutex::new(HashMap::new()),
            next_key: Mutex::new(1),
            reserved: Mutex::new(INITIAL_CAPACITY),
        })
    }

    /// Load a persisted backend: reads the usearch index at `path` and the
    /// `{path}.meta.json` sidecar. The HNSW parameters are assumed to match
    /// [`UsearchBackendConfig::default`]; for custom params use
    /// [`Self::open_with_config`].
    pub fn open(path: &str, dim: usize) -> oneai_core::Result<Self> {
        Self::open_with_config(path, dim, UsearchBackendConfig::default())
    }

    /// Load with explicit HNSW parameters.
    pub fn open_with_config(
        path: &str,
        dim: usize,
        cfg: UsearchBackendConfig,
    ) -> oneai_core::Result<Self> {
        let index = build_index(dim, cfg)?;
        index
            .load(path)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("usearch load {path}: {e}")))?;
        let sidecar_path = sidecar_path(path);
        let sidecar_str = std::fs::read_to_string(&sidecar_path).map_err(|e| {
            oneai_core::OneAIError::Rag(format!("read sidecar {}: {e}", sidecar_path))
        })?;
        let sidecar: Sidecar = serde_json::from_str(&sidecar_str)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("parse sidecar: {e}")))?;
        Ok(Self {
            index: Mutex::new(index),
            dim: sidecar.dim,
            cfg,
            key_to_id: Mutex::new(sidecar.key_to_id.into_iter().collect()),
            id_to_key: Mutex::new(sidecar.id_to_key.into_iter().collect()),
            meta: Mutex::new(sidecar.meta.into_iter().collect()),
            next_key: Mutex::new(sidecar.next_key),
            // Loaded index already has capacity for the on-disk vectors; reserve
            // headroom for further inserts.
            reserved: Mutex::new(sidecar.next_key.max(INITIAL_CAPACITY as u64) as usize),
        })
    }

    /// Persist the index + sidecar. Writes `{path}.meta.json` next to `path`.
    pub async fn save(&self, path: &str) -> oneai_core::Result<()> {
        let index = self.index.lock().await;
        index
            .save(path)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("usearch save {path}: {e}")))?;
        drop(index);

        let key_to_id = self.key_to_id.lock().await;
        let id_to_key = self.id_to_key.lock().await;
        let meta = self.meta.lock().await;
        let next_key = self.next_key.lock().await;
        let sidecar = Sidecar {
            dim: self.dim,
            key_to_id: key_to_id.iter().map(|(k, v)| (*k, v.clone())).collect(),
            id_to_key: id_to_key.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            meta: meta.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            next_key: *next_key,
        };
        let sidecar_str = serde_json::to_string(&sidecar)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("serialize sidecar: {e}")))?;
        let p = sidecar_path(path);
        std::fs::write(&p, sidecar_str)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("write sidecar {p}: {e}")))
    }
}

/// Path of the metadata sidecar written next to the usearch index file.
/// Given `path = "/tmp/idx.usearch"` → `"/tmp/idx.usearch.meta.json"`.
fn sidecar_path(path: &str) -> String {
    format!("{path}.meta.json")
}

#[async_trait]
impl VectorBackend for UsearchBackend {
    async fn upsert(
        &self,
        id: &str,
        embedding: &[f32],
        metadata: Metadata,
    ) -> oneai_core::Result<()> {
        if embedding.len() != self.dim {
            return Err(oneai_core::OneAIError::Rag(format!(
                "UsearchBackend: dim mismatch (got {}, expected {})",
                embedding.len(),
                self.dim
            )));
        }
        let index = self.index.lock().await;
        let mut key_to_id = self.key_to_id.lock().await;
        let mut id_to_key = self.id_to_key.lock().await;
        let mut meta = self.meta.lock().await;
        let mut next_key = self.next_key.lock().await;
        let mut reserved = self.reserved.lock().await;
        // Retire the old key mapping (stale vector stays in the graph but is
        // skipped at search via key_to_id miss).
        if let Some(old_key) = id_to_key.get(id).copied() {
            key_to_id.remove(&old_key);
        }
        let key = *next_key;
        *next_key += 1;
        // Grow reserved capacity geometrically when exhausted (usearch errors
        // "Reserve capacity ahead of insertions!" otherwise).
        if (key as usize) > *reserved {
            let new_cap = (*reserved * 2).max(key as usize + INITIAL_CAPACITY);
            index.reserve(new_cap).map_err(|e| {
                oneai_core::OneAIError::Rag(format!("usearch reserve {new_cap}: {e}"))
            })?;
            *reserved = new_cap;
        }
        index
            .add(key, embedding)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("usearch add: {e}")))?;
        id_to_key.insert(id.to_string(), key);
        key_to_id.insert(key, id.to_string());
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
                "UsearchBackend: query dim mismatch (got {}, expected {})",
                query.len(),
                self.dim
            )));
        }
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let index = self.index.lock().await;
        let key_to_id = self.key_to_id.lock().await;
        let meta = self.meta.lock().await;

        // Over-fetch to compensate for stale keys & filtered-out rows.
        let fetch_k = match filter {
            Some(_) => (top_k * 4).max(top_k),
            None => top_k,
        };

        let matches = if let Some(f) = filter {
            // Pre-filter: the closure decides which keys participate in the
            // HNSW traversal (spike-verified `filtered_search`).
            index
                .filtered_search(query, fetch_k, |key: u64| -> bool {
                    key_to_id
                        .get(&key)
                        .is_some_and(|id| meta.get(id).is_some_and(|m| f.matches(m)))
                })
                .map_err(|e| oneai_core::OneAIError::Rag(format!("usearch filtered_search: {e}")))?
        } else {
            index
                .search(query, fetch_k)
                .map_err(|e| oneai_core::OneAIError::Rag(format!("usearch search: {e}")))?
        };

        let mut hits: Vec<VectorHit> = Vec::new();
        for (key, dist) in matches.keys.iter().zip(matches.distances.iter()) {
            let Some(id) = key_to_id.get(key) else {
                continue; // stale key from a prior upsert
            };
            let metadata = meta.get(id).cloned().unwrap_or_default();
            // Cosine distance in [0,2]; similarity = 1 - dist.
            let score = (1.0 - dist).clamp(-1.0, 1.0);
            hits.push(VectorHit {
                id: id.clone(),
                score,
                metadata,
            });
            if hits.len() >= top_k {
                break;
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(top_k);
        Ok(hits)
    }

    async fn delete(&self, id: &str) -> oneai_core::Result<()> {
        let mut key_to_id = self.key_to_id.lock().await;
        let mut id_to_key = self.id_to_key.lock().await;
        let mut meta = self.meta.lock().await;
        if let Some(key) = id_to_key.remove(id) {
            key_to_id.remove(&key);
            meta.remove(id);
        }
        // The vector remains in the HNSW graph (no cheap remove); it will be
        // skipped at search because key_to_id no longer maps it. Reclaim via
        // periodic rebuild.
        Ok(())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

impl UsearchBackend {
    /// Expose the active config (for diagnostics).
    pub fn config(&self) -> UsearchBackendConfig {
        self.cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hnsw_upsert_search_filter_delete() {
        let be = UsearchBackend::new(4).unwrap();
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
    }

    #[tokio::test]
    async fn upsert_replaces_and_stale_skipped() {
        let be = UsearchBackend::new(4).unwrap();
        be.upsert("a", &[0.1, 0.2, 0.3, 0.4], Metadata::new())
            .await
            .unwrap();
        be.upsert("a", &[0.9, 0.8, 0.7, 0.6], Metadata::new())
            .await
            .unwrap();
        let hits = be.search(&[0.9, 0.8, 0.7, 0.6], 10, None).await.unwrap();
        // Only one 'a' despite the stale graph vector.
        assert_eq!(hits.iter().filter(|h| h.id == "a").count(), 1);
        assert_eq!(hits[0].id, "a");
    }

    #[tokio::test]
    async fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join("oneai-vector-usearch-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idx.usearch");
        let path_str = path.to_string_lossy().to_string();

        {
            let be = UsearchBackend::new(4).unwrap();
            be.upsert(
                "a",
                &[0.1, 0.2, 0.3, 0.4],
                Metadata::from([("k".into(), "x".into())]),
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
            be.save(&path_str).await.unwrap();
        }

        let be = UsearchBackend::open(&path_str, 4).unwrap();
        let hits = be.search(&[0.1, 0.2, 0.3, 0.4], 2, None).await.unwrap();
        assert_eq!(hits[0].id, "a");
        assert_eq!(hits[0].metadata["k"], "x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
