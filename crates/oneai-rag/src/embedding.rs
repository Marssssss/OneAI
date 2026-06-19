//! Embedding service — vector embedding generation for RAG document indexing and memory search.
//!
//! Core trait and types (`EmbeddingService`, `EmbeddingModel`, `EmbeddingConfig`, etc.)
//! are defined in `oneai_core::traits` and re-exported here.
//!
//! Concrete implementations live in this module:
//! - **OpenAIEmbeddingService**: Via OpenAI's text-embedding API (cloud, high quality)
//! - **AnthropicEmbeddingService**: Via Anthropic/Voyage embedding API (cloud, excellent quality)
//! - **OllamaEmbeddingService**: Via Ollama's embedding API (local, no API key needed)
//! - **FastEmbedService**: Placeholder for local ONNX model via fastembed crate
//!
//! The EmbeddingServiceRegistry manages service lifecycle, caching, and fallback.
//! AutoEmbeddingDocumentIndex provides zero-config RAG with automatic embedding computation.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};

// Re-export core trait and types from oneai-core
pub use oneai_core::{
    EmbeddingModel,
    EmbeddingService,
    EmbeddingServiceType,
    EmbeddingConfig,
    EmbeddingHealthStatus,
};

// ─── EmbeddingConfig::build_service() extension ──────────────────────────────

/// Extension for EmbeddingConfig to build concrete EmbeddingService implementations.
///
/// This trait is implemented in `oneai-rag` because the concrete service types
/// (OpenAIEmbeddingService, AnthropicEmbeddingService, etc.) live here.
/// The core EmbeddingConfig struct is in oneai-core, but `build_service()` needs
/// access to the concrete implementations.
pub trait EmbeddingConfigExt {
    /// Build an EmbeddingService from the config.
    fn build_service(&self) -> Result<Arc<dyn EmbeddingService>>;
}

