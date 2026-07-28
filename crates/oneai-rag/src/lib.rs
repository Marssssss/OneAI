//! # OneAI RAG
//!
//! Retrieval-Augmented Generation support.
//!
//! Provides document management, indexing, and retrieval for injecting
//! relevant context into LLM inference requests.
//! New: Embedding service (FastEmbed, Ollama, OpenAI) for automatic embedding generation.

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.

pub mod chunk_split;
pub mod document;
pub mod embedding;
pub mod index;
pub mod provider_adapter;
pub mod retrieval;

pub use chunk_split::*;
pub use document::*;
pub use embedding::*;
pub use index::*;
#[cfg(feature = "ort")]
pub use provider_adapter::BgeM3Adapter;
pub use provider_adapter::{
    Availability, EmbeddingProviderAdapter, EmbeddingProviderRegistry, EmbeddingResolver, EnvProbe,
    FastEmbedAdapter, OllamaAdapter, OpenAiAdapter, OpenAiCompatAdapter, VoyageAdapter,
};
pub use retrieval::*;
