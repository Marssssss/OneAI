//! Embedding service — vector embedding generation for RAG document indexing and memory search.
//!
//! Core trait and types (`EmbeddingService`, `EmbeddingModel`, `EmbeddingConfig`, etc.)
//! are defined in `oneai_core::traits` and re-exported here.
//!
//! Concrete implementations live in this module:
//! - **OpenAIEmbeddingService**: Via OpenAI's text-embedding API (cloud, high quality)
//! - **VoyageEmbeddingService**: Via the Voyage AI embedding API (cloud, `VOYAGE_API_KEY`)
//! - **OllamaEmbeddingService**: Via Ollama's embedding API (local, no API key needed)
//! - **FastEmbedService**: Local ONNX model via fastembed crate (stub)
//!
//! The EmbeddingServiceRegistry manages service lifecycle, caching, and fallback.
//! Provider auto-detection + build-time/runtime fallback live in
//! [`provider_adapter`](crate::provider_adapter) (`EmbeddingResolver`).
//! AutoEmbeddingDocumentIndex provides zero-config RAG with automatic embedding computation.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};

// Re-export core trait and types from oneai-core
pub use oneai_core::{
    EmbeddingModel,
    EmbeddingService,
    EmbeddingProvider,
    InputType,
    EmbeddingConfig,
    EmbeddingHealthStatus,
    KNOWN_EMBEDDING_DIMENSIONS,
};

// ─── EmbeddingConfig::build_service() extension ──────────────────────────────

/// Extension for EmbeddingConfig to resolve concrete EmbeddingService implementations.
///
/// Implemented in `oneai-rag` (where the concrete service types live) and
/// delegating to [`EmbeddingResolver`](crate::provider_adapter::EmbeddingResolver)
/// so that the config-layer auto-detection + build-time/runtime fallback apply
/// uniformly whether the service is built from `AppBuilder` or the CLI.
pub trait EmbeddingConfigExt {
    /// Resolve an [`EmbeddingService`] (carrying cache + runtime fallback) from
    /// this config via the auto-detection resolver.
    ///
    /// `Auto`/missing-key/unreachable-local configs resolve to `Ok(None)` so
    /// that memory recall falls back to keyword matching rather than hard-failing.
    fn build_service(&self) -> Result<Option<Arc<dyn EmbeddingService>>>;
}

impl EmbeddingConfigExt for EmbeddingConfig {
    fn build_service(&self) -> Result<Option<Arc<dyn EmbeddingService>>> {
        let probe = crate::provider_adapter::EnvProbe::from_env();
        let registry = crate::provider_adapter::EmbeddingResolver::resolve_with(self, &probe)?;
        Ok(registry.map(|r| Arc::new(r) as Arc<dyn EmbeddingService>))
    }
}

// ─── OpenAIEmbeddingService ──────────────────────────────────────────────────

/// OpenAI embedding service — calls OpenAI's text-embedding API.
///
/// Supports text-embedding-3-small (1536-dim) and text-embedding-3-large (3072-dim).
/// Requires an OpenAI API key.
///
/// **API endpoint**: `POST https://api.openai.com/v1/embeddings`
///
/// **Request format**:
/// ```json
/// {
///   "model": "text-embedding-3-small",
///   "input": ["text1", "text2"]
/// }
/// ```
///
/// **Response format**:
/// ```json
/// {
///   "data": [
///     { "embedding": [0.1, 0.2, ...], "index": 0 },
///     { "embedding": [0.3, 0.4, ...], "index": 1 }
///   ],
///   "model": "text-embedding-3-small",
///   "usage": { "prompt_tokens": 10, "total_tokens": 10 }
/// }
/// ```
pub struct OpenAIEmbeddingService {
    /// The embedding model to use.
    model: EmbeddingModel,
    /// OpenAI API key.
    api_key: String,
    /// Base URL (default: https://api.openai.com/v1, customizable for compatible APIs).
    base_url: String,
    /// HTTP client.
    client: reqwest::Client,
}

