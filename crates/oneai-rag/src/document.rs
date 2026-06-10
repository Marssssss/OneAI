//! RAG document types — the fundamental data structures for RAG operations.
//!
//! Documents are the source material that RAG retrieves from.
//! A Document is split into Chunks for indexing and retrieval.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A source document in the RAG system.
///
/// Documents can come from various sources: files, URLs, databases, etc.
/// They are split into smaller chunks for indexing and retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Unique document identifier.
    pub id: String,

    /// The full text content of the document.
    pub content: String,

    /// Document metadata (source, author, date, etc.).
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    /// The chunks derived from this document (populated after chunking).
    #[serde(default)]
    pub chunks: Vec<Chunk>,
}

impl Document {
    /// Create a new document with auto-generated ID.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.into(),
            metadata: HashMap::new(),
            chunks: Vec::new(),
        }
    }

    /// Create a new document with a specific ID.
    pub fn with_id(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            metadata: HashMap::new(),
            chunks: Vec::new(),
        }
    }

    /// Create a document with metadata.
    pub fn with_metadata(
        content: impl Into<String>,
        metadata: HashMap<String, String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.into(),
            metadata,
            chunks: Vec::new(),
        }
    }

    /// Split this document into chunks using a chunking strategy.
    ///
    /// After chunking, `self.chunks` will be populated and each chunk
    /// will reference this document's ID.
    pub fn chunk(&mut self, strategy: &ChunkingStrategy) {
        self.chunks = strategy.chunk(&self.content, &self.id);
    }

    /// Get the number of chunks.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

/// A chunk of a document — the unit of indexing and retrieval in RAG.
///
/// Each chunk is a contiguous segment of the parent document's content,
/// suitable for embedding and vector similarity search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    /// Unique chunk identifier.
    pub id: String,

    /// The parent document ID.
    pub document_id: String,

    /// The text content of this chunk.
    pub content: String,

    /// The start character offset in the parent document.
    pub start_offset: usize,

    /// The end character offset in the parent document.
    pub end_offset: usize,

    /// Chunk metadata (inherits from parent document + chunk-specific).
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl Chunk {
    /// Create a new chunk.
    pub fn new(
        document_id: impl Into<String>,
        content: impl Into<String>,
        start_offset: usize,
        end_offset: usize,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            document_id: document_id.into(),
            content: content.into(),
            start_offset,
            end_offset,
            metadata: HashMap::new(),
        }
    }

    /// Get the character length of this chunk.
    pub fn len(&self) -> usize {
        self.content.len()
    }

    /// Check if this chunk is empty.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

/// Chunking strategy — how to split documents into chunks.
///
/// Different strategies are appropriate for different content types:
/// - `FixedSize`: Simple fixed-size chunking with optional overlap
/// - `SentenceBoundary`: Split at sentence boundaries
/// - `ParagraphBoundary`: Split at paragraph boundaries
/// - `Semantic`: Split based on semantic similarity (requires embeddings)
#[derive(Debug, Clone)]
pub enum ChunkingStrategy {
    /// Fixed-size chunking with optional overlap.
    FixedSize {
        /// Maximum chunk size in characters.
        chunk_size: usize,
        /// Overlap between consecutive chunks in characters.
        overlap: usize,
    },

    /// Split at sentence boundaries (period + whitespace).
    SentenceBoundary {
        /// Maximum chunk size in characters.
        max_chunk_size: usize,
    },

    /// Split at paragraph boundaries (double newline).
    ParagraphBoundary {
        /// Maximum chunk size in characters.
        max_chunk_size: usize,
    },
}

impl Default for ChunkingStrategy {
    fn default() -> Self {
        Self::FixedSize {
            chunk_size: 512,
            overlap: 64,
        }
    }
}

impl ChunkingStrategy {
    /// Chunk text content according to this strategy.
    pub fn chunk(&self, content: &str, document_id: &str) -> Vec<Chunk> {
        match self {
            ChunkingStrategy::FixedSize { chunk_size, overlap } => {
                self.chunk_fixed_size(content, document_id, *chunk_size, *overlap)
            }
            ChunkingStrategy::SentenceBoundary { max_chunk_size } => {
                self.chunk_by_sentences(content, document_id, *max_chunk_size)
            }
            ChunkingStrategy::ParagraphBoundary { max_chunk_size } => {
                self.chunk_by_paragraphs(content, document_id, *max_chunk_size)
            }
        }
    }

    /// Fixed-size chunking with overlap.
    fn chunk_fixed_size(
        &self,
        content: &str,
        document_id: &str,
        chunk_size: usize,
        overlap: usize,
    ) -> Vec<Chunk> {
        if content.is_empty() {
            return Vec::new();
        }

        let step = chunk_size - overlap;
        let mut chunks = Vec::new();
        let mut start = 0;

        while start < content.len() {
            let end = std::cmp::min(start + chunk_size, content.len());
            let chunk_content = content[start..end].to_string();

            // Skip empty chunks
            if !chunk_content.trim().is_empty() {
                chunks.push(Chunk::new(document_id, chunk_content, start, end));
            }

            start += step;
            if start >= content.len() {
                break;
            }
        }

        chunks
    }

    /// Sentence-boundary chunking.
    fn chunk_by_sentences(
        &self,
        content: &str,
        document_id: &str,
        max_chunk_size: usize,
    ) -> Vec<Chunk> {
        // Split by sentence-ending punctuation followed by whitespace
        let sentence_boundaries: Vec<usize> = content.char_indices()
            .filter(|(i, c)| {
                // Match period, question mark, exclamation followed by whitespace
                if *c == '.' || *c == '?' || *c == '!' {
                    // Check if followed by whitespace or end of text
                    let next_idx = i + 1;
                    if next_idx >= content.len() {
                        return true;
                    }
                    let next_char = content[next_idx..].chars().next();
                    next_char.map(|c| c.is_whitespace()).unwrap_or(true)
                } else {
                    false
                }
            })
            .map(|(i, _)| i + 1) // Include the punctuation
            .collect();

        self.chunk_by_boundaries(content, document_id, max_chunk_size, &sentence_boundaries)
    }

