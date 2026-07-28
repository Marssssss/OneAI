//! Local `bge-reranker-v2-m3` cross-encoder reranker over ONNX Runtime
//! ([`RerankerProvider`]).
//!
//! `bge-reranker-v2-m3` (BAAI, Apache-2.0, 567M, ~18.5M downloads) is the
//! default second-stage reranker. The P0 spike verified the `ort 2.0.0-rc.12`
//! forward path; this module adds pair tokenization (query+document via the
//! `tokenizers` crate's dual-sequence `encode`) and reads the CLS-position
//! logit as the relevance score.
//!
//! Reranking is the last pipeline step and, per Anthropic's Contextual
//! Retrieval evaluation, cuts retrieval failure rate by 67% on top of
//! hybrid + BM25.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use ndarray::Array2;
use oneai_core::traits::{RankedDoc, RerankDoc, RerankerProvider};
use oneai_core::{OneAIError, Result};
use ort::session::Session;
use ort::value::TensorRef;
use tokenizers::Tokenizer;

const MAX_LEN: usize = 8192;

/// Local ONNX cross-encoder reranker.
pub struct BgeRerankerOnnx {
    session: Mutex<Session>,
    tokenizer: Mutex<Tokenizer>,
    model_name: String,
}

impl BgeRerankerOnnx {
    /// Load from a directory containing `model.onnx` (+ external data) and
    /// `tokenizer.json`.
    pub fn new(model_dir: &str) -> Result<Self> {
        Self::with_name("bge-reranker-v2-m3", model_dir)
    }

    /// Load with an explicit model name (for diagnostics).
    pub fn with_name(name: &str, model_dir: &str) -> Result<Self> {
        let dir = Path::new(model_dir);
        let onnx = dir.join("model.onnx");
        let tok = dir.join("tokenizer.json");
        let session = Session::builder()
            .map_err(|e| OneAIError::Embedding(format!("ort builder: {e}")))?
            .commit_from_file(&onnx)
            .map_err(|e| OneAIError::Embedding(format!("ort load {}: {e}", onnx.display())))?;
        let tokenizer = Tokenizer::from_file(&tok)
            .map_err(|e| OneAIError::Embedding(format!("tokenizer load {}: {e}", tok.display())))?;
        Ok(Self {
            session: Mutex::new(session),
            tokenizer: Mutex::new(tokenizer),
            model_name: name.to_string(),
        })
    }

    /// Score one (query, doc) pair → CLS-position logit.
    fn score(&self, query: &str, doc: &str) -> Result<f32> {
        let (ids, mask, tt) = self.tokenize_pair(query, doc);
        let seq = ids.len();
        if seq == 0 {
            return Ok(0.0);
        }
        let ids_arr = Array2::<i64>::from_shape_vec((1, seq), ids)
            .map_err(|e| OneAIError::Embedding(format!("ndarray ids: {e}")))?;
        let mask_arr = Array2::<i64>::from_shape_vec((1, seq), mask)
            .map_err(|e| OneAIError::Embedding(format!("ndarray mask: {e}")))?;
        let tt_arr = Array2::<i64>::from_shape_vec((1, seq), tt)
            .map_err(|e| OneAIError::Embedding(format!("ndarray tt: {e}")))?;
        let mut session = self
            .session
            .lock()
            .map_err(|e| OneAIError::Embedding(format!("session lock: {e}")))?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => TensorRef::from_array_view(&ids_arr).map_err(|e| OneAIError::Embedding(format!("input_ids: {e}")))?,
                "attention_mask" => TensorRef::from_array_view(&mask_arr).map_err(|e| OneAIError::Embedding(format!("mask: {e}")))?,
                "token_type_ids" => TensorRef::from_array_view(&tt_arr).map_err(|e| OneAIError::Embedding(format!("tt: {e}")))?,
            ])
            .map_err(|e| OneAIError::Embedding(format!("ort run: {e}")))?;
        // `outputs` borrows `session`; keep the guard alive while reading.

        // bge-reranker-v2-m3 emits a single "logits" output (shape [1,1] or
        // [seq,1]); the CLS-position (index 0) logit is the relevance score.
        let logits = outputs
            .get("logits")
            .ok_or_else(|| OneAIError::Embedding("missing logits output".into()))?;
        let (_shape, data) = logits
            .try_extract_tensor::<f32>()
            .map_err(|e| OneAIError::Embedding(format!("extract logits: {e}")))?;
        Ok(data.first().copied().unwrap_or(0.0))
    }

    fn tokenize_pair(&self, query: &str, doc: &str) -> (Vec<i64>, Vec<i64>, Vec<i64>) {
        let tokenizer = match self.tokenizer.lock() {
            Ok(t) => t,
            Err(_) => return (Vec::new(), Vec::new(), Vec::new()),
        };
        // Dual-sequence encode: token_type_ids are 0 for query, 1 for doc.
        let enc = match tokenizer.encode((query, doc), true) {
            Ok(e) => e,
            Err(_) => return (Vec::new(), Vec::new(), Vec::new()),
        };
        let ids: Vec<i64> = enc
            .get_ids()
            .iter()
            .take(MAX_LEN)
            .map(|v| *v as i64)
            .collect();
        let mask: Vec<i64> = enc
            .get_attention_mask()
            .iter()
            .take(MAX_LEN)
            .map(|v| *v as i64)
            .collect();
        let tt: Vec<i64> = enc
            .get_type_ids()
            .iter()
            .take(MAX_LEN)
            .map(|v| *v as i64)
            .collect();
        (ids, mask, tt)
    }
}

#[async_trait]
impl RerankerProvider for BgeRerankerOnnx {
    async fn rerank(
        &self,
        query: &str,
        docs: &[RerankDoc],
        top_n: usize,
    ) -> Result<Vec<RankedDoc>> {
        let mut scored: Vec<(usize, f32)> = Vec::with_capacity(docs.len());
        for (i, d) in docs.iter().enumerate() {
            let s = self.score(query, &d.content)?;
            scored.push((i, s));
        }
        // Descending by score.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_n);
        Ok(scored
            .into_iter()
            .map(|(i, s)| RankedDoc {
                id: docs[i].id.clone(),
                content: docs[i].content.clone(),
                score: s,
            })
            .collect())
    }

    fn model(&self) -> &str {
        &self.model_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> Option<String> {
        std::env::var("BGE_RERANKER_DIR")
            .ok()
            .filter(|s| !s.is_empty())
    }

    #[tokio::test]
    #[ignore = "needs BGE_RERANKER_DIR with model.onnx + tokenizer.json"]
    async fn smoke_rerank() {
        let Some(d) = dir() else {
            eprintln!("BGE_RERANKER_DIR unset — skipping");
            return;
        };
        let rk = BgeRerankerOnnx::new(&d).expect("load model");
        let docs = vec![
            RerankDoc::new("a", "机器学习是人工智能的一个分支"),
            RerankDoc::new("b", "今天天气不错适合出门散步"),
        ];
        let ranked = rk.rerank("人工智能相关内容", &docs, 2).await.unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].id, "a", "reranker should rank the AI doc first");
    }
}