impl OpenAIEmbeddingService {
    /// Create with an OpenAI API key and model.
    pub fn new(api_key: String, model: EmbeddingModel) -> Self {
        Self {
            model,
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Create with a custom base URL (for OpenAI-compatible APIs like DeepSeek, 智谱).
    pub fn with_base_url(api_key: String, model: EmbeddingModel, base_url: String) -> Self {
        Self {
            model,
            api_key,
            base_url,
            client: reqwest::Client::new(),
        }
    }

    /// Create with a custom HTTP client.
    pub fn with_client(api_key: String, model: EmbeddingModel, base_url: String, client: reqwest::Client) -> Self {
        Self { model, api_key, base_url, client }
    }

    /// Get the embeddings API endpoint URL.
    fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl EmbeddingService for OpenAIEmbeddingService {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let texts = [text.to_string()];
        let embeddings = self.embed_batch(&texts).await?;
        embeddings.into_iter().next()
            .ok_or_else(|| OneAIError::Embedding("OpenAI embedding returned no results".to_string()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Enforce provider input limits — split over-size inputs on UTF-8 byte
        // boundaries (token count ≤ byte count) before sending.
        let texts = crate::chunk_split::enforce_max_input_tokens(self, texts);
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let request_body = serde_json::json!({
            "model": self.model.as_str(),
            "input": texts,
        });

        let response = self.client
            .post(self.embeddings_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| OneAIError::Embedding(format!("OpenAI embedding HTTP error: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OneAIError::Embedding(format!(
                "OpenAI embedding API error: status {} — {}", status, body
            )));
        }

        let response_json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| OneAIError::Embedding(format!("OpenAI embedding response parse error: {}", e)))?;

        let data_array = response_json.get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| OneAIError::Embedding("OpenAI embedding response missing 'data' field".to_string()))?;

        let mut sorted_data: Vec<(usize, Vec<f32>)> = Vec::new();
        for entry in data_array {
            let index = entry.get("index")
                .and_then(|i| i.as_u64())
                .ok_or_else(|| OneAIError::Embedding("OpenAI embedding entry missing 'index'".to_string()))? as usize;

            let embedding_array = entry.get("embedding")
                .and_then(|e| e.as_array())
                .ok_or_else(|| OneAIError::Embedding("OpenAI embedding entry missing 'embedding'".to_string()))?;

            let embedding: Vec<f32> = embedding_array.iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect();

            sorted_data.push((index, embedding));
        }

        sorted_data.sort_by_key(|(idx, _)| *idx);

        if sorted_data.len() != texts.len() {
            return Err(OneAIError::Embedding(format!(
                "OpenAI embedding returned {} results for {} inputs",
                sorted_data.len(), texts.len()
            )));
        }

        Ok(sorted_data.into_iter().map(|(_, emb)| emb).collect())
    }

    /// OpenAI text-embedding-3-* models cap at 8192 input tokens.
    fn max_input_tokens(&self) -> Option<usize> {
        match self.model.as_str() {
            "text-embedding-3-small" | "text-embedding-3-large" => Some(8192),
            "text-embedding-ada-002" => Some(8191),
            _ => None,
        }
    }

    fn model(&self) -> EmbeddingModel {
        self.model.clone()
    }
}

// ─── VoyageEmbeddingService ─────────────────────────────────────────────────

/// Voyage embedding service — calls the Voyage AI embedding API.
///
/// Supports voyage-3 (1024-dim) and voyage-3-lite (512-dim). Requires a
/// `VOYAGE_API_KEY`. (Anthropic itself has no native embedding API — its
/// embedding capability is the Voyage service, acquired by Anthropic.)
///
/// **API endpoint**: `POST https://api.voyageai.com/v1/embeddings`
///
/// **Request format**:
/// ```json
/// {
///   "model": "voyage-3",
///   "input": ["text1", "text2"]
/// }
/// ```
///
/// **Response format**:
/// ```json
/// {
///   "data": [
///     { "embedding": [0.1, 0.2, ...], "index": 0 },
///     { "embedding": [0.3, 0.4, ...], "index": 1 }
///   ],
///   "model": "voyage-3",
///   "usage": { "input_tokens": 10 }
/// }
/// ```
pub struct VoyageEmbeddingService {
    /// The embedding model to use.
    model: EmbeddingModel,
    /// Voyage API key (`VOYAGE_API_KEY`).
    api_key: String,
    /// Base URL (default: https://api.voyageai.com/v1).
    base_url: String,
    /// HTTP client.
    client: reqwest::Client,
}

impl VoyageEmbeddingService {
    /// Create with a Voyage API key and model.
    pub fn new(api_key: String, model: EmbeddingModel) -> Self {
        Self {
            model,
            api_key,
            base_url: "https://api.voyageai.com/v1".to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Create with a custom base URL.
    pub fn with_base_url(api_key: String, model: EmbeddingModel, base_url: String) -> Self {
        Self { model, api_key, base_url, client: reqwest::Client::new() }
    }

    /// Create with a custom HTTP client.
    pub fn with_client(api_key: String, model: EmbeddingModel, base_url: String, client: reqwest::Client) -> Self {
        Self { model, api_key, base_url, client }
    }

    fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl EmbeddingService for VoyageEmbeddingService {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let texts = [text.to_string()];
        let embeddings = self.embed_batch(&texts).await?;
        embeddings.into_iter().next()
            .ok_or_else(|| OneAIError::Embedding("Voyage embedding returned no results".to_string()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Enforce provider input limits — split over-size inputs on UTF-8 byte
        // boundaries (token count ≤ byte count) before sending.
        let texts = crate::chunk_split::enforce_max_input_tokens(self, texts);
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let request_body = serde_json::json!({
            "model": self.model.as_str(),
            "input": texts,
        });

        let response = self.client
            .post(self.embeddings_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| OneAIError::Embedding(format!("Voyage embedding HTTP error: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OneAIError::Embedding(format!(
                "Voyage embedding API error: status {} — {}", status, body
            )));
        }

        let response_json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| OneAIError::Embedding(format!("Voyage embedding response parse error: {}", e)))?;

        let data_array = response_json.get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| OneAIError::Embedding("Voyage embedding response missing 'data' field".to_string()))?;

        let mut sorted_data: Vec<(usize, Vec<f32>)> = Vec::new();
        for entry in data_array {
            let index = entry.get("index")
                .and_then(|i| i.as_u64())
                .ok_or_else(|| OneAIError::Embedding("Voyage embedding entry missing 'index'".to_string()))? as usize;

            let embedding_array = entry.get("embedding")
                .and_then(|e| e.as_array())
                .ok_or_else(|| OneAIError::Embedding("Voyage embedding entry missing 'embedding'".to_string()))?;

            let embedding: Vec<f32> = embedding_array.iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect();

            sorted_data.push((index, embedding));
        }

        sorted_data.sort_by_key(|(idx, _)| *idx);

        if sorted_data.len() != texts.len() {
            return Err(OneAIError::Embedding(format!(
                "Voyage embedding returned {} results for {} inputs",
                sorted_data.len(), texts.len()
            )));
        }

        Ok(sorted_data.into_iter().map(|(_, emb)| emb).collect())
    }

    /// Voyage models accept up to 32k input tokens.
    fn max_input_tokens(&self) -> Option<usize> {
        Some(32_768)
    }

    fn model(&self) -> EmbeddingModel {
        self.model.clone()
    }
}

// ─── OllamaEmbeddingService ──────────────────────────────────────────────────

/// Ollama embedding service — calls Ollama's local embedding API.
///
/// Uses Ollama's `/api/embeddings` endpoint with the configured model.
/// No API key needed — Ollama runs locally.
///
/// **API endpoint**: `POST http://localhost:11434/api/embeddings`
///
/// Note: Ollama's embedding API currently supports single-text embedding only.
/// For batch embedding, we make sequential calls.
pub struct OllamaEmbeddingService {
    /// The Ollama model name (e.g., "nomic-embed-text", "mxbai-embed-large").
    model_name: String,
    /// Ollama base URL (default: http://localhost:11434).
    base_url: String,
    /// HTTP client.
    client: reqwest::Client,
}

impl OllamaEmbeddingService {
    /// Create with default Ollama URL and model (nomic-embed-text).
    pub fn new() -> Self {
        Self {
            model_name: "nomic-embed-text".to_string(),
            base_url: "http://localhost:11434".to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Create with a custom Ollama model.
    pub fn with_model(model_name: String) -> Self {
        Self {
            model_name,
            base_url: "http://localhost:11434".to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Create with a custom Ollama URL.
    pub fn with_url(base_url: String) -> Self {
        Self {
            model_name: "nomic-embed-text".to_string(),
            base_url,
            client: reqwest::Client::new(),
        }
    }

    /// Create with custom URL and model.
    pub fn with_url_and_model(base_url: String, model_name: String) -> Self {
        Self { model_name, base_url, client: reqwest::Client::new() }
    }

    /// Create with a custom HTTP client.
    pub fn with_client(base_url: String, model_name: String, client: reqwest::Client) -> Self {
        Self { model_name, base_url, client }
    }

    fn embeddings_url(&self) -> String {
        format!("{}/api/embeddings", self.base_url.trim_end_matches('/'))
    }
}

impl Default for OllamaEmbeddingService {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl EmbeddingService for OllamaEmbeddingService {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let request_body = serde_json::json!({
            "model": self.model_name,
            "prompt": text,
        });

        let response = self.client
            .post(self.embeddings_url())
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| OneAIError::Embedding(format!("Ollama embedding HTTP error: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OneAIError::Embedding(format!(
                "Ollama embedding API error: status {} — {}", status, body
            )));
        }

        let response_json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| OneAIError::Embedding(format!("Ollama embedding response parse error: {}", e)))?;

        let embedding_array = response_json.get("embedding")
            .and_then(|e| e.as_array())
            .ok_or_else(|| OneAIError::Embedding("Ollama embedding response missing 'embedding' field".to_string()))?;

        let embedding: Vec<f32> = embedding_array.iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();

        Ok(embedding)
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Ollama's API currently supports single-text embedding only,
        // so we make sequential calls for batch.
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            let embedding = self.embed(text).await?;
            results.push(embedding);
        }
        Ok(results)
    }

