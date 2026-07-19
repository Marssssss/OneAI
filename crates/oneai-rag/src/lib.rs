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


pub mod document;
pub mod index;
pub mod retrieval;
pub mod embedding;
pub mod chunk_split;
pub mod provider_adapter;

pub use document::*;
pub use index::*;
pub use retrieval::*;
pub use embedding::*;
pub use chunk_split::*;
pub use provider_adapter::{
    EmbeddingProviderAdapter, EmbeddingProviderRegistry, EmbeddingResolver, EnvProbe,
    Availability, OpenAiAdapter, VoyageAdapter, OllamaAdapter, FastEmbedAdapter,
    OpenAiCompatAdapter,
};