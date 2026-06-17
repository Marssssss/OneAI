//! # OneAI Memory
//!
//! Short-term memory (sliding window), long-term memory (HNSW vector store + content store + hybrid scoring),
//! context compression, memory reflection (STM↔LTM closed loop), and MemoryManager for unified access.

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


pub mod short_term;
pub mod long_term;
pub mod compression;
pub mod hybrid_scorer;
pub mod vector_store;
pub mod manager;
pub mod reflection;

pub use short_term::*;
pub use long_term::*;
pub use compression::*;
pub use hybrid_scorer::*;
pub use vector_store::*;
pub use manager::*;
pub use reflection::{MemoryReflection, MemoryReflectionConfig, EpisodicMemory};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use oneai_core::{MemoryEntry, MemoryQuery};
    use oneai_core::traits::MemoryStore;

    #[test]
    fn test_short_term_memory_push() {
        let mut stm = ShortTermMemory::new(3);
        assert!(stm.is_empty());

        stm.push(MemoryEntry {
            id: "1".to_string(),
            content: "First".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        });
        stm.push(MemoryEntry {
            id: "2".to_string(),
            content: "Second".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        });
        assert_eq!(stm.len(), 2);

        stm.push(MemoryEntry {
            id: "3".to_string(),
            content: "Third".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        });
        assert_eq!(stm.len(), 3);

        // Window is full — pushing should evict the oldest
        let evicted = stm.push(MemoryEntry {
            id: "4".to_string(),
            content: "Fourth".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        });
        assert!(evicted.is_some());
        assert_eq!(stm.len(), 3);
        assert_eq!(stm.entries().front().unwrap().content, "Second");
    }

    #[test]
    fn test_short_term_memory_keyword_search() {
        let mut stm = ShortTermMemory::new(10);
        stm.push(MemoryEntry {
            id: "1".to_string(),
            content: "Rust programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        });
        stm.push(MemoryEntry {
            id: "2".to_string(),
            content: "Python programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "assistant".to_string())]),
        });
        stm.push(MemoryEntry {
            id: "3".to_string(),
            content: "The weather is sunny today".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        });

        let results = stm.find_by_keyword("programming");
        assert_eq!(results.len(), 2);

        let results = stm.find_by_keyword("rust");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_short_term_memory_assemble_context() {
        let mut stm = ShortTermMemory::new(10);
        stm.push(MemoryEntry {
            id: "1".to_string(),
            content: "Hello".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        });
        stm.push(MemoryEntry {
            id: "2".to_string(),
            content: "Hi there".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "assistant".to_string())]),
        });

        let context = stm.assemble_context();
        assert!(context.contains("[user] Hello"));
        assert!(context.contains("[assistant] Hi there"));
    }

    #[test]
    fn test_short_term_memory_estimated_tokens() {
        let mut stm = ShortTermMemory::new(10);
        stm.push(MemoryEntry {
            id: "1".to_string(),
            content: "This is a test message with some words".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        });

        let tokens = stm.estimated_tokens();
        assert!(tokens > 0);
    }

    #[tokio::test]
    async fn test_short_term_memory_sync() {
        let stm = ShortTermMemorySync::new(5);

        stm.push(MemoryEntry {
            id: "1".to_string(),
            content: "Test".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        }).await;

        let entries = stm.entries().await;
        assert_eq!(entries.len(), 1);

        let tokens = stm.estimated_tokens().await;
        assert!(tokens > 0);

        let context = stm.assemble_context().await;
        assert!(!context.is_empty());
    }

    #[tokio::test]
    async fn test_short_term_memory_sync_compress() {
        let stm = ShortTermMemorySync::new(10);

        // Add several entries
        for i in 0..5 {
            stm.push(MemoryEntry {
                id: format!("{}", i),
                content: format!("Entry {} with some content to make it longer", i),
                timestamp: chrono::Utc::now(),
                embedding: None,
                metadata: HashMap::new(),
            }).await;
        }

        // Compress with a very low threshold — should evict some entries
        let evicted = stm.compress(100).await.unwrap();
        // Some entries should have been evicted
        assert!(evicted.len() > 0);
    }

    #[tokio::test]
    async fn test_embedded_vector_store() {
        let store = ThreadSafeEmbeddedVectorStore::new();

        // Upsert some vectors
        store.upsert_entry(
            "doc1",
            vec![0.1, 0.2, 0.3],
            HashMap::from([("content".to_string(), "Rust programming".to_string())]),
            chrono::Utc::now(),
        ).await.unwrap();

        store.upsert_entry(
            "doc2",
            vec![0.4, 0.5, 0.6],
            HashMap::from([("content".to_string(), "Python programming".to_string())]),
            chrono::Utc::now() - chrono::Duration::hours(1),
        ).await.unwrap();

        store.upsert_entry(
            "doc3",
            vec![0.1, 0.2, 0.4],
            HashMap::from([("content".to_string(), "Rust tutorial".to_string())]),
            chrono::Utc::now(),
        ).await.unwrap();

        assert_eq!(store.len().await, 3);

        // Search for similar vectors
        let results = store.search_hybrid(vec![0.1, 0.2, 0.35], 2).await.unwrap();
        assert_eq!(results.len(), 2);
        // doc1 and doc3 should be most similar to the query
        assert!(results[0].id == "doc1" || results[0].id == "doc3");
    }

    #[tokio::test]
    async fn test_vector_store_keyword_search() {
        let store = ThreadSafeEmbeddedVectorStore::new();

        store.upsert_entry(
            "doc1",
            vec![0.1, 0.2, 0.3],
            HashMap::from([("content".to_string(), "Rust programming language".to_string())]),
            chrono::Utc::now(),
        ).await.unwrap();

        store.upsert_entry(
            "doc2",
            vec![0.4, 0.5, 0.6],
            HashMap::from([("content".to_string(), "Python programming language".to_string())]),
            chrono::Utc::now(),
        ).await.unwrap();

        store.upsert_entry(
            "doc3",
            vec![0.7, 0.8, 0.9],
            HashMap::from([("content".to_string(), "The weather is sunny".to_string())]),
            chrono::Utc::now(),
        ).await.unwrap();

        // Keyword search
        let results = store.search_by_keyword("programming", 10).await.unwrap();
        assert_eq!(results.len(), 2);

        let results = store.search_by_keyword("rust", 10).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_embedded_vector_store_delete() {
        let store = ThreadSafeEmbeddedVectorStore::new();

        store.upsert_entry(
            "doc1",
            vec![1.0, 0.0, 0.0],
            HashMap::new(),
            chrono::Utc::now(),
        ).await.unwrap();

        assert_eq!(store.len().await, 1);

        store.delete_entry("doc1").await.unwrap();
        assert_eq!(store.len().await, 0);
    }

    #[test]
    fn test_hybrid_scorer() {
        let scorer = HybridScorer::new();
        // Default weights: alpha=0.7, beta=0.3
        let score = scorer.score(0.9, 0.5);
        assert!((score - (0.7 * 0.9 + 0.3 * 0.5)).abs() < 0.001);
    }

    #[test]
    fn test_hybrid_scorer_custom_weights() {
        let scorer = HybridScorer::with_weights(0.5, 0.5);
        let score = scorer.score(0.8, 0.6);
        assert!((score - 0.7).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_content_store() {
        let store = ThreadSafeContentStore::new();

        // Insert entries
        store.insert(MemoryEntry {
            id: "1".to_string(),
            content: "Rust programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        }).await;

        store.insert(MemoryEntry {
            id: "2".to_string(),
            content: "Python programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "assistant".to_string())]),
        }).await;

        store.insert(MemoryEntry {
            id: "3".to_string(),
            content: "The weather is sunny".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        }).await;

        assert_eq!(store.len().await, 3);

        // Keyword search
        let results = store.search_by_keyword("programming").await;
        assert_eq!(results.len(), 2);

        let results = store.search_by_keyword("rust").await;
        assert_eq!(results.len(), 1);

        // Keyword search with metadata filter
        let results = store.search_by_keyword_with_filter(
            "programming",
            &HashMap::from([("role".to_string(), "user".to_string())]),
        ).await;
        assert_eq!(results.len(), 1);

        // Get by ID
        let entry = store.get("1").await;
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().content, "Rust programming language");

        // Delete
        let removed = store.remove("3").await;
        assert!(removed.is_some());
        assert_eq!(store.len().await, 2);
    }

    #[tokio::test]
    async fn test_long_term_memory_store_and_retrieve() {
        let ltm = LongTermMemory::new();

        // Store an entry with embedding
        let entry = MemoryEntry {
            id: "1".to_string(),
            content: "Rust programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: Some(vec![0.1, 0.2, 0.3]),
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        };
        ltm.store(entry).await.unwrap();

        // Store another entry
        let entry2 = MemoryEntry {
            id: "2".to_string(),
            content: "Python programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: Some(vec![0.4, 0.5, 0.6]),
            metadata: HashMap::from([("role".to_string(), "assistant".to_string())]),
        };
        ltm.store(entry2).await.unwrap();

        // Retrieve by embedding (semantic search)
        let query = MemoryQuery {
            text: "programming".to_string(),
            embedding: Some(vec![0.1, 0.2, 0.35]),
            time_range: None,
            metadata_filters: HashMap::new(),
        };
        let results = ltm.retrieve(&query, 1).await.unwrap();
        assert_eq!(results.len(), 1);
        // The most similar entry should be "Rust" (closest to query embedding)
        assert_eq!(results[0].content, "Rust programming language");
    }

    #[tokio::test]
    async fn test_long_term_memory_keyword_search() {
        let ltm = LongTermMemory::new();

        // Store entries without embeddings
        ltm.store(MemoryEntry {
            id: "1".to_string(),
            content: "Rust programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        }).await.unwrap();

        ltm.store(MemoryEntry {
            id: "2".to_string(),
            content: "Python programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "assistant".to_string())]),
        }).await.unwrap();

        ltm.store(MemoryEntry {
            id: "3".to_string(),
            content: "The weather is sunny today".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        }).await.unwrap();

        // Keyword search (no embedding)
        let query = MemoryQuery {
            text: "programming".to_string(),
            embedding: None,
            time_range: None,
            metadata_filters: HashMap::new(),
        };
        let results = ltm.retrieve(&query, 10).await.unwrap();
        assert_eq!(results.len(), 2);

        // Keyword search with metadata filter
        let query = MemoryQuery {
            text: "programming".to_string(),
            embedding: None,
            time_range: None,
            metadata_filters: HashMap::from([("role".to_string(), "user".to_string())]),
        };
        let results = ltm.retrieve(&query, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "Rust programming language");
    }

    #[test]
    fn test_cosine_similarity() {
        // Identical vectors should have similarity 1.0
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = EmbeddedVectorStore::cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001);

        // Orthogonal vectors should have similarity 0.0
        let c = vec![0.0, 1.0, 0.0];
        let sim2 = EmbeddedVectorStore::cosine_similarity(&a, &c);
        assert!((sim2 - 0.0).abs() < 0.001);

        // Opposite vectors should have similarity -1.0
        let d = vec![-1.0, 0.0, 0.0];
        let sim3 = EmbeddedVectorStore::cosine_similarity(&a, &d);
        assert!((sim3 - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn test_temporal_score() {
        let now = chrono::Utc::now();

        // Same time should score 1.0
        let score = EmbeddedVectorStore::temporal_score(&now, &now);
        assert_eq!(score, 1.0);

        // 1 hour ago should score ~0.5 (half-life = 1 hour)
        let one_hour_ago = now - chrono::Duration::hours(1);
        let score2 = EmbeddedVectorStore::temporal_score(&one_hour_ago, &now);
        assert!((score2 - 0.5).abs() < 0.1);

        // Very old should score near 0
        let one_year_ago = now - chrono::Duration::days(365);
        let score3 = EmbeddedVectorStore::temporal_score(&one_year_ago, &now);
        assert!(score3 < 0.01);
    }
}