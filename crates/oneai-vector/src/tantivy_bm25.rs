//! `KeywordBackend` backed by [`tantivy`] + [`tantivy-jieba`] CJK BM25.
//!
//! The spike verified the search path: `Schema::builder()`,
//! `TextFieldIndexing::default().set_tokenizer("jieba")
//! .set_index_option(IndexRecordOption::WithFreqsAndPositions)` (positions are
//! required — `WithFreqs` raises `FieldDoesNotHavePositionsIndexed` on phrase
//! queries), `set_stored()` to retrieve text via `doc.get_first(..).as_str()`,
//! `writer_with_num_threads(1, ..)`, and `TopDocs::with_limit(n)
//! .order_by_score()` as the collector.
//!
//! BM25 goes through Tantivy (not SQLite FTS5) deliberately: FTS5's CJK story
//! needs a separate tokenizer extension and its `k1`/`b`/`-1 DESC` ordering is a
//! well-known footgun, while Tantivy + jieba gives correct rare-term IDF (the
//! spike showed "人工智能" → score 3.39 on a 4-doc corpus).
//!
//! Deletions use Tantivy's standard `delete_by_term` on an internal indexed
//! `I64` rowid field (one rowid per id; upsert of an existing id deletes the
//! old rowid then adds a new one). The id string and metadata JSON are stored
//! (not indexed) text fields, read back at search time.

use std::collections::HashMap;

use async_trait::async_trait;
use oneai_core::traits::{Filter, KeywordBackend, Metadata, VectorHit};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions, Value};
use tantivy::Term;
use tantivy::{doc, Index, IndexReader, IndexWriter, TantivyDocument};
use tokio::sync::Mutex;

use crate::meta_from_json;
use crate::meta_to_json;

/// Fields captured at schema-build time. Tantivy 0.26 returns typed field
/// handles from `add_*_field`; we stash them so we never re-resolve by name.
struct SchemaFields {
    content: tantivy::schema::Field,
    id_str: tantivy::schema::Field,
    rowid: tantivy::schema::Field,
    meta: tantivy::schema::Field,
}

/// Tantivy BM25 lexical backend.
pub struct TantivyBm25Backend {
    index: Index,
    fields: SchemaFields,
    schema: Schema,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    /// id → rowid (for delete-by-term on upsert/delete).
    id_to_rowid: Mutex<HashMap<String, i64>>,
    next_rowid: Mutex<i64>,
}

impl TantivyBm25Backend {
    /// In-RAM backend.
    pub fn in_memory() -> oneai_core::Result<Self> {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema.clone());
        index
            .tokenizers()
            .register("jieba", tantivy_jieba::JiebaTokenizer::new());
        Self::finish(index, schema, fields)
    }

    /// Open or create a persisted index at `dir` (MMap directory).
    pub fn open_dir(dir: &str) -> oneai_core::Result<Self> {
        let path = std::path::Path::new(dir);
        let exists = path.exists() && path.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false);
        let (schema, fields) = build_schema();
        let index = if exists {
            Index::open_in_dir(path).map_err(|e| oneai_core::OneAIError::Rag(format!("tantivy open {dir}: {e}")))?
        } else {
            std::fs::create_dir_all(path)
                .map_err(|e| oneai_core::OneAIError::Rag(format!("mkdir {dir}: {e}")))?;
            Index::create_in_dir(path, schema.clone())
                .map_err(|e| oneai_core::OneAIError::Rag(format!("tantovy create {dir}: {e}")))?
        };
        index
            .tokenizers()
            .register("jieba", tantivy_jieba::JiebaTokenizer::new());
        Self::finish(index, schema, fields)
    }

    fn finish(index: Index, schema: Schema, fields: SchemaFields) -> oneai_core::Result<Self> {
        let writer = index
            .writer_with_num_threads(1, 50_000_000)
            .map_err(|e| oneai_core::OneAIError::Rag(format!("tantivy writer: {e}")))?;
        let reader = index
            .reader()
            .map_err(|e| oneai_core::OneAIError::Rag(format!("tantivy reader: {e}")))?;
        Ok(Self {
            index,
            fields,
            schema,
            writer: Mutex::new(writer),
            reader,
            id_to_rowid: Mutex::new(HashMap::new()),
            next_rowid: Mutex::new(1),
        })
    }

    /// Expose the schema (for diagnostics / Studio).
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Expose the index (for diagnostics / Studio).
    pub fn index(&self) -> &Index {
        &self.index
    }
}