    fn model(&self) -> EmbeddingModel {
        EmbeddingModel::new(self.model_name.clone())
    }
}

// ─── FastEmbedService ───────────────────────────────────────────────────────

/// FastEmbed embedding service — local ONNX model via fastembed crate.
///
/// This is the **recommended default** embedding service:
/// - No API key required
/// - Cross-platform (ONNX Runtime supports all target platforms)
/// - Good Chinese language support (AllMiniLML6V2)
/// - Fast inference (~50ms per embedding on desktop)
/// - Small model size (~22MB for AllMiniLML6V2)
///
/// **Note**: Full implementation requires the `fastembed` crate dependency.
/// Currently provides a stub that returns deterministic test embeddings
/// for development/testing. When fastembed is added as a dependency,
/// the `embed()` and `embed_batch()` methods will use real ONNX inference.
/// FastEmbed embedding service — local ONNX model via the `fastembed` crate.
///
/// **Zero-config, offline-capable**: no API key, no network at steady state
/// (the model is downloaded from HuggingFace on first use and cached). This is
/// the auto-detection chain's last resort so users with no embedding key / no
/// local Ollama still get real semantic recall (vs. keyword matching).
///
/// Model files are downloaded lazily on the first `embed()` call (construction
/// is free); if the one-time download fails (offline, disk), `embed()` returns
/// an error that `MemoryManager`'s fail-safe catches → keyword-recall fallback.
pub struct FastEmbedService {
    /// The embedding model name (mapped to a `fastembed::EmbeddingModel`).
    model: EmbeddingModel,
    /// Lazily-initialized ONNX model (downloaded+loaded on first `embed()`).
    /// `embed()` needs `&mut self`, so a sync mutex guards init + inference;
    /// the guard is never held across an `.await`.
    inner: std::sync::Mutex<Option<fastembed::TextEmbedding>>,
}

impl FastEmbedService {
    /// Create a FastEmbedService with the default model (AllMiniLML6V2, 384-dim).
    pub fn new() -> Self {
        Self {
            model: EmbeddingModel::allminilm_l6_v2(),
            inner: std::sync::Mutex::new(None),
        }
    }

    /// Create with a specific model name (must map to a known fastembed model;
    /// unknown names fall back to AllMiniLML6V2).
    pub fn with_model(model: EmbeddingModel) -> Self {
        Self {
            model,
            inner: std::sync::Mutex::new(None),
        }
    }

    /// Map the configured model-name string to a `fastembed::EmbeddingModel`.
    /// Unknown names default to AllMiniLML6V2 (the auto-chain fallback model).
    fn fe_model(&self) -> fastembed::EmbeddingModel {
        use fastembed::EmbeddingModel as M;
        match self.model.as_str() {
            "bge-base-en-v1.5" => M::BGEBaseENV15,
            "bge-large-en-v1.5" => M::BGELargeENV15,
            "mixedbread-embed-large-v1" | "mxbai-embed-large-v1" => M::MxbaiEmbedLargeV1,
            "all-MiniLM-L6-v2" | _ => M::AllMiniLML6V2,
        }
    }

    /// The HuggingFace repo id fastembed downloads this model from (matches
    /// `fastembed`'s `model_code`).
    fn fe_repo_code(&self) -> &'static str {
        match self.model.as_str() {
            "bge-base-en-v1.5" => "Xenova/bge-base-en-v1.5",
            "bge-large-en-v1.5" => "Xenova/bge-large-en-v1.5",
            "mixedbread-embed-large-v1" | "mxbai-embed-large-v1" => "mixedbread-ai/mxbai-embed-large-v1",
            "all-MiniLM-L6-v2" | _ => "Qdrant/all-MiniLM-L6-v2-onnx",
        }
    }

