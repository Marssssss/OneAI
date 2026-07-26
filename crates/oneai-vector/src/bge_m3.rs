//! Local BGE-M3 dense embedder over ONNX Runtime ([`EmbeddingService`]).
//!
//! `bge-m3` (BAAI, MIT) is the default embedder for OneAI: 1024-dim dense,
//! 8192 max length, 100+ languages, ~35M downloads. The P0 spike verified
//! the `ort 2.0.0-rc.12` API end-to-end (MiniLM): `Session::builder()?.commit_from_file`,
//! `session.run(ort::inputs![.. => TensorRef::from_array_view(&arr)?])`,
//! `outputs[..].try_extract_tensor::<f32>() -> (&Shape, &[f32])` (raw flat
//! data, indexed by shape — **not** a Tensor with `.view()`).
//!
//! Pooling: mean-pool over non-padded tokens + L2-normalize — matches
//! FlagEmbedding's `BGEM3InferenceFunction` dense path (and the spike's
//! MiniLM). Tokenization uses the `tokenizers` crate loading the model's
//! `tokenizer.json`.
//!
//! Inputs capped at 8192 tokens (BGE-M3 max) by slicing the encoded arrays
//! post-`encode` — avoids depending on the tokenizer's own truncation
//! configuration. The sparse/ColBERT heads of BGE-M3 are not surfaced here;
//! the [`EmbeddingService`] trait is dense-only. Apps needing sparse/ColBERT
//! implement their own backend.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use ndarray::Array2;
use oneai_core::traits::EmbeddingService;
use oneai_core::{EmbeddingModel, OneAIError, Result};
use ort::session::Session;
use ort::value::TensorRef;
use tokenizers::Tokenizer;

const MAX_LEN: usize = 8192;
const DIM: usize = 1024;

/// Local BGE-M3 dense embedder.
pub struct BgeM3Embedder {
    session: Mutex<Session>,
    tokenizer: Mutex<Tokenizer>,
}

impl BgeM3Embedder {
    /// Load from a directory containing `model.onnx` (and `model.onnx_data`
    /// external-data file if the model uses one) and `tokenizer.json`.
    pub fn new(model_dir: &str) -> Result<Self> {
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
        })
    }
}

#[async_trait]
impl EmbeddingService for BgeM3Embedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.is_empty() {
            return Ok(vec![0.0; DIM]);
        }
        let (ids, mask, tt) = self.tokenize(text);
        let seq = ids.len();
        if seq == 0 {
            return Ok(vec![0.0; DIM]);
        }
        let ids_arr = Array2::<i64>::from_shape_vec((1, seq), ids)
            .map_err(|e| OneAIError::Embedding(format!("ndarray ids: {e}")))?;
        let mask_arr = Array2::<i64>::from_shape_vec((1, seq), mask)
            .map_err(|e| OneAIError::Embedding(format!("ndarray mask: {e}")))?;
        let tt_arr = Array2::<i64>::from_shape_vec((1, seq), tt)
            .map_err(|e| OneAIError::Embedding(format!("ndarray tt: {e}")))?;

        let mut session = self.session.lock().map_err(|e| OneAIError::Embedding(format!("session lock: {e}")))?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => TensorRef::from_array_view(&ids_arr).map_err(|e| OneAIError::Embedding(format!("input_ids: {e}")))?,
                "attention_mask" => TensorRef::from_array_view(&mask_arr).map_err(|e| OneAIError::Embedding(format!("mask: {e}")))?,
                "token_type_ids" => TensorRef::from_array_view(&tt_arr).map_err(|e| OneAIError::Embedding(format!("tt: {e}")))?,
            ])
            .map_err(|e| OneAIError::Embedding(format!("ort run: {e}")))?;
        // `outputs` borrows `session` (the tensor data lives in the session's
        // arena); keep the guard alive while we read it, then let it drop at
        // scope end.

        let last = outputs
            .get("last_hidden_state")
            .ok_or_else(|| OneAIError::Embedding("missing last_hidden_state output".into()))?;
        let (shape, data) = last
            .try_extract_tensor::<f32>()
            .map_err(|e| OneAIError::Embedding(format!("extract tensor: {e}")))?;
        let dim = (shape.num_elements() / seq).max(1);

        // Mean-pool over tokens where attention_mask == 1, then L2-normalize.
        let mut emb = vec![0.0f32; dim];
        let mut n = 0usize;
        for s in 0..seq {
            if mask_arr[(0, s)] == 1 {
                let start = s * dim;
                for (emb_v, data_v) in emb.iter_mut().zip(&data[start..start + dim]) {
                    *emb_v += *data_v;
                }
                n += 1;
            }
        }
        if n == 0 {
            return Ok(vec![0.0; dim]);
        }
        let nf = n as f32;
        for v in emb.iter_mut() {
            *v /= nf;
        }
        let norm = emb.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        for v in emb.iter_mut() {
            *v /= norm;
        }
        Ok(emb)
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }

    fn model(&self) -> EmbeddingModel {
        EmbeddingModel::new("bge-m3")
    }

    fn max_input_tokens(&self) -> Option<usize> {
        Some(MAX_LEN)
    }

    fn dimension(&self) -> usize {
        DIM
    }
}

impl BgeM3Embedder {
    /// Tokenize `text` → (input_ids, attention_mask, token_type_ids), each
    /// capped to [`MAX_LEN`] tokens. token_type_ids are all 0 (single
    /// sequence).
    fn tokenize(&self, text: &str) -> (Vec<i64>, Vec<i64>, Vec<i64>) {
        let tokenizer = match self.tokenizer.lock() {
            Ok(t) => t,
            Err(_) => return (Vec::new(), Vec::new(), Vec::new()),
        };
        let enc = match tokenizer.encode(text, true) {
            Ok(e) => e,
            Err(_) => return (Vec::new(), Vec::new(), Vec::new()),
        };
        let ids: Vec<i64> = enc.get_ids().iter().take(MAX_LEN).map(|v| *v as i64).collect();
        let mask: Vec<i64> = enc.get_attention_mask().iter().take(MAX_LEN).map(|v| *v as i64).collect();
        let tt: Vec<i64> = enc.get_type_ids().iter().take(MAX_LEN).map(|v| *v as i64).collect();
        (ids, mask, tt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These require the actual BGE-M3 ONNX model + tokenizer.json on disk.
    // Set BGE_M3_DIR to point at a HF cache snapshot dir containing model.onnx
    // + tokenizer.json. They are #[ignore] so default `cargo test` never
    // depends on a multi-hundred-MB model blob.
    fn dir() -> Option<String> {
        std::env::var("BGE_M3_DIR").ok().filter(|s| !s.is_empty())
    }

    #[tokio::test]
    #[ignore = "needs BGE_M3_DIR with model.onnx + tokenizer.json"]
    async fn smoke_embed() {
        let Some(d) = dir() else {
            eprintln!("BGE_M3_DIR unset — skipping");
            return;
        };
        let emb = BgeM3Embedder::new(&d).expect("load model");
        let v = emb.embed("今天天气不错适合出门散步").await.unwrap();
        assert_eq!(v.len(), 1024);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "embedding not unit-norm: {norm}");
        assert!(emb.health_check().await.is_ok());
    }
}
