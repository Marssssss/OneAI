//! Embedding service — vector embedding generation for RAG document indexing.
//!
//! This addresses Issue #22: the RAG module doesn't actually compute embeddings.
//! Currently, `DocumentIndex.add_document()` stores chunks but requires
//! manual `add_embedding()` calls — there's no automatic embedding generation.
//!
//! The EmbeddingService trait provides a unified interface for generating
//! vector embeddings from text, with multiple implementations:
//! - **FastEmbedService**: Local ONNX model via fastembed crate (PREFERRED)
//!   No API key needed, cross-platform, good Chinese support
//! - **OllamaEmbeddingService**: Via Ollama's embedding API
//! - **OpenAIEmbeddingService**: Via OpenAI's text-embedding API
//!
//! When integrated into DocumentIndex, embeddings are automatically computed
//! during `add_document()` — no manual `add_embedding()` calls needed.

use async_trait::async_trait;
use oneai_core::error::Result;

// ─── EmbeddingModel ─────────────────────────────────────────────────────────

/// Available embedding models.
///
/// Each model has different characteristics (size, speed, quality, language support).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingModel {
    /// AllMiniLML6V2 — lightweight, fast, good Chinese support.
    /// Recommended default for most use cases.
    /// 384-dimensional embeddings, ~22MB model size.
    AllMiniLML6V2,

    /// BGEBaseENv15 — better English quality, moderate size.
    /// 768-dimensional embeddings, ~430MB model size.
    BGEBaseENv15,

    /// MxbaiEmbedLargeV1 — high quality, larger model.
    /// 1024-dimensional embeddings, ~670MB model size.
    MxbaiEmbedLargeV1,

    /// OpenAI text-embedding-3-small — cloud-based, excellent quality.
    /// 1536-dimensional embeddings, requires API key.
    OpenAISmall,

    /// OpenAI text-embedding-3-large — cloud-based, best quality.
    /// 3072-dimensional embeddings, requires API key.
    OpenAILarge,
}

impl EmbeddingModel {
    /// Get the embedding dimension for this model.
    pub fn dimension(&self) -> usize {
        match self {
            Self::AllMiniLML6V2 => 384,
            Self::BGEBaseENv15 => 768,
            Self::MxbaiEmbedLargeV1 => 1024,
            Self::OpenAISmall => 1536,
            Self::OpenAILarge => 3072,
        }
    }

    /// Whether this model requires an API key.
    pub fn requires_api_key(&self) -> bool {
        matches!(self, Self::OpenAISmall | Self::OpenAILarge)
    }

    /// Whether this model runs locally (no external API needed).
    pub fn is_local(&self) -> bool {
        matches!(self, Self::AllMiniLML6V2 | Self::BGEBaseENv15 | Self::MxbaiEmbedLargeV1)
    }
}

// ─── EmbeddingService trait ─────────────────────────────────────────────────

/// Embedding service — generates vector embeddings from text.
///
/// The primary interface for embedding generation. Implementations
/// use different backends (local ONNX, Ollama API, OpenAI API).
///
/// When integrated into DocumentIndex, the service is called automatically
/// during document insertion — each chunk's embedding is computed
/// and stored in the vector store without manual intervention.
#[async_trait]
pub trait EmbeddingService: Send + Sync {
    /// Generate an embedding for a single text string.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Generate embeddings for multiple text strings in a batch.
    ///
    /// Batch embedding is more efficient than individual calls
    /// because it amortizes the model inference overhead.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Get the embedding model being used.
    fn model(&self) -> EmbeddingModel;

    /// Get the embedding dimension.
    fn dimension(&self) -> usize {
        self.model().dimension()
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
/// Uses the `fastembed` crate which provides ONNX Runtime-based
/// embedding generation with pre-trained models.
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
        assert!(model.is_local(), "FastEmbedService only supports local models");
        Self { model }
    }
}

impl Default for FastEmbedService {
    fn default() -> Self { Self::new() }
}

// Note: Full implementation requires fastembed crate dependency.
// The crate provides TextEmbedding struct with .embed() method.
// This will be implemented in the full code phase.

// ─── EmbeddingConfig ────────────────────────────────────────────────────────

/// Configuration for the embedding service.
///
/// Used in AppBuilder to configure which embedding service to use
/// and what model to use.
#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    /// The embedding service type to use.
    pub service_type: EmbeddingServiceType,

    /// The model to use (if applicable).
    pub model: EmbeddingModel,

    /// API key (required for OpenAI service type).
    pub api_key: Option<String>,

    /// Ollama base URL (required for Ollama service type).
    pub ollama_url: Option<String>,
}

/// The type of embedding service to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingServiceType {
    /// FastEmbed (local ONNX) — recommended default.
    FastEmbed,
    /// Ollama embedding API — requires local Ollama server.
    Ollama,
    /// OpenAI embedding API — requires API key.
    OpenAI,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            service_type: EmbeddingServiceType::FastEmbed,
            model: EmbeddingModel::AllMiniLML6V2,
            api_key: None,
            ollama_url: None,
        }
    }
}