    /// Resolve the model's cache dir across the places fastembed/hf-hub may
    /// store it: `HF_HOME`, the conventional `~/.cache/huggingface` (hf-hub
    /// default, where `download_fastembed_models.sh` writes), `FASTEMBED_CACHE_DIR`,
    /// or fastembed's `.fastembed_cache`. Returns the first existing repo dir.
    fn cache_repo_dir(&self) -> Option<std::path::PathBuf> {
        let repo = self.fe_repo_code();
        let repo_segment = format!("models--{}", repo.replace('/', "--"));
        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(h) = std::env::var("HF_HOME") {
            roots.push(std::path::PathBuf::from(h));
        }
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(std::path::PathBuf::from(&home).join(".cache/huggingface"));
        }
        if let Ok(f) = std::env::var("FASTEMBED_CACHE_DIR") {
            roots.push(std::path::PathBuf::from(f));
        }
        roots.push(std::path::PathBuf::from(fastembed::get_cache_dir())); // .fastembed_cache (cwd)
        roots
            .into_iter()
            .map(|r| r.join("hub").join(&repo_segment))
            .find(|p| p.exists())
    }

    /// If the model was pre-fetched into the hf-hub cache (e.g. by
    /// `scripts/download_fastembed_models.sh`), load it directly as a
    /// `UserDefinedEmbeddingModel` — bypassing hf-hub's network resolution
    /// entirely (hf-hub's ureq client ignores proxy env on some setups, so this
    /// is the reliable offline path). Returns None if not cached.
    fn try_load_from_cache(&self) -> Option<fastembed::TextEmbedding> {
        let repo_dir = self.cache_repo_dir()?;
        let sha = std::fs::read_to_string(repo_dir.join("refs").join("main")).ok()?;
        let snap = repo_dir.join("snapshots").join(sha.trim());
        // Some repos put the onnx at `model.onnx` (Qdrant), others at
        // `onnx/model.onnx` (Xenova). Try both.
        let onnx = std::fs::read(snap.join("model.onnx"))
            .or_else(|_| std::fs::read(snap.join("onnx").join("model.onnx")))
            .ok()?;
        let read = |f: &str| std::fs::read(snap.join(f)).ok();
        let tok = read("tokenizer.json")?;
        let cfg = read("config.json")?;
        let stm = read("special_tokens_map.json")?;
        let tcfg = read("tokenizer_config.json")?;
        let files = fastembed::TokenizerFiles {
            tokenizer_file: tok,
            config_file: cfg,
            special_tokens_map_file: stm,
            tokenizer_config_file: tcfg,
        };
        let udm = fastembed::UserDefinedEmbeddingModel::new(onnx, files);
        fastembed::TextEmbedding::try_new_from_user_defined(
            udm,
            fastembed::InitOptionsUserDefined::new(),
        )
        .ok()
    }

    /// Lazily download+load the ONNX model on first use, then run a batch
    /// embedding. Construction is free; the one-time download happens here.
    /// The guard is never held across an `.await`, so a sync mutex is safe.
    async fn embed_internal(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut guard = self.inner.lock().map_err(|e| {
            OneAIError::Embedding(format!("fastembed mutex poisoned: {e}"))
        })?;
        if guard.is_none() {
            // 1) Prefer a pre-fetched cache (offline, proxy-safe).
            // 2) Fall back to hf-hub download (works where ureq's network is
            //    reachable — normal envs with direct HF access).
            let model = self.try_load_from_cache().or_else(|| {
                let opts = fastembed::TextInitOptions::new(self.fe_model())
                    .with_show_download_progress(false);
                fastembed::TextEmbedding::try_new(opts).ok()
            }).ok_or_else(|| OneAIError::Embedding(
                "fastembed init failed: model not cached and network download unavailable. \
                 Run `scripts/download_fastembed_models.sh` to pre-fetch the ONNX model."
                    .to_string()
            ))?;
            *guard = Some(model);
        }
        let model = guard.as_mut().expect("just-initialized fastembed model");
        let embeddings = model.embed(texts, None).map_err(|e| {
            OneAIError::Embedding(format!("fastembed embed failed: {e}"))
        })?;
        Ok(embeddings)
    }
}

impl Default for FastEmbedService {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl EmbeddingService for FastEmbedService {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let out = self.embed_internal(vec![text.to_string()]).await?;
        out.into_iter()
            .next()
            .ok_or_else(|| OneAIError::Embedding("FastEmbed returned no embedding".to_string()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_internal(texts.to_vec()).await
    }

    fn model(&self) -> EmbeddingModel {
        self.model.clone()
    }
}

// ─── EmbeddingServiceRegistry ────────────────────────────────────────────────

/// Registry for managing embedding services.
///
/// Provides a unified accessor for the configured service and supports
/// runtime fallback. The registry holds a primary service and optional
/// fallback services for resilience.
///
/// **Usage**:
/// ```ignore
/// let registry = EmbeddingServiceRegistry::new(Arc::new(FastEmbedService::new()));
///
/// // Generate embeddings via registry (uses primary service)
/// let embedding = registry.embed("hello world").await?;
///
/// // Add a fallback service (used if primary fails)
/// let registry = registry.with_fallback(Arc::new(OpenAIEmbeddingService::new(api_key, EmbeddingModel::openai_small())));
///
/// // Health check
/// let status = registry.health_check().await?;
/// assert!(status.is_functional());
/// ```
pub struct EmbeddingServiceRegistry {
    /// Primary embedding service.
    primary: Arc<dyn EmbeddingService>,
    /// Fallback embedding service (optional — used if primary fails).
    fallback: Option<Arc<dyn EmbeddingService>>,
    /// Cache for computed embeddings (text → embedding).
    cache: Arc<tokio::sync::RwLock<HashMap<String, Vec<f32>>>>,
    /// Whether caching is enabled.
    cache_enabled: bool,
}

impl EmbeddingServiceRegistry {
    /// Create a registry with a primary embedding service.
    pub fn new(primary: Arc<dyn EmbeddingService>) -> Self {
        Self {
            primary,
            fallback: None,
            cache: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cache_enabled: true,
        }
    }