fn build_schema() -> (Schema, SchemaFields) {
    let mut builder = Schema::builder();
    let content = builder.add_text_field(
        "content",
        TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("jieba")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    // Stored-only id string (read back at search; not indexed — we delete via
    // the rowid field below).
    let id_str = builder.add_text_field("id_str", TextOptions::default().set_stored());
    // Indexed I64 rowid for delete_by_term. Not stored (never read back).
    let rowid = builder.add_i64_field("rowid_i64", NumericOptions::default().set_indexed());
    let meta = builder.add_text_field("meta", TextOptions::default().set_stored());
    let schema = builder.build();
    (schema, SchemaFields { content, id_str, rowid, meta })
}

#[async_trait]
impl KeywordBackend for TantivyBm25Backend {
    async fn upsert_doc(&self, id: &str, text: &str, metadata: Metadata) -> oneai_core::Result<()> {
        let mut writer = self.writer.lock().await;
        let mut id_to_rowid = self.id_to_rowid.lock().await;
        let mut next_rowid = self.next_rowid.lock().await;

        if let Some(old_rowid) = id_to_rowid.get(id).copied() {
            let term = Term::from_field_i64(self.fields.rowid, old_rowid);
            // delete_term returns an Opstamp (infallible) — no error to map.
            writer.delete_term(term);
        }
        let rowid_val = *next_rowid;
        *next_rowid += 1;
        let meta_json = meta_to_json(&metadata);
        let _ = writer
            .add_document(doc!(
                self.fields.content => text,
                self.fields.id_str => id,
                self.fields.rowid => rowid_val,
                self.fields.meta => meta_json,
            ))
            .map_err(|e| oneai_core::OneAIError::Rag(format!("tantivy add: {e}")))?;
        writer
            .commit()
            .map_err(|e| oneai_core::OneAIError::Rag(format!("tantivy commit: {e}")))?;
        id_to_rowid.insert(id.to_string(), rowid_val);
        Ok(())
    }

    async fn search(
        &self,
        query: &str,
        top_k: usize,
        filter: Option<&Filter>,
    ) -> oneai_core::Result<Vec<VectorHit>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        // Make committed writes visible.
        self.reader
            .reload()
            .map_err(|e| oneai_core::OneAIError::Rag(format!("tantivy reload: {e}")))?;
        let searcher = self.reader.searcher();
        let qp = QueryParser::for_index(&self.index, vec![self.fields.content]);
        let qobj = match qp.parse_query(query) {
            Ok(q) => q,
            // Empty/parse-failure query → no hits rather than erroring.
            Err(_) => return Ok(Vec::new()),
        };
        // Over-fetch when filtering so post-filtering still yields top_k.
        let fetch = match filter {
            Some(f) if !f.metadata_eq.is_empty() || !f.metadata_in.is_empty() => (top_k * 4).max(top_k),
            _ => top_k,
        };
        let top = searcher
            .search(&qobj, &TopDocs::with_limit(fetch).order_by_score())
            .map_err(|e| oneai_core::OneAIError::Rag(format!("tantivy search: {e}")))?;

        let mut hits: Vec<VectorHit> = Vec::new();
        for (score, addr) in top {
            let d: TantivyDocument = searcher
                .doc(addr)
                .map_err(|e| oneai_core::OneAIError::Rag(format!("tantivy doc: {e}")))?;
            let id = d
                .get_first(self.fields.id_str)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let meta_json = d
                .get_first(self.fields.meta)
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let metadata = meta_from_json(meta_json);
            if let Some(f) = filter {
                if !f.matches(&metadata) {
                    continue;
                }
            }
            hits.push(VectorHit { id, score, metadata });
            if hits.len() >= top_k {
                break;
            }
        }
        Ok(hits)
    }

    async fn delete(&self, id: &str) -> oneai_core::Result<()> {
        let mut writer = self.writer.lock().await;
        let mut id_to_rowid = self.id_to_rowid.lock().await;
        if let Some(old_rowid) = id_to_rowid.remove(id) {
            let term = Term::from_field_i64(self.fields.rowid, old_rowid);
            writer.delete_term(term);
            writer
                .commit()
                .map_err(|e| oneai_core::OneAIError::Rag(format!("tantivy commit: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs() -> &'static [(&'static str, &'static str)] {
        &[
            ("d1", "今天天气不错适合出门散步"),
            ("d2", "明天可能会下雨记得带伞"),
            ("d3", "天气预报说后天放晴气温回升"),
            ("d4", "机器学习是人工智能的一个分支"),
        ]
    }

    async fn fresh() -> TantivyBm25Backend {
        let be = TantivyBm25Backend::in_memory().unwrap();
        for (id, text) in docs() {
            let mut m = Metadata::new();
            m.insert("tag".into(), if *id == "d4" { "ai".into() } else { "weather".into() });
            be.upsert_doc(id, text, m).await.unwrap();
        }
        be
    }

    #[tokio::test]
    async fn bm25_cjk_rare_term() {
        let be = fresh().await;
        // "人工智能" is a rare term in d4 — expect d4 top with high score.
        let hits = be.search("人工智能", 5, None).await.unwrap();
        assert!(!hits.is_empty(), "expected BM25 hits for 人工智能");
        assert_eq!(hits[0].id, "d4");
        assert!(hits[0].score > 1.0, "rare-term IDF should boost score: {}", hits[0].score);

        // "天气" matches d1/d3 (the spike pattern).
        let hits = be.search("天气", 5, None).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert!(ids.contains(&"d1") || ids.contains(&"d3"));
    }

    #[tokio::test]
    async fn filter_by_metadata() {
        let be = fresh().await;
        let f = Filter::new().with_eq("tag", "weather");
        let hits = be.search("天气", 10, Some(&f)).await.unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.metadata["tag"] == "weather"));
        assert!(hits.iter().all(|h| h.id != "d4"));
    }

    #[tokio::test]
    async fn upsert_replaces_and_delete_removes() {
        let be = TantivyBm25Backend::in_memory().unwrap();
        be.upsert_doc("x", "alpha beta", Metadata::new()).await.unwrap();
        be.upsert_doc("x", "gamma delta", Metadata::new()).await.unwrap();
        // Old content "alpha" must no longer match 'x' after replacement.
        let hits = be.search("alpha", 10, None).await.unwrap();
        assert!(!hits.iter().any(|h| h.id == "x"), "stale content leaked after upsert");
        let hits = be.search("gamma", 10, None).await.unwrap();
        assert!(hits.iter().any(|h| h.id == "x"));

        be.delete("x").await.unwrap();
        let hits = be.search("gamma", 10, None).await.unwrap();
        assert!(!hits.iter().any(|h| h.id == "x"), "deleted doc leaked");
    }
}
