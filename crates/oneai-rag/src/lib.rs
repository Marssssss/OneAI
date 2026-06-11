//! # OneAI RAG
//!
//! Retrieval-Augmented Generation support.
//!
//! Provides document management, indexing, and retrieval for injecting
//! relevant context into LLM inference requests.
//! New: Embedding service (FastEmbed, Ollama, OpenAI) for automatic embedding generation.

pub mod document;
pub mod index;
pub mod retrieval;
pub mod embedding;

pub use document::*;
pub use index::*;
pub use retrieval::*;
pub use embedding::*;