    /// Create without caching.
    pub fn without_cache(primary: Arc<dyn EmbeddingService>) -> Self {
        Self {
            primary,
            fallback: None,
            cache: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cache_enabled: false,
        }
    }

    /// Set the fallback embedding service.
    pub fn with_fallback(mut self, fallback: Arc<dyn EmbeddingService>) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// Health status — probe the primary (and fallback) services.
    pub async fn health_status(&self) -> Result<EmbeddingHealthStatus> {
        let primary_healthy = self.primary.health_check().await.is_ok();
        let fallback_healthy = if let Some(fallback) = &self.fallback {
            Some(fallback.health_check().await.is_ok())
        } else {
            None
        };

        Ok(EmbeddingHealthStatus::new(
            self.primary.model().as_str().to_string(),
            primary_healthy,
            self.fallback.as_ref().map(|f| f.model().as_str().to_string()),
            fallback_healthy,
            self.cache_enabled,
            self.cache.read().await.len(),
        ))
    }

    /// Clear the embedding cache.
    pub async fn clear_cache(&self) {
        self.cache.write().await.clear();
    }

    /// Get the cache size.
    pub async fn cache_size(&self) -> usize {
        self.cache.read().await.len()
    }
}

/// The registry itself is an [`EmbeddingService`]: it transparently adds
/// caching and primary→fallback runtime switching to whatever the resolver
/// wired as primary/fallback. This lets `MemoryManager` take a single
/// `Arc<dyn EmbeddingService>` and still get fallback behavior.
#[async_trait]
impl EmbeddingService for EmbeddingServiceRegistry {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Check cache first
        if self.cache_enabled {
            let cache = self.cache.read().await;
            if let Some(embedding) = cache.get(text) {
                return Ok(embedding.clone());
            }
        }

        // Try primary service
        let result = self.primary.embed(text).await;

        let embedding = match result {
            Ok(emb) => emb,
            Err(primary_err) => {
                // Try fallback if available
                if let Some(fallback) = &self.fallback {
                    tracing::warn!("Primary embedding service failed: {} — trying fallback", primary_err);
                    fallback.embed(text).await?
                } else {
                    return Err(primary_err);
                }
            }
        };

        // Cache the result
        if self.cache_enabled {
            let mut cache = self.cache.write().await;
            cache.insert(text.to_string(), embedding.clone());
        }

        Ok(embedding)
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Check cache for any already-computed embeddings
        let mut results = Vec::with_capacity(texts.len());
        let mut uncached_indices = Vec::new();
        let mut uncached_texts = Vec::new();

        if self.cache_enabled {
            let cache = self.cache.read().await;
            for (i, text) in texts.iter().enumerate() {
                if let Some(embedding) = cache.get(text) {
                    results.push((i, embedding.clone()));
                } else {
                    uncached_indices.push(i);
                    uncached_texts.push(text.clone());
                }
            }
        } else {
            uncached_indices = (0..texts.len()).collect();
            uncached_texts = texts.to_vec();
        }

        // Compute uncached embeddings
        if !uncached_texts.is_empty() {
            let batch_result = self.primary.embed_batch(&uncached_texts).await;

            let batch_embeddings = match batch_result {
                Ok(embs) => embs,
                Err(primary_err) => {
                    if let Some(fallback) = &self.fallback {
                        tracing::warn!("Primary embedding batch failed: {} — trying fallback", primary_err);
                        fallback.embed_batch(&uncached_texts).await?
                    } else {
                        return Err(primary_err);
                    }
                }
            };

            // Cache new results
            if self.cache_enabled {
                let mut cache = self.cache.write().await;
                for (text, embedding) in uncached_texts.iter().zip(batch_embeddings.iter()) {
                    cache.insert(text.clone(), embedding.clone());
                }
            }

            for (idx, embedding) in uncached_indices.iter().zip(batch_embeddings.iter()) {
                results.push((*idx, embedding.clone()));
            }
        }

        // Sort by original index
        results.sort_by_key(|(idx, _)| *idx);
        Ok(results.into_iter().map(|(_, emb)| emb).collect())
    }

    fn max_input_tokens(&self) -> Option<usize> {
        self.primary.max_input_tokens()
    }

    fn model(&self) -> EmbeddingModel {
        self.primary.model()
    }
}

// ─── AutoEmbeddingDocumentIndex ──────────────────────────────────────────────

/// Document index with automatic embedding generation.
///
/// Wraps a DocumentIndex and an EmbeddingService to automatically
/// compute embeddings when documents are added. No manual
/// `add_embedding()` calls needed.
///
/// **Usage**:
/// ```ignore
/// let embedding_service = Arc::new(FastEmbedService::new());
/// let vector_store = Arc::new(ThreadSafeEmbeddedVectorStore::new());
/// let index = AutoEmbeddingDocumentIndex::new(vector_store, embedding_service);
///
/// // Add a document — embeddings are automatically computed
/// let doc = Document::with_id("doc1", "Rust is a programming language");
/// let chunk_ids = index.add_document(doc).await?;
///
/// // Search by text — query embedding is automatically computed
/// let results = index.search_by_text("programming language", 5).await?;
/// ```
pub struct AutoEmbeddingDocumentIndex {
    /// The underlying document index.
    index: crate::index::DocumentIndex,
    /// The embedding service for automatic embedding generation.
    embedding_service: Arc<dyn EmbeddingService>,
}

impl AutoEmbeddingDocumentIndex {
    /// Create with a vector store and embedding service.
    pub fn new(
        vector_store: Arc<dyn oneai_core::traits::VectorStore>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            index: crate::index::DocumentIndex::with_defaults(vector_store),
            embedding_service,
        }
    }

