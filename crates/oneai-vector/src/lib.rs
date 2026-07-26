//! OneAI default retrieval backend stack.
//!
//! Implements the retrieval-backend traits declared in [`oneai-core::traits`]
//! (`VectorBackend`, `KeywordBackend`, `RetrievalBackend`, `RerankerProvider`)
//! using the P0-spike-verified library set (2026-07-26):
//!
//! | Backend | Crate | Feature | Role |
//! |---|---|---|---|
//! | [`InMemoryVectorBackend`] | (std only) | always | brute-force cosine, tests/tiny sets |
//! | [`SqliteVecBackend`] | `sqlite-vec` | `sqlite` | exact KNN via `vec0`, mobile/small |
//! | [`UsearchBackend`] | `usearch` | `usearch` | HNSW + mmap + pre-filter |
//! | [`TantivyBm25Backend`] | `tantivy` + `tantivy-jieba` | `tantivy` | CJK BM25 |
//! | [`BgeM3Embedder`] | `ort` | `ort` | local BGE-M3 dense embeddings |
//! | [`BgeRerankerOnnx`] | `ort` | `ort` | local `bge-reranker-v2-m3` cross-encoder |
//! | [`StandardRetrievalPipeline`] | (composes the above) | `default` | BM25+dense → RRF → rerank |
//!
//! ## The non-negotiable default pipeline
//!
//! [`StandardRetrievalPipeline`] runs BM25 + dense → RRF(k=60) → rerank
//! (top-150 → top-K), which Anthropic's "Contextual Retrieval" evaluation
//! showed cuts top-20 retrieval failure rate by 67%. An app that implements
//! its own [`RetrievalBackend`] (e.g. Qdrant, which does hybrid natively)
//! bypasses this pipeline — the trait docs in `oneai-core` say explicitly
//! that skipping the keyword/sparse leg degrades Chinese short queries and
//! unique-identifier lookups, so dropping BM25 is an informed decision.
//!
//! ## Feature flags
//!
//! - `default = ["sqlite", "usearch", "tantivy"]` — the storage backends +
//!   BM25 + the pipeline all build without network access. The pipeline can
//!   take any `EmbeddingService` (the ones in `oneai-rag`, or `BgeM3Embedder`
//!   when `ort` is on).
//! - `ort` — local ONNX embedder + reranker. Off by default because
//!   `ort-sys` downloads a prebuilt `libonnxruntime` blob and the embedder
//!   needs ONNX model files at runtime. Enable with `--features ort`.
//!
//! ## Design principle
//!
//! These implementations must NOT leak storage-internal types — an app that
//! brings Qdrant/Milvus/pgvector/Elasticsearch implements against the public
//! `oneai-core` trait surface, not against these internals. Backends here are
//! the *reference* embedded stack.
//!
//! `unsafe` is confined to the `sqlite-vec` static-registration shim (a single
//! FFI transmute required to register `sqlite3_vec_init` as a SQLite auto
//! extension; every other backend is pure safe Rust over its crate's API).

use std::collections::HashMap;

use oneai_core::Metadata;

pub mod fusion;
pub mod in_memory;
pub mod pipeline;

#[cfg(feature = "sqlite")]
pub mod sqlite_vec;

#[cfg(feature = "usearch")]
pub mod usearch_backend;

#[cfg(feature = "tantivy")]
pub mod tantivy_bm25;

#[cfg(feature = "ort")]
pub mod bge_m3;
#[cfg(feature = "ort")]
pub mod bge_reranker;

pub use fusion::{dbsf_fuse, rrf_fuse};
pub use in_memory::InMemoryVectorBackend;
pub use pipeline::{StandardRetrievalPipeline, StandardRetrievalPipelineBuilder, StandardRetrievalPipelineConfig};

#[cfg(feature = "sqlite")]
pub use sqlite_vec::SqliteVecBackend;
#[cfg(feature = "usearch")]
pub use usearch_backend::UsearchBackend;
#[cfg(feature = "tantivy")]
pub use tantivy_bm25::TantivyBm25Backend;
#[cfg(feature = "ort")]
pub use bge_m3::BgeM3Embedder;
#[cfg(feature = "ort")]
pub use bge_reranker::BgeRerankerOnnx;

// ─── shared helpers ─────────────────────────────────────────────────────────

/// Cosine similarity between two equal-length vectors. Both must be non-empty
/// and the same length; returns 0.0 for a zero-norm query (degenerate, no
/// direction) rather than dividing by ~0.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na.sqrt() * nb.sqrt()).max(1e-12);
    if !denom.is_finite() || denom <= 1e-12 {
        return 0.0;
    }
    dot / denom
}

/// Merge two metadata maps, `b` taking precedence over `a` (used when fusing
/// legs that each carry metadata for the same id).
pub(crate) fn merge_meta(a: &Metadata, b: &Metadata) -> Metadata {
    let mut out = a.clone();
    for (k, v) in b {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Convert a metadata map to a JSON string for storage in single-column backends.
pub(crate) fn meta_to_json(meta: &Metadata) -> String {
    serde_json::to_string(&meta).unwrap_or_else(|_| "{}".into())
}

/// Parse a metadata map from a JSON string; empty/invalid → empty map.
pub(crate) fn meta_from_json(s: &str) -> Metadata {
    serde_json::from_str::<HashMap<String, String>>(s).unwrap_or_default()
}
