//! # OneAI RAG
//!
//! Retrieval-Augmented Generation support.
//!
//! Provides document management, indexing, and retrieval for injecting
//! relevant context into LLM inference requests.

pub mod document;
pub mod index;
pub mod retrieval;

pub use document::*;
pub use index::*;
pub use retrieval::*;