    /// Create with custom chunking strategy.
    pub fn with_strategy(
        vector_store: Arc<dyn oneai_core::traits::VectorStore>,
        chunking_strategy: crate::document::ChunkingStrategy,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            index: crate::index::DocumentIndex::new(vector_store, chunking_strategy),
            embedding_service,
        }
    }

    /// Add a document with automatic embedding generation.
    pub async fn add_document(&mut self, document: crate::document::Document) -> Result<Vec<String>> {
        let chunk_ids = self.index.add_document(document)?;

        let chunk_texts: Vec<String> = chunk_ids.iter()
            .filter_map(|id| self.index.get_chunk(id).map(|ic| ic.chunk.content.clone()))
            .collect();

        if chunk_texts.is_empty() {
            return Ok(chunk_ids);
        }

        let embeddings = self.embedding_service.embed_batch(&chunk_texts).await?;

        for (chunk_id, embedding) in chunk_ids.iter().zip(embeddings.iter()) {
            self.index.add_embedding(chunk_id, embedding.clone()).await?;
        }

        Ok(chunk_ids)
    }

    /// Search by text — automatically computes query embedding.
    pub async fn search_by_text(&self, query_text: &str, top_k: usize) -> Result<Vec<crate::retrieval::RetrievalResult>> {
        let query_embedding = self.embedding_service.embed(query_text).await?;
        self.index.search(query_embedding, top_k).await
    }

    /// Search by pre-computed embedding.
    pub async fn search_by_embedding(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<crate::retrieval::RetrievalResult>> {
        self.index.search(query_embedding, top_k).await
    }

    /// Keyword search (no embedding needed).
    pub fn search_by_keyword(&self, keyword: &str, top_k: usize) -> Vec<crate::retrieval::RetrievalResult> {
        self.index.search_by_keyword(keyword, top_k)
    }

    /// Remove a document and all its chunks.
    pub async fn remove_document(&mut self, document_id: &str) -> Result<()> {
        self.index.remove_document(document_id).await
    }

    /// Get chunk count.
    pub fn chunk_count(&self) -> usize {
        self.index.chunk_count()
    }

    /// Get document count.
    pub fn document_count(&self) -> usize {
        self.index.document_count()
    }

    /// Get the embedding service.
    pub fn embedding_service(&self) -> &Arc<dyn EmbeddingService> {
        &self.embedding_service
    }

    /// Get the underlying document index.
    pub fn index(&self) -> &crate::index::DocumentIndex {
        &self.index
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_model_dimension() {
        assert_eq!(EmbeddingModel::allminilm_l6_v2().dimension(), 384);
        assert_eq!(EmbeddingModel::openai_small().dimension(), 1536);
        assert_eq!(EmbeddingModel::openai_large().dimension(), 3072);
        assert_eq!(EmbeddingModel::voyage3().dimension(), 1024);
        assert_eq!(EmbeddingModel::voyage3_lite().dimension(), 512);
        // unknown model name → 0 (runtime-probed)
        assert_eq!(EmbeddingModel::new("nomic-embed-text").dimension(), 0);
    }

    #[test]
    fn test_embedding_model_name() {
        assert_eq!(EmbeddingModel::openai_small().as_str(), "text-embedding-3-small");
        assert_eq!(EmbeddingModel::voyage3().as_str(), "voyage-3");
        assert_eq!(EmbeddingModel::nomic_embed_text().as_str(), "nomic-embed-text");
    }

    #[test]
    fn test_embedding_model_case_insensitive_dim_lookup() {
        assert_eq!(EmbeddingModel::new("TEXT-EMBEDDING-3-SMALL").dimension(), 1536);
    }

    #[test]
    fn test_embedding_config_default_is_auto() {
        let config = EmbeddingConfig::default();
        assert_eq!(config.provider, EmbeddingProvider::Auto);
        assert!(config.api_key.is_none());
        assert!(config.model.is_none());
    }

    #[test]
    fn test_embedding_config_openai() {
        let config = EmbeddingConfig::openai("sk-test".to_string());
        assert_eq!(config.provider, EmbeddingProvider::OpenAi);
        assert_eq!(config.api_key, Some("sk-test".to_string()));
    }

    #[test]
    fn test_embedding_config_voyage() {
        let config = EmbeddingConfig::voyage("pa-test".to_string());
        assert_eq!(config.provider, EmbeddingProvider::Voyage);
    }

    #[test]
    fn test_embedding_config_ollama() {
        let config = EmbeddingConfig::ollama().with_model("mxbai-embed-large");
        assert_eq!(config.provider, EmbeddingProvider::Ollama);
        assert_eq!(config.model.as_ref().unwrap().as_str(), "mxbai-embed-large");
    }

    // ─── Resolver-backed build_service tests (deterministic, no network) ──

    fn resolve(cfg: &EmbeddingConfig) -> Option<EmbeddingServiceRegistry> {
        use crate::provider_adapter::{EmbeddingResolver, EnvProbe};
        EmbeddingResolver::resolve_with(cfg, &EnvProbe::empty()).unwrap()
    }

    #[test]
    fn test_resolve_auto_no_keys_falls_back_to_fastembed() {
        // Auto with no keys/ollama → FastEmbed local ONNX (real semantic recall),
        // NOT None — so keyless users still get semantic memory/RAG.
        assert!(resolve(&EmbeddingConfig::default()).is_some());
    }

    #[test]
    fn test_resolve_openai_with_key() {
        let cfg = EmbeddingConfig::openai("sk-test".to_string());
        let reg = resolve(&cfg).expect("explicit openai with key should resolve");
        assert_eq!(reg.model().as_str(), "text-embedding-3-small");
    }

    #[test]
    fn test_resolve_openai_no_key_returns_none_gracefully() {
        // explicit provider, key absent → Ok(None), not Err
        let mut cfg = EmbeddingConfig::default();
        cfg.provider = EmbeddingProvider::OpenAi;
        assert!(resolve(&cfg).is_none());
    }

    #[test]
    fn test_resolve_voyage_with_key() {
        let cfg = EmbeddingConfig::voyage("pa-test".to_string());
        let reg = resolve(&cfg).expect("explicit voyage with key should resolve");
        assert_eq!(reg.model().as_str(), "voyage-3");
    }

    #[test]
    fn test_resolve_ollama_with_explicit_base_url() {
        // explicit base_url → available without TCP probe; create is offline.
        let cfg = EmbeddingConfig::ollama().with_base_url("http://localhost:11434");
        let reg = resolve(&cfg).expect("explicit ollama with base_url should resolve");
        assert_eq!(reg.model().as_str(), "nomic-embed-text");
    }

    // ─── Stub embedding service (no download) for registry/cache tests ───

    /// Deterministic, dependency-free embedding service used to exercise the
    /// registry's cache/fallback logic without triggering a real ONNX model
    /// download. Real FastEmbed inference is covered by the `#[ignore]`'d
    /// tests below (run with `--ignored` once the model is cached).
    struct StubEmbed {
        dim: usize,
        model: EmbeddingModel,
    }
    impl StubEmbed {
        fn new() -> Self { Self { dim: 384, model: EmbeddingModel::allminilm_l6_v2() } }
        fn with_dim_model(dim: usize, model: EmbeddingModel) -> Self { Self { dim, model } }
        fn vec_for(&self, text: &str) -> Vec<f32> {
            let mut h: u64 = 5381;
            for b in text.bytes() { h = h.wrapping_mul(33).wrapping_add(b as u64); }
            (0..self.dim).map(|i| {
                let s = h.wrapping_add(i as u64);
                ((s % 1000) as f32 / 1000.0 - 0.5) * 0.1
            }).collect()
        }
    }
    #[async_trait]
    impl EmbeddingService for StubEmbed {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> { Ok(self.vec_for(text)) }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| self.vec_for(t)).collect())
        }
        fn model(&self) -> EmbeddingModel { self.model.clone() }
    }

    // ─── FastEmbedService real-inference tests (need a cached model) ──────
    // These trigger a one-time model download from HuggingFace on first run
    // (~22MB AllMiniLML6V2), cached in HF_HOME so subsequent runs are offline.
    // Marked `#[ignore]` so the suite stays green without network; run with
    // `cargo test -p oneai-rag -- --ignored` once the model is available.

    #[tokio::test]
    #[ignore = "needs the AllMiniLML6V2 model (one-time HF download)"]
    async fn test_fastembed_service_embed() {
        let service = FastEmbedService::new();
        let embedding = service.embed("hello world").await.unwrap();
        assert_eq!(embedding.len(), 384);
        for val in &embedding { assert!(val.is_finite()); }
    }

    #[tokio::test]
    #[ignore = "needs the AllMiniLML6V2 model (one-time HF download)"]
    async fn test_fastembed_service_embed_batch() {
        let service = FastEmbedService::new();
        let texts = vec!["hello".to_string(), "world".to_string()];
        let embeddings = service.embed_batch(&texts).await.unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), 384);
    }

    #[tokio::test]
    #[ignore = "needs the AllMiniLML6V2 model (one-time HF download)"]
    async fn test_fastembed_service_deterministic() {
        let service = FastEmbedService::new();
        let emb1 = service.embed("test text").await.unwrap();
        let emb2 = service.embed("test text").await.unwrap();
        assert_eq!(emb1, emb2);
    }

    #[tokio::test]
    #[ignore = "needs the AllMiniLML6V2 model (one-time HF download)"]
    async fn test_fastembed_service_different_texts() {
        let service = FastEmbedService::new();
        let emb1 = service.embed("hello").await.unwrap();
        let emb2 = service.embed("world").await.unwrap();
        assert_ne!(emb1, emb2);
    }

    #[tokio::test]
    #[ignore = "needs the AllMiniLML6V2 model (one-time HF download)"]
    async fn test_fastembed_service_health_check() {
        let service = FastEmbedService::new();
        service.health_check().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs the AllMiniLML6V2 model (one-time HF download)"]
    async fn test_fastembed_service_actual_dimension() {
        let service = FastEmbedService::new();
        assert_eq!(service.actual_dimension().await.unwrap(), 384);
    }

    #[tokio::test]
    #[ignore = "needs the BGE-base model (one-time HF download)"]
    async fn test_fastembed_service_with_model() {
        let service = FastEmbedService::with_model(EmbeddingModel::bge_base_en_v15());
        let embedding = service.embed("test").await.unwrap();
        assert_eq!(embedding.len(), 768);
    }

    // ─── EmbeddingServiceRegistry tests (stub-backed, no download) ────────

    #[tokio::test]
    async fn test_registry_embed_with_cache() {
        let primary = Arc::new(StubEmbed::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        let emb1 = registry.embed("hello").await.unwrap();
        let emb2 = registry.embed("hello").await.unwrap();
        assert_eq!(emb1, emb2);
        assert_eq!(registry.cache_size().await, 1);
    }

    #[tokio::test]
    async fn test_registry_embed_batch() {
        let primary = Arc::new(StubEmbed::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        let texts = vec!["hello".to_string(), "world".to_string()];
        let embeddings = registry.embed_batch(&texts).await.unwrap();
        assert_eq!(embeddings.len(), 2);
    }

    #[tokio::test]
    async fn test_registry_clear_cache() {
        let primary = Arc::new(StubEmbed::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        registry.embed("test").await.unwrap();
        assert_eq!(registry.cache_size().await, 1);
        registry.clear_cache().await;
        assert_eq!(registry.cache_size().await, 0);
    }

    #[tokio::test]
    async fn test_registry_without_cache() {
        let primary = Arc::new(StubEmbed::new());
        let registry = EmbeddingServiceRegistry::without_cache(primary);
        registry.embed("test").await.unwrap();
        assert_eq!(registry.cache_size().await, 0);
    }

    #[tokio::test]
    async fn test_registry_with_fallback() {
        // primary returns 384-dim; fallback (768-dim) is wired but not used
        // because the primary succeeds.
        let primary = Arc::new(StubEmbed::with_dim_model(384, EmbeddingModel::allminilm_l6_v2()));
        let fallback = Arc::new(StubEmbed::with_dim_model(768, EmbeddingModel::bge_base_en_v15()));
        let registry = EmbeddingServiceRegistry::new(primary).with_fallback(fallback);
        let emb = registry.embed("test").await.unwrap();
        assert_eq!(emb.len(), 384);
    }

    #[tokio::test]
    async fn test_registry_health_status() {
        let primary = Arc::new(StubEmbed::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        let status = registry.health_status().await.unwrap();
        assert!(status.primary_healthy);
        assert!(status.is_functional());
        assert!(status.cache_enabled);
        assert_eq!(status.primary_service, "all-MiniLM-L6-v2");
    }

    #[tokio::test]
    async fn test_registry_health_status_with_fallback() {
        let primary = Arc::new(StubEmbed::with_dim_model(384, EmbeddingModel::allminilm_l6_v2()));
        let fallback = Arc::new(StubEmbed::with_dim_model(768, EmbeddingModel::bge_base_en_v15()));
        let registry = EmbeddingServiceRegistry::new(primary).with_fallback(fallback);
        let status = registry.health_status().await.unwrap();
        assert!(status.primary_healthy);
        assert!(status.fallback_healthy.unwrap());
        assert!(status.is_functional());
    }

    #[tokio::test]
    async fn test_registry_dimension() {
        let primary = Arc::new(StubEmbed::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        assert_eq!(registry.dimension(), 384);
    }

    #[tokio::test]
    async fn test_registry_runtime_fallback_on_primary_error() {
        // Primary always errors → registry must transparently switch to fallback.
        struct Failing;
        #[async_trait]
        impl EmbeddingService for Failing {
            async fn embed(&self, _: &str) -> Result<Vec<f32>> {
                Err(oneai_core::error::OneAIError::Embedding("primary down".into()))
            }
            async fn embed_batch(&self, _: &[String]) -> Result<Vec<Vec<f32>>> {
                Err(oneai_core::error::OneAIError::Embedding("primary down".into()))
            }
            fn model(&self) -> EmbeddingModel { EmbeddingModel::allminilm_l6_v2() }
        }
        let primary: Arc<dyn EmbeddingService> = Arc::new(Failing);
        let fallback: Arc<dyn EmbeddingService> = Arc::new(StubEmbed::new());
        let registry = EmbeddingServiceRegistry::new(primary).with_fallback(fallback);
        let emb = registry.embed("test").await.unwrap();
        assert_eq!(emb.len(), 384);
    }

    // ─── OpenAI/Voyage/Ollama service construction tests ─────────────────

    #[test]
    fn test_openai_service_creation() {
        let service = OpenAIEmbeddingService::new("sk-test".to_string(), EmbeddingModel::openai_small());
        assert_eq!(service.model(), EmbeddingModel::openai_small());
        assert_eq!(service.dimension(), 1536);
    }

    #[test]
    fn test_openai_service_with_base_url() {
        let service = OpenAIEmbeddingService::with_base_url(
            "sk-test".to_string(), EmbeddingModel::openai_small(),
            "https://api.deepseek.com/v1".to_string(),
        );
        assert_eq!(service.model(), EmbeddingModel::openai_small());
    }

    #[test]
    fn test_voyage_service_creation() {
        let service = VoyageEmbeddingService::new("pa-test".to_string(), EmbeddingModel::voyage3());
        assert_eq!(service.model(), EmbeddingModel::voyage3());
        assert_eq!(service.dimension(), 1024);
    }

    #[test]
    fn test_ollama_service_creation() {
        let service = OllamaEmbeddingService::new();
        assert_eq!(service.model(), EmbeddingModel::nomic_embed_text());
    }

    #[test]
    fn test_ollama_service_with_model() {
        let service = OllamaEmbeddingService::with_model("mxbai-embed-large".to_string());
        assert_eq!(service.model().as_str(), "mxbai-embed-large");
    }

    // ─── EmbeddingHealthStatus tests ─────────────────────────────────────

    #[test]
    fn test_health_status_is_functional() {
        let status = EmbeddingHealthStatus::new(
            "all-MiniLM-L6-v2".to_string(),
            true,
            None,
            None,
            true,
            0,
        );
        assert!(status.is_functional());
    }

    #[test]
    fn test_health_status_not_functional() {
        let status = EmbeddingHealthStatus::new(
            "text-embedding-3-small".to_string(),
            false,
            None,
            None,
            false,
            0,
        );
        assert!(!status.is_functional());
    }

    #[test]
    fn test_health_status_fallback_functional() {
        let status = EmbeddingHealthStatus::new(
            "text-embedding-3-small".to_string(),
            false,
            Some("all-MiniLM-L6-v2".to_string()),
            Some(true),
            true,
            5,
        );
        assert!(status.is_functional());
    }

    // ─── FastEmbed real-inference tests (require a one-time model download) ──

    #[tokio::test]
    async fn test_fastembed_embeds_real_vector() {
        // Triggers the one-time AllMiniLML6V2 download (~22MB) on first run;
        // cached afterwards so it works offline. Skipped if HF_HUB is offline.
        let svc = FastEmbedService::new();
        let emb = match svc.embed("hello world").await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipped (model download unavailable): {e}");
                return;
            }
        };
        assert_eq!(emb.len(), 384);
        assert!(emb.iter().all(|v| v.is_finite()));
    }
}
