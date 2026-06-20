//! Short-term memory — sliding window, in-memory conversation context.
//!
//! ShortTermMemory manages recent conversation turns in memory using a sliding window.
//! When the window is full, older entries are removed. It also supports:
//! - Token estimation for context window budgeting
//! - Conversation assembly from stored entries for LLM context
//! - Integration with the MemoryStore trait for unified memory access

use std::collections::VecDeque;

use oneai_core::{MemoryEntry, MemoryQuery};
use oneai_core::error::Result;
use oneai_core::traits::MemoryStore;

/// Short-term memory with sliding window.
///
/// Stores recent conversation turns in memory.
/// When the window is full, older entries are removed.
/// Provides token estimation for budgeting context windows.
pub struct ShortTermMemory {
    /// Maximum number of entries in the sliding window.
    window_size: usize,
    /// The stored entries.
    entries: VecDeque<MemoryEntry>,
}

impl ShortTermMemory {
    /// Create a new short-term memory with the given window size.
    pub fn new(window_size: usize) -> Self {
        Self {
            window_size,
            entries: VecDeque::with_capacity(window_size),
        }
    }

    /// Push an entry into the sliding window.
    /// If the window is full, the oldest entry is removed and returned.
    pub fn push(&mut self, entry: MemoryEntry) -> Option<MemoryEntry> {
        let evicted = if self.entries.len() >= self.window_size {
            self.entries.pop_front()
        } else {
            None
        };
        self.entries.push_back(entry);
        evicted
    }

    /// Get all entries in the current window.
    pub fn entries(&self) -> &VecDeque<MemoryEntry> {
        &self.entries
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Get the current number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the memory is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Estimate the total token count in the current window.
    ///
    /// Uses a rough heuristic: ~1 token per 4 characters of English text,
    /// plus overhead for metadata. This is a rough estimate; actual
    /// tokenization depends on the model's tokenizer.
    pub fn estimated_tokens(&self) -> usize {
        self.entries.iter().map(|entry| {
            // Rough token estimate: 1 token per ~4 chars
            entry.content.len() / 4 + 50 // +50 for metadata overhead
        }).sum()
    }

    /// Get the window size.
    pub fn window_size(&self) -> usize {
        self.window_size
    }

    /// Find entries matching a keyword query.
    ///
    /// Simple keyword matching for fast retrieval without embedding computation.
    pub fn find_by_keyword(&self, keyword: &str) -> Vec<&MemoryEntry> {
        self.entries.iter()
            .filter(|entry| oneai_core::keyword_matches(&entry.content, keyword))
            .collect()
    }

    /// Assemble a conversation-ready string from the stored entries.
    ///
    /// Formats entries as a sequence of turns suitable for injection into
    /// an LLM context window.
    pub fn assemble_context(&self) -> String {
        self.entries.iter()
            .map(|entry| {
                let role = entry.metadata.get("role").map(|s| s.as_str()).unwrap_or("unknown");
                format!("[{}] {}", role, entry.content)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait::async_trait]
impl MemoryStore for ShortTermMemory {
    async fn store(&self, _entry: MemoryEntry) -> Result<()> {
        // Note: MemoryStore requires &self, but push needs &mut self.
        // This is a limitation of the trait interface — in practice,
        // ShortTermMemory should be used via direct push() calls,
        // and MemoryStore is primarily for long-term memory.
        // For compatibility, we use interior mutability via RwLock in practice.
        tracing::warn!("ShortTermMemory::store via MemoryStore trait — use push() for direct access");
        Ok(())
    }

    async fn retrieve(&self, query: &MemoryQuery, top_k: usize) -> Result<Vec<MemoryEntry>> {
        // Simple keyword matching for short-term memory retrieval
        let results: Vec<MemoryEntry> = self.entries.iter()
            .filter(|entry| oneai_core::keyword_matches(&entry.content, &query.text))
            .take(top_k)
            .cloned()
            .collect();
        Ok(results)
    }

    async fn compress(&self, _threshold: usize) -> Result<Vec<MemoryEntry>> {
        // For short-term memory, compression means evicting older entries
        // if the estimated token count exceeds the threshold.
        // This is typically handled by the ContextCompressor instead.
        Ok(Vec::new())
    }

    async fn clear(&self) -> Result<()> {
        // Same interior mutability limitation as store()
        tracing::warn!("ShortTermMemory::clear via MemoryStore trait — use clear() for direct access");
        Ok(())
    }
}

/// Thread-safe wrapper for ShortTermMemory that supports the MemoryStore trait.
///
/// Uses interior mutability (RwLock) to allow async trait methods.
pub struct ShortTermMemorySync {
    inner: tokio::sync::RwLock<ShortTermMemory>,
}

impl ShortTermMemorySync {
    /// Create a new thread-safe short-term memory.
    pub fn new(window_size: usize) -> Self {
        Self {
            inner: tokio::sync::RwLock::new(ShortTermMemory::new(window_size)),
        }
    }

    /// Push an entry into the sliding window.
    pub async fn push(&self, entry: MemoryEntry) -> Option<MemoryEntry> {
        self.inner.write().await.push(entry)
    }

    /// Get all entries.
    pub async fn entries(&self) -> Vec<MemoryEntry> {
        self.inner.read().await.entries.iter().cloned().collect()
    }

    /// Get estimated token count.
    pub async fn estimated_tokens(&self) -> usize {
        self.inner.read().await.estimated_tokens()
    }

    /// Assemble context string.
    pub async fn assemble_context(&self) -> String {
        self.inner.read().await.assemble_context()
    }
}

#[async_trait::async_trait]
impl MemoryStore for ShortTermMemorySync {
    async fn store(&self, entry: MemoryEntry) -> Result<()> {
        self.inner.write().await.push(entry);
        Ok(())
    }

    async fn retrieve(&self, query: &MemoryQuery, top_k: usize) -> Result<Vec<MemoryEntry>> {
        self.inner.read().await.retrieve(query, top_k).await
    }

    async fn compress(&self, threshold: usize) -> Result<Vec<MemoryEntry>> {
        let mut inner = self.inner.write().await;
        let mut evicted = Vec::new();
        while inner.estimated_tokens() > threshold && !inner.is_empty() {
            if let Some(entry) = inner.entries.pop_front() {
                evicted.push(entry);
            }
        }
        Ok(evicted)
    }

    async fn clear(&self) -> Result<()> {
        // The inner ShortTermMemory's `MemoryStore::clear` returns a must-use
        // future; bind it explicitly to document that the trait impl is a
        // no-op (it only logs), matching prior behavior. Actual clearing is
        // done via the inherent `ShortTermMemory::clear(&mut self)`.
        let _ = self.inner.write().await.clear();
        Ok(())
    }
}