impl EmbeddingConfigExt for EmbeddingConfig {
    fn build_service(&self) -> Result<Arc<dyn EmbeddingService>> {
        match self.service_type {
            EmbeddingServiceType::FastEmbed => {
                Ok(Arc::new(FastEmbedService::with_model(self.model)))
            }
            EmbeddingServiceType::OpenAI => {
                let api_key = self.api_key.as_ref()
                    .ok_or_else(|| OneAIError::Embedding("OpenAI embedding requires API key".to_string()))?;
                let base_url = self.base_url.as_deref()
                    .unwrap_or("https://api.openai.com/v1");
                Ok(Arc::new(OpenAIEmbeddingService::with_base_url(
                    api_key.clone(), self.model, base_url.to_string(),
                )))
            }
            EmbeddingServiceType::Anthropic => {
                let api_key = self.api_key.as_ref()
                    .ok_or_else(|| OneAIError::Embedding("Anthropic embedding requires API key".to_string()))?;
                let base_url = self.base_url.as_deref()
                    .unwrap_or("https://api.anthropic.com/v1");
                Ok(Arc::new(AnthropicEmbeddingService::with_base_url(
                    api_key.clone(), self.model, base_url.to_string(),
                )))
            }
            EmbeddingServiceType::Ollama => {
                let base_url = self.base_url.as_deref()
                    .unwrap_or("http://localhost:11434");
                let model_name = self.ollama_model.as_deref()
                    .unwrap_or("nomic-embed-text");
                Ok(Arc::new(OllamaEmbeddingService::with_url_and_model(
                    base_url.to_string(), model_name.to_string(),
                )))
            }
            _ => {
                Err(OneAIError::Embedding(format!("Unsupported embedding service type: {:?}", self.service_type)))
            }
        }
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
        assert!(model.service_type() == EmbeddingServiceType::OpenAI,
            "OpenAIEmbeddingService only supports OpenAI embedding models");
        Self {
            model,
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Create with a custom base URL (for OpenAI-compatible APIs like DeepSeek, 智谱).
    pub fn with_base_url(api_key: String, model: EmbeddingModel, base_url: String) -> Self {
        assert!(model.service_type() == EmbeddingServiceType::OpenAI,
            "OpenAIEmbeddingService only supports OpenAI embedding models");
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

        let request_body = serde_json::json!({
            "model": self.model.model_name(),
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

    fn model(&self) -> EmbeddingModel {
        self.model
    }
}

// ─── AnthropicEmbeddingService ───────────────────────────────────────────────

/// Anthropic embedding service — calls Anthropic/Voyage embedding API.
///
/// Supports voyage-3 (1024-dim) and voyage-3-lite (512-dim).
/// Requires an Anthropic API key.
///
/// **API endpoint**: `POST https://api.anthropic.com/v1/embeddings`
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
pub struct AnthropicEmbeddingService {
    /// The embedding model to use.
    model: EmbeddingModel,
    /// Anthropic API key.
    api_key: String,
    /// Base URL (default: https://api.anthropic.com/v1).
    base_url: String,
    /// HTTP client.
    client: reqwest::Client,
}

impl AnthropicEmbeddingService {
    /// Create with an Anthropic API key and Voyage model.
    pub fn new(api_key: String, model: EmbeddingModel) -> Self {
        assert!(model.service_type() == EmbeddingServiceType::Anthropic,
            "AnthropicEmbeddingService only supports Voyage embedding models");
        Self {
            model,
            api_key,
            base_url: "https://api.anthropic.com/v1".to_string(),
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
impl EmbeddingService for AnthropicEmbeddingService {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let texts = [text.to_string()];
        let embeddings = self.embed_batch(&texts).await?;
        embeddings.into_iter().next()
            .ok_or_else(|| OneAIError::Embedding("Anthropic embedding returned no results".to_string()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let request_body = serde_json::json!({
            "model": self.model.model_name(),
            "input": texts,
        });

        let response = self.client
            .post(self.embeddings_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| OneAIError::Embedding(format!("Anthropic embedding HTTP error: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OneAIError::Embedding(format!(
                "Anthropic embedding API error: status {} — {}", status, body
            )));
        }

        let response_json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| OneAIError::Embedding(format!("Anthropic embedding response parse error: {}", e)))?;

        let data_array = response_json.get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| OneAIError::Embedding("Anthropic embedding response missing 'data' field".to_string()))?;

        let mut sorted_data: Vec<(usize, Vec<f32>)> = Vec::new();
        for entry in data_array {
            let index = entry.get("index")
                .and_then(|i| i.as_u64())
                .ok_or_else(|| OneAIError::Embedding("Anthropic embedding entry missing 'index'".to_string()))? as usize;

            let embedding_array = entry.get("embedding")
                .and_then(|e| e.as_array())
                .ok_or_else(|| OneAIError::Embedding("Anthropic embedding entry missing 'embedding'".to_string()))?;

            let embedding: Vec<f32> = embedding_array.iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect();

            sorted_data.push((index, embedding));
        }

        sorted_data.sort_by_key(|(idx, _)| *idx);

        if sorted_data.len() != texts.len() {
            return Err(OneAIError::Embedding(format!(
                "Anthropic embedding returned {} results for {} inputs",
                sorted_data.len(), texts.len()
            )));
        }

        Ok(sorted_data.into_iter().map(|(_, emb)| emb).collect())
    }

    fn model(&self) -> EmbeddingModel {
        self.model
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
        EmbeddingModel::Ollama
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
pub struct FastEmbedService {
    /// The embedding model to use.
    model: EmbeddingModel,
}

impl FastEmbedService {
    /// Create a FastEmbedService with the default model (AllMiniLML6V2).
    pub fn new() -> Self {
        Self { model: EmbeddingModel::AllMiniLML6V2 }
    }

    /// Create with a specific model.
    pub fn with_model(model: EmbeddingModel) -> Self {
        assert!(model.is_local() && model != EmbeddingModel::Ollama,
            "FastEmbedService only supports local ONNX models (not Ollama)");
        Self { model }
    }
}

impl Default for FastEmbedService {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl EmbeddingService for FastEmbedService {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Stub implementation — generates a deterministic test embedding.
        let dim = self.model.dimension();
        let hash = simple_text_hash(text);
        let embedding: Vec<f32> = (0..dim)
            .map(|i| {
                let seed = hash.wrapping_add(i as u64);
                let normalized = (seed % 1000) as f32 / 1000.0 - 0.5;
                normalized * 0.1
            })
            .collect();
        Ok(embedding)
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            let embedding = self.embed(text).await?;
            results.push(embedding);
        }
        Ok(results)
    }

    fn model(&self) -> EmbeddingModel {
        self.model
    }
}

/// Simple deterministic hash for generating test embeddings.
fn simple_text_hash(text: &str) -> u64 {
    let mut hash: u64 = 5381;
    for byte in text.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
    }
    hash
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
/// let registry = registry.with_fallback(Arc::new(OpenAIEmbeddingService::new(api_key, EmbeddingModel::OpenAISmall)));
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

    /// Generate an embedding for a single text string.
    ///
    /// Uses the primary service. If it fails and a fallback is configured,
    /// tries the fallback service. Results are cached if caching is enabled.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
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

    /// Generate embeddings for multiple text strings in a batch.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
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

    /// Get the primary embedding model.
    pub fn model(&self) -> EmbeddingModel {
        self.primary.model()
    }

    /// Get the embedding dimension from the primary service.
    pub fn dimension(&self) -> usize {
        self.primary.model().dimension()
    }

    /// Get the actual embedding dimension (querying runtime if needed).
    pub async fn actual_dimension(&self) -> Result<usize> {
        self.primary.actual_dimension().await
    }

    /// Health check — verify the primary (and fallback) services are functional.
    pub async fn health_check(&self) -> Result<EmbeddingHealthStatus> {
        let primary_healthy = self.primary.health_check().await.is_ok();
        let fallback_healthy = if let Some(fallback) = &self.fallback {
            Some(fallback.health_check().await.is_ok())
        } else {
            None
        };

        Ok(EmbeddingHealthStatus::new(
            self.primary.model().model_name().to_string(),
            primary_healthy,
            self.fallback.as_ref()
                .map(|f| f.model().model_name().to_string()),
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
        assert_eq!(EmbeddingModel::AllMiniLML6V2.dimension(), 384);
        assert_eq!(EmbeddingModel::OpenAISmall.dimension(), 1536);
        assert_eq!(EmbeddingModel::OpenAILarge.dimension(), 3072);
        assert_eq!(EmbeddingModel::Voyage3.dimension(), 1024);
        assert_eq!(EmbeddingModel::Voyage3Lite.dimension(), 512);
        assert_eq!(EmbeddingModel::Ollama.dimension(), 0);
    }

    #[test]
    fn test_embedding_model_requires_api_key() {
        assert!(EmbeddingModel::OpenAISmall.requires_api_key());
        assert!(EmbeddingModel::OpenAILarge.requires_api_key());
        assert!(EmbeddingModel::Voyage3.requires_api_key());
        assert!(EmbeddingModel::Voyage3Lite.requires_api_key());
        assert!(!EmbeddingModel::AllMiniLML6V2.requires_api_key());
        assert!(!EmbeddingModel::Ollama.requires_api_key());
    }

    #[test]
    fn test_embedding_model_is_local() {
        assert!(EmbeddingModel::AllMiniLML6V2.is_local());
        assert!(EmbeddingModel::BGEBaseENv15.is_local());
        assert!(EmbeddingModel::Ollama.is_local());
        assert!(!EmbeddingModel::OpenAISmall.is_local());
        assert!(!EmbeddingModel::Voyage3.is_local());
    }

    #[test]
    fn test_embedding_model_name() {
        assert_eq!(EmbeddingModel::OpenAISmall.model_name(), "text-embedding-3-small");
        assert_eq!(EmbeddingModel::Voyage3.model_name(), "voyage-3");
        assert_eq!(EmbeddingModel::Ollama.model_name(), "nomic-embed-text");
    }

    #[test]
    fn test_embedding_model_service_type() {
        assert_eq!(EmbeddingModel::OpenAISmall.service_type(), EmbeddingServiceType::OpenAI);
        assert_eq!(EmbeddingModel::Voyage3.service_type(), EmbeddingServiceType::Anthropic);
        assert_eq!(EmbeddingModel::Ollama.service_type(), EmbeddingServiceType::Ollama);
        assert_eq!(EmbeddingModel::AllMiniLML6V2.service_type(), EmbeddingServiceType::FastEmbed);
    }

    #[test]
    fn test_embedding_config_default() {
        let config = EmbeddingConfig::default();
        assert_eq!(config.service_type, EmbeddingServiceType::FastEmbed);
        assert_eq!(config.model, EmbeddingModel::AllMiniLML6V2);
        assert!(config.api_key.is_none());
    }

    #[test]
    fn test_embedding_config_openai() {
        let config = EmbeddingConfig::openai("sk-test".to_string(), EmbeddingModel::OpenAISmall);
        assert_eq!(config.service_type, EmbeddingServiceType::OpenAI);
        assert_eq!(config.api_key, Some("sk-test".to_string()));
    }

    #[test]
    fn test_embedding_config_anthropic() {
        let config = EmbeddingConfig::anthropic("ant-test".to_string(), EmbeddingModel::Voyage3);
        assert_eq!(config.service_type, EmbeddingServiceType::Anthropic);
    }

    #[test]
    fn test_embedding_config_ollama() {
        let config = EmbeddingConfig::ollama(Some("mxbai-embed-large".to_string()));
        assert_eq!(config.service_type, EmbeddingServiceType::Ollama);
    }

    #[test]
    fn test_embedding_config_build_service_fastembed() {
        let config = EmbeddingConfig::default();
        let service = config.build_service().unwrap();
        assert_eq!(service.model(), EmbeddingModel::AllMiniLML6V2);
        assert_eq!(service.dimension(), 384);
    }

    #[test]
    fn test_embedding_config_build_service_openai_no_key() {
        let config = EmbeddingConfig::from_parts(
            EmbeddingServiceType::OpenAI,
            EmbeddingModel::OpenAISmall,
            None,
        );
        assert!(config.build_service().is_err());
    }

    #[test]
    fn test_embedding_config_build_service_anthropic_no_key() {
        let config = EmbeddingConfig::from_parts(
            EmbeddingServiceType::Anthropic,
            EmbeddingModel::Voyage3,
            None,
        );
        assert!(config.build_service().is_err());
    }

    #[test]
    fn test_embedding_config_build_service_openai_with_key() {
        let config = EmbeddingConfig::openai("sk-test".to_string(), EmbeddingModel::OpenAISmall);
        let service = config.build_service().unwrap();
        assert_eq!(service.model(), EmbeddingModel::OpenAISmall);
    }

    #[test]
    fn test_embedding_config_build_service_anthropic_with_key() {
        let config = EmbeddingConfig::anthropic("ant-test".to_string(), EmbeddingModel::Voyage3);
        let service = config.build_service().unwrap();
        assert_eq!(service.model(), EmbeddingModel::Voyage3);
    }

    #[test]
    fn test_embedding_config_build_service_ollama() {
        let config = EmbeddingConfig::ollama(None);
        let service = config.build_service().unwrap();
        assert_eq!(service.model(), EmbeddingModel::Ollama);
    }

    // ─── FastEmbedService tests ──────────────────────────────────────────

    #[tokio::test]
    async fn test_fastembed_service_embed() {
        let service = FastEmbedService::new();
        let embedding = service.embed("hello world").await.unwrap();
        assert_eq!(embedding.len(), 384);
        for val in &embedding { assert!(val.is_finite()); }
    }

    #[tokio::test]
    async fn test_fastembed_service_embed_batch() {
        let service = FastEmbedService::new();
        let texts = vec!["hello".to_string(), "world".to_string()];
        let embeddings = service.embed_batch(&texts).await.unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), 384);
    }

    #[tokio::test]
    async fn test_fastembed_service_deterministic() {
        let service = FastEmbedService::new();
        let emb1 = service.embed("test text").await.unwrap();
        let emb2 = service.embed("test text").await.unwrap();
        assert_eq!(emb1, emb2);
    }

    #[tokio::test]
    async fn test_fastembed_service_different_texts() {
        let service = FastEmbedService::new();
        let emb1 = service.embed("hello").await.unwrap();
        let emb2 = service.embed("world").await.unwrap();
        assert_ne!(emb1, emb2);
    }

    #[tokio::test]
    async fn test_fastembed_service_health_check() {
        let service = FastEmbedService::new();
        service.health_check().await.unwrap();
    }

    #[tokio::test]
    async fn test_fastembed_service_actual_dimension() {
        let service = FastEmbedService::new();
        assert_eq!(service.actual_dimension().await.unwrap(), 384);
    }

    #[tokio::test]
    async fn test_fastembed_service_with_model() {
        let service = FastEmbedService::with_model(EmbeddingModel::BGEBaseENv15);
        let embedding = service.embed("test").await.unwrap();
        assert_eq!(embedding.len(), 768);
    }

    // ─── EmbeddingServiceRegistry tests ──────────────────────────────────

    #[tokio::test]
    async fn test_registry_embed_with_cache() {
        let primary = Arc::new(FastEmbedService::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        let emb1 = registry.embed("hello").await.unwrap();
        let emb2 = registry.embed("hello").await.unwrap();
        assert_eq!(emb1, emb2);
        assert_eq!(registry.cache_size().await, 1);
    }

    #[tokio::test]
    async fn test_registry_embed_batch() {
        let primary = Arc::new(FastEmbedService::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        let texts = vec!["hello".to_string(), "world".to_string()];
        let embeddings = registry.embed_batch(&texts).await.unwrap();
        assert_eq!(embeddings.len(), 2);
    }

    #[tokio::test]
    async fn test_registry_clear_cache() {
        let primary = Arc::new(FastEmbedService::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        registry.embed("test").await.unwrap();
        assert_eq!(registry.cache_size().await, 1);
        registry.clear_cache().await;
        assert_eq!(registry.cache_size().await, 0);
    }

    #[tokio::test]
    async fn test_registry_without_cache() {
        let primary = Arc::new(FastEmbedService::new());
        let registry = EmbeddingServiceRegistry::without_cache(primary);
        registry.embed("test").await.unwrap();
        assert_eq!(registry.cache_size().await, 0);
    }

    #[tokio::test]
    async fn test_registry_with_fallback() {
        let primary = Arc::new(FastEmbedService::new());
        let fallback = Arc::new(FastEmbedService::with_model(EmbeddingModel::BGEBaseENv15));
        let registry = EmbeddingServiceRegistry::new(primary).with_fallback(fallback);
        let emb = registry.embed("test").await.unwrap();
        assert_eq!(emb.len(), 384);
    }

    #[tokio::test]
    async fn test_registry_health_check() {
        let primary = Arc::new(FastEmbedService::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        let status = registry.health_check().await.unwrap();
        assert!(status.primary_healthy);
        assert!(status.is_functional());
        assert!(status.cache_enabled);
        assert_eq!(status.primary_service, "all-MiniLM-L6-v2");
    }

    #[tokio::test]
    async fn test_registry_health_check_with_fallback() {
        let primary = Arc::new(FastEmbedService::new());
        let fallback = Arc::new(FastEmbedService::with_model(EmbeddingModel::BGEBaseENv15));
        let registry = EmbeddingServiceRegistry::new(primary).with_fallback(fallback);
        let status = registry.health_check().await.unwrap();
        assert!(status.primary_healthy);
        assert!(status.fallback_healthy.unwrap());
        assert!(status.is_functional());
    }

    #[tokio::test]
    async fn test_registry_dimension() {
        let primary = Arc::new(FastEmbedService::new());
        let registry = EmbeddingServiceRegistry::new(primary);
        assert_eq!(registry.dimension(), 384);
    }

    // ─── OpenAI/Anthropic/Ollama service construction tests ──────────────

    #[test]
    fn test_openai_service_creation() {
        let service = OpenAIEmbeddingService::new("sk-test".to_string(), EmbeddingModel::OpenAISmall);
        assert_eq!(service.model(), EmbeddingModel::OpenAISmall);
        assert_eq!(service.dimension(), 1536);
    }

    #[test]
    fn test_openai_service_with_base_url() {
        let service = OpenAIEmbeddingService::with_base_url(
            "sk-test".to_string(), EmbeddingModel::OpenAISmall,
            "https://api.deepseek.com/v1".to_string(),
        );
        assert_eq!(service.model(), EmbeddingModel::OpenAISmall);
    }

    #[test]
    fn test_anthropic_service_creation() {
        let service = AnthropicEmbeddingService::new("ant-test".to_string(), EmbeddingModel::Voyage3);
        assert_eq!(service.model(), EmbeddingModel::Voyage3);
        assert_eq!(service.dimension(), 1024);
    }

    #[test]
    fn test_ollama_service_creation() {
        let service = OllamaEmbeddingService::new();
        assert_eq!(service.model(), EmbeddingModel::Ollama);
    }

    #[test]
    fn test_ollama_service_with_model() {
        let service = OllamaEmbeddingService::with_model("mxbai-embed-large".to_string());
        assert_eq!(service.model(), EmbeddingModel::Ollama);
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

    // ─── Hash tests ──────────────────────────────────────────────────────

    #[test]
    fn test_simple_text_hash_deterministic() {
        assert_eq!(simple_text_hash("hello"), simple_text_hash("hello"));
    }

    #[test]
    fn test_simple_text_hash_different() {
        assert_ne!(simple_text_hash("hello"), simple_text_hash("world"));
    }

    #[test]
    fn test_simple_text_hash_empty() {
        assert_ne!(simple_text_hash(""), 0);
    }
}