    /// Paragraph-boundary chunking.
    fn chunk_by_paragraphs(
        &self,
        content: &str,
        document_id: &str,
        max_chunk_size: usize,
    ) -> Vec<Chunk> {
        // Split by double newline (paragraph boundary)
        let paragraph_boundaries: Vec<usize> = content.match_indices("\n\n")
            .map(|(i, _)| i + 2) // Skip the double newline
            .collect();

        self.chunk_by_boundaries(content, document_id, max_chunk_size, &paragraph_boundaries)
    }

    /// General boundary-based chunking with max size constraint.
    fn chunk_by_boundaries(
        &self,
        content: &str,
        document_id: &str,
        max_chunk_size: usize,
        boundaries: &[usize],
    ) -> Vec<Chunk> {
        if content.is_empty() {
            return Vec::new();
        }

        let mut chunks = Vec::new();
        let mut start = 0;

        // Add end-of-content as final boundary
        let all_boundaries: Vec<usize> = boundaries.iter()
            .filter(|b| **b < content.len())
            .cloned()
            .chain(std::iter::once(content.len()))
            .collect();

        for boundary in all_boundaries {
            if boundary - start > max_chunk_size && boundary > start {
                // Split further with fixed-size within this boundary region
                let sub_content = &content[start..boundary];
                let sub_chunks = self.chunk_fixed_size(sub_content, document_id, max_chunk_size, 32);
                for sub_chunk in sub_chunks {
                    chunks.push(Chunk::new(
                        document_id,
                        sub_chunk.content,
                        start + sub_chunk.start_offset,
                        start + sub_chunk.end_offset,
                    ));
                }
                start = boundary;
            } else if boundary > start {
                let chunk_content = content[start..boundary].trim().to_string();
                if !chunk_content.is_empty() {
                    chunks.push(Chunk::new(document_id, chunk_content, start, boundary));
                }
                start = boundary;
            }
        }

        // Handle remaining content after last boundary
        if start < content.len() {
            let remaining = content[start..].trim().to_string();
            if !remaining.is_empty() {
                chunks.push(Chunk::new(document_id, remaining, start, content.len()));
            }
        }

        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_creation() {
        let doc = Document::new("Hello world");
        assert!(!doc.id.is_empty());
        assert_eq!(doc.content, "Hello world");
        assert!(doc.metadata.is_empty());
        assert!(doc.chunks.is_empty());
    }

    #[test]
    fn test_document_with_id() {
        let doc = Document::with_id("doc1", "Test content");
        assert_eq!(doc.id, "doc1");
        assert_eq!(doc.content, "Test content");
    }

    #[test]
    fn test_fixed_size_chunking() {
        let mut doc = Document::with_id("doc1", "Hello world this is a test document with some content for chunking");
        let strategy = ChunkingStrategy::FixedSize {
            chunk_size: 20,
            overlap: 5,
        };
        doc.chunk(&strategy);

        assert!(doc.chunk_count() > 1);
        // Each chunk should reference the parent document
        for chunk in &doc.chunks {
            assert_eq!(chunk.document_id, "doc1");
            assert!(!chunk.content.is_empty());
        }
    }

    #[test]
    fn test_fixed_size_chunking_no_overlap() {
        let mut doc = Document::with_id("doc1", "ABCDEFGHIJ"); // 10 chars
        let strategy = ChunkingStrategy::FixedSize {
            chunk_size: 5,
            overlap: 0,
        };
        doc.chunk(&strategy);

        assert_eq!(doc.chunk_count(), 2);
        assert_eq!(doc.chunks[0].content, "ABCDE");
        assert_eq!(doc.chunks[1].content, "FGHIJ");
    }

    #[test]
    fn test_sentence_boundary_chunking() {
        let content = "Hello world. This is a test. How are you doing today? I hope well! End of text.";
        let mut doc = Document::with_id("doc1", content);
        let strategy = ChunkingStrategy::SentenceBoundary {
            max_chunk_size: 100,
        };
        doc.chunk(&strategy);

        assert!(doc.chunk_count() > 0);
        // Each chunk should be a sentence or group of sentences
        for chunk in &doc.chunks {
            assert!(!chunk.content.is_empty());
        }
    }

    #[test]
    fn test_paragraph_boundary_chunking() {
        let content = "First paragraph with some content.\n\nSecond paragraph with more text.\n\nThird paragraph ends here.";
        let mut doc = Document::with_id("doc1", content);
        let strategy = ChunkingStrategy::ParagraphBoundary {
            max_chunk_size: 200,
        };
        doc.chunk(&strategy);

        assert_eq!(doc.chunk_count(), 3);
    }

    #[test]
    fn test_chunking_empty_document() {
        let mut doc = Document::with_id("doc1", "");
        doc.chunk(&ChunkingStrategy::default());
        assert_eq!(doc.chunk_count(), 0);
    }

    #[test]
    fn test_chunk_creation() {
        let chunk = Chunk::new("doc1", "Hello", 0, 5);
        assert!(!chunk.id.is_empty());
        assert_eq!(chunk.document_id, "doc1");
        assert_eq!(chunk.content, "Hello");
        assert_eq!(chunk.start_offset, 0);
        assert_eq!(chunk.end_offset, 5);
        assert_eq!(chunk.len(), 5);
    }
}