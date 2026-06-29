//! Usage management — recording LLM inference token usage per call and
//! aggregating per-session / per-model / global totals.
//!
//! OneAI tracks usage strictly by token dimensions (prompt / completion /
//! total / call_count / per-model). There is **no USD cost or budget
//! enforcement** here — cost/pricing data was removed entirely, and loop
//! termination is governed by `TokenBudget` / `ContextBudgetManager` in the
//! [`budget`](crate::budget) module.
//!
//! Key concepts:
//! - `UsageTracker`: Records usage per inference call, accumulates session/global totals
//! - `UsageRecord`: Single inference call usage data
//! - `UsageSummary`: Aggregated usage view (per session, per model, or global)
//! - `InMemoryUsageTracker`: Thread-safe in-memory implementation
//!
//! The UsageTracker is wired into AgentLoop — after each inference call,
//! usage is automatically recorded.
//!
//! For persistent usage tracking, use `SqliteUsageTracker` from oneai-persistence.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;

// ─── UsageTracker trait ──────────────────────────────────────────────────────

/// Trait for tracking LLM inference token usage.
///
/// Implementations record each inference call's token usage, accumulate
/// per-session and global totals, and provide per-model breakdowns.
///
/// The default implementation is `InMemoryUsageTracker` — thread-safe,
/// suitable for single-process sessions. For persistent usage tracking
/// across restarts, use `SqliteUsageTracker` from `oneai-persistence`.
#[async_trait::async_trait]
pub trait UsageTracker: Send + Sync {
    /// Record usage from an inference call.
    async fn record_usage(&self, record: UsageRecord) -> Result<()>;

    /// Get the total usage for a specific session.
    async fn session_usage(&self, session_id: &str) -> Result<UsageSummary>;

    /// Get the global usage across all sessions.
    async fn global_usage(&self) -> Result<UsageSummary>;

    /// Get usage breakdown by model for a specific session.
    async fn usage_by_model(&self, session_id: &str) -> Result<HashMap<String, UsageSummary>>;

    /// Get usage breakdown by model globally.
    async fn usage_by_model_global(&self) -> Result<HashMap<String, UsageSummary>>;

    /// Get all usage records for a session (for export/reporting).
    async fn session_records(&self, session_id: &str) -> Result<Vec<UsageRecord>>;

    /// Get all usage records globally (for export/reporting).
    async fn global_records(&self) -> Result<Vec<UsageRecord>>;

    /// Clear usage data for a specific session.
    async fn clear_session(&self, session_id: &str) -> Result<()>;

    /// Clear all usage data.
    async fn clear_all(&self) -> Result<()>;
}

// ─── UsageRecord ─────────────────────────────────────────────────────────────

/// A single inference call usage record.
///
/// Contains the token-usage data for one inference call: which model/provider
/// was used and how many prompt/completion tokens it consumed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UsageRecord {
    /// The session this call belongs to.
    pub session_id: String,

    /// The model that produced this response (e.g., "gpt-4o", "claude-opus-4").
    pub model: String,

    /// The provider used (e.g., "openai", "anthropic", "ollama").
    pub provider: String,

    /// Number of prompt (input) tokens.
    pub prompt_tokens: u32,

    /// Number of completion (output) tokens.
    pub completion_tokens: u32,

    /// When this call occurred.
    pub timestamp: DateTime<Utc>,

    /// Whether the token counts are **client-side estimates** rather than the
    /// provider's reported usage. This is `true` when the provider returned no
    /// usage in its (streaming) response — common for OpenAI-compatible
    /// providers that don't send `stream_options` (e.g. GLM). In that case the
    /// loop falls back to counting tokens locally with the `TokenCounter`, and
    /// marks the record so reports can distinguish real usage from estimates.
    #[serde(default)]
    pub is_estimated: bool,

    /// Additional metadata (e.g., tool call type).
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl UsageRecord {
    /// Create a new usage record.
    pub fn new(
        session_id: impl Into<String>,
        model: impl Into<String>,
        provider: impl Into<String>,
        prompt_tokens: u32,
        completion_tokens: u32,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            model: model.into(),
            provider: provider.into(),
            prompt_tokens,
            completion_tokens,
            timestamp: Utc::now(),
            is_estimated: false,
            metadata: HashMap::new(),
        }
    }

    /// Create a usage record with a specific timestamp (for loading from storage).
    ///
    /// This is used by persistent usage trackers when loading records
    /// from a database. The timestamp is preserved from storage rather
    /// than using the current time.
    pub fn with_timestamp(
        session_id: impl Into<String>,
        model: impl Into<String>,
        provider: impl Into<String>,
        prompt_tokens: u32,
        completion_tokens: u32,
        timestamp: DateTime<Utc>,
        metadata: HashMap<String, String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            model: model.into(),
            provider: provider.into(),
            prompt_tokens,
            completion_tokens,
            timestamp,
            is_estimated: false,
            metadata,
        }
    }

    /// Total tokens (prompt + completion) for this call.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens as u64 + self.completion_tokens as u64
    }

    /// Add metadata to this record.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Mark this record's token counts as client-side estimates (and supply the
    /// estimated token values). Used when the provider returned no usage
    /// and the loop counted tokens locally.
    pub fn estimated(mut self, prompt_tokens: u32, completion_tokens: u32) -> Self {
        self.prompt_tokens = prompt_tokens;
        self.completion_tokens = completion_tokens;
        self.is_estimated = true;
        self
    }
}

// ─── UsageSummary ────────────────────────────────────────────────────────────

/// Aggregated usage summary — per session, per model, or global.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UsageSummary {
    /// Total tokens (prompt + completion).
    pub total_tokens: u64,

    /// Prompt tokens.
    pub prompt_tokens: u64,

    /// Completion tokens.
    pub completion_tokens: u64,

    /// Number of inference calls.
    pub call_count: u64,

    /// Number of calls whose token counts are client-side estimates (provider
    /// returned no usage). Lets reports flag that part of the usage is estimated.
    #[serde(default)]
    pub estimated_call_count: u64,

    /// Timestamp of the first call.
    pub first_call: DateTime<Utc>,

    /// Timestamp of the most recent call.
    pub last_call: DateTime<Utc>,
}

impl UsageSummary {
    /// Create an empty usage summary.
    pub fn empty() -> Self {
        Self {
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            call_count: 0,
            estimated_call_count: 0,
            first_call: Utc::now(),
            last_call: Utc::now(),
        }
    }

    /// Create a usage summary from a list of usage records.
    pub fn from_records(records: &[UsageRecord]) -> Self {
        if records.is_empty() {
            return Self::empty();
        }

        let total_tokens = records.iter().map(|r| r.prompt_tokens as u64 + r.completion_tokens as u64).sum();
        let prompt_tokens = records.iter().map(|r| r.prompt_tokens as u64).sum();
        let completion_tokens = records.iter().map(|r| r.completion_tokens as u64).sum();
        let call_count = records.len() as u64;
        let estimated_call_count = records.iter().filter(|r| r.is_estimated).count() as u64;
        let first_call = records.iter().map(|r| r.timestamp).min().unwrap_or(Utc::now());
        let last_call = records.iter().map(|r| r.timestamp).max().unwrap_or(Utc::now());

        Self {
            total_tokens,
            prompt_tokens,
            completion_tokens,
            call_count,
            estimated_call_count,
            first_call,
            last_call,
        }
    }

    /// Add a single usage record to this summary.
    pub fn add_record(&mut self, record: &UsageRecord) {
        self.total_tokens += record.prompt_tokens as u64 + record.completion_tokens as u64;
        self.prompt_tokens += record.prompt_tokens as u64;
        self.completion_tokens += record.completion_tokens as u64;
        self.call_count += 1;
        if record.is_estimated {
            self.estimated_call_count += 1;
        }
        self.last_call = record.timestamp;
        if self.call_count == 1 {
            self.first_call = record.timestamp;
        }
    }
}

impl Default for UsageSummary {
    fn default() -> Self {
        Self::empty()
    }
}

// ─── InMemoryUsageTracker ────────────────────────────────────────────────────

/// Thread-safe in-memory usage tracker — suitable for single-process sessions.
///
/// Stores usage records per session_id in a HashMap, with global aggregation.
///
/// For persistent usage tracking across restarts, use `SqliteUsageTracker`
/// from `oneai-persistence`.
pub struct InMemoryUsageTracker {
    /// Per-session usage records.
    sessions: tokio::sync::RwLock<HashMap<String, Vec<UsageRecord>>>,

    /// Global usage records (all sessions).
    global: tokio::sync::RwLock<Vec<UsageRecord>>,
}

impl InMemoryUsageTracker {
    /// Create a new in-memory usage tracker.
    pub fn new() -> Self {
        Self {
            sessions: tokio::sync::RwLock::new(HashMap::new()),
            global: tokio::sync::RwLock::new(Vec::new()),
        }
    }
}

impl Default for InMemoryUsageTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl UsageTracker for InMemoryUsageTracker {
    async fn record_usage(&self, record: UsageRecord) -> Result<()> {
        // Add to session records
        let mut sessions = self.sessions.write().await;
        sessions
            .entry(record.session_id.clone())
            .or_insert_with(Vec::new)
            .push(record.clone());

        // Add to global records
        let mut global = self.global.write().await;
        global.push(record);

        Ok(())
    }

    async fn session_usage(&self, session_id: &str) -> Result<UsageSummary> {
        let sessions = self.sessions.read().await;
        let records = sessions.get(session_id).cloned().unwrap_or_default();
        Ok(UsageSummary::from_records(&records))
    }

    async fn global_usage(&self) -> Result<UsageSummary> {
        let global = self.global.read().await;
        Ok(UsageSummary::from_records(&global))
    }

    async fn usage_by_model(&self, session_id: &str) -> Result<HashMap<String, UsageSummary>> {
        let sessions = self.sessions.read().await;
        let records = sessions.get(session_id).cloned().unwrap_or_default();

        let mut by_model: HashMap<String, Vec<UsageRecord>> = HashMap::new();
        for record in records {
            by_model.entry(record.model.clone()).or_default().push(record);
        }

        Ok(by_model.into_iter()
            .map(|(model, records)| (model, UsageSummary::from_records(&records)))
            .collect())
    }

    async fn usage_by_model_global(&self) -> Result<HashMap<String, UsageSummary>> {
        let global = self.global.read().await;

        let mut by_model: HashMap<String, Vec<UsageRecord>> = HashMap::new();
        for record in global.iter() {
            by_model.entry(record.model.clone()).or_default().push(record.clone());
        }

        Ok(by_model.into_iter()
            .map(|(model, records)| (model, UsageSummary::from_records(&records)))
            .collect())
    }

    async fn session_records(&self, session_id: &str) -> Result<Vec<UsageRecord>> {
        let sessions = self.sessions.read().await;
        Ok(sessions.get(session_id).cloned().unwrap_or_default())
    }

    async fn global_records(&self) -> Result<Vec<UsageRecord>> {
        let global = self.global.read().await;
        Ok(global.clone())
    }

    async fn clear_session(&self, session_id: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        sessions.remove(session_id);

        // Also remove session records from global
        let mut global = self.global.write().await;
        global.retain(|r| r.session_id != session_id);

        Ok(())
    }

    async fn clear_all(&self) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        sessions.clear();
        let mut global = self.global.write().await;
        global.clear();
        Ok(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usage_record_creation() {
        let record = UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50);
        assert_eq!(record.session_id, "sess1");
        assert_eq!(record.model, "gpt-4o");
        assert_eq!(record.prompt_tokens, 100);
        assert_eq!(record.completion_tokens, 50);
        assert_eq!(record.total_tokens(), 150);
        assert!(record.metadata.is_empty());
    }

    #[test]
    fn test_usage_record_with_metadata() {
        let record = UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)
            .with_metadata("tool", "shell")
            .with_metadata("iteration", "3");
        assert_eq!(record.metadata.get("tool"), Some(&"shell".to_string()));
        assert_eq!(record.metadata.get("iteration"), Some(&"3".to_string()));
    }

    #[test]
    fn test_usage_summary_empty() {
        let summary = UsageSummary::empty();
        assert_eq!(summary.total_tokens, 0);
        assert_eq!(summary.call_count, 0);
    }

    #[test]
    fn test_usage_summary_from_records() {
        let records = vec![
            UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50),
            UsageRecord::new("sess1", "gpt-4o", "openai", 200, 100),
        ];
        let summary = UsageSummary::from_records(&records);
        assert_eq!(summary.total_tokens, 450); // 150 + 300
        assert_eq!(summary.prompt_tokens, 300);
        assert_eq!(summary.completion_tokens, 150);
        assert_eq!(summary.call_count, 2);
    }

    #[test]
    fn test_usage_summary_add_record() {
        let mut summary = UsageSummary::empty();
        let record = UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50);
        summary.add_record(&record);
        assert_eq!(summary.total_tokens, 150);
        assert_eq!(summary.call_count, 1);
    }

    #[test]
    fn test_usage_record_estimated_flag_and_serde_backcompat() {
        // is_estimated defaults false on the plain constructor.
        let r = UsageRecord::new("s", "gpt-4o", "openai", 100, 50);
        assert!(!r.is_estimated);

        // estimated() builder sets the flag + overrides counts.
        let e = UsageRecord::new("s", "gpt-4o", "openai", 0, 0).estimated(120, 60);
        assert!(e.is_estimated);
        assert_eq!(e.prompt_tokens, 120);
        assert_eq!(e.completion_tokens, 60);

        // Backward-compat: a serialized record WITHOUT is_estimated (old format)
        // must still deserialize (field defaults via #[serde(default)]).
        let old_json = r#"{"session_id":"s","model":"gpt-4o","provider":"openai","prompt_tokens":100,"completion_tokens":50,"timestamp":"2026-06-23T00:00:00Z","metadata":{}}"#;
        let parsed: UsageRecord = serde_json::from_str(old_json).expect("old record deserializes");
        assert!(!parsed.is_estimated, "missing field defaults to false");
        assert_eq!(parsed.prompt_tokens, 100);
    }

    #[test]
    fn test_usage_summary_counts_estimated_calls() {
        let records = vec![
            UsageRecord::new("s", "m", "p", 100, 50),
            UsageRecord::new("s", "m", "p", 0, 0).estimated(200, 100),
            UsageRecord::new("s", "m", "p", 0, 0).estimated(300, 150),
        ];
        let summary = UsageSummary::from_records(&records);
        assert_eq!(summary.call_count, 3);
        assert_eq!(summary.estimated_call_count, 2);
        // Estimated counts are included in the totals.
        assert_eq!(summary.prompt_tokens, 100 + 200 + 300);
        assert_eq!(summary.completion_tokens, 50 + 100 + 150);
    }

    #[tokio::test]
    async fn test_in_memory_usage_tracker_record_and_query() {
        let tracker = InMemoryUsageTracker::new();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "claude-sonnet-4", "anthropic", 200, 100)).await.unwrap();

        let session_usage = tracker.session_usage("sess1").await.unwrap();
        assert_eq!(session_usage.call_count, 2);
        assert_eq!(session_usage.total_tokens, 450);

        let global_usage = tracker.global_usage().await.unwrap();
        assert_eq!(global_usage.call_count, 2);
    }

    #[tokio::test]
    async fn test_in_memory_usage_tracker_per_model_breakdown() {
        let tracker = InMemoryUsageTracker::new();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "claude-sonnet-4", "anthropic", 200, 100)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 300, 150)).await.unwrap();

        let by_model = tracker.usage_by_model("sess1").await.unwrap();
        assert_eq!(by_model.len(), 2);

        let gpt4o = by_model.get("gpt-4o").unwrap();
        assert_eq!(gpt4o.call_count, 2);
        assert_eq!(gpt4o.total_tokens, 600); // 150 + 450

        let claude = by_model.get("claude-sonnet-4").unwrap();
        assert_eq!(claude.call_count, 1);
        assert_eq!(claude.total_tokens, 300);
    }

    #[tokio::test]
    async fn test_in_memory_usage_tracker_clear() {
        let tracker = InMemoryUsageTracker::new();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess2", "gpt-4o", "openai", 100, 50)).await.unwrap();

        tracker.clear_session("sess1").await.unwrap();
        let sess1 = tracker.session_usage("sess1").await.unwrap();
        assert_eq!(sess1.call_count, 0);

        let global = tracker.global_usage().await.unwrap();
        assert_eq!(global.call_count, 1); // sess2 still exists

        tracker.clear_all().await.unwrap();
        let global2 = tracker.global_usage().await.unwrap();
        assert_eq!(global2.call_count, 0);
    }

    #[tokio::test]
    async fn test_in_memory_usage_tracker_session_records() {
        let tracker = InMemoryUsageTracker::new();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "claude-sonnet-4", "anthropic", 200, 100)).await.unwrap();

        let records = tracker.session_records("sess1").await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].model, "gpt-4o");
        assert_eq!(records[1].model, "claude-sonnet-4");
    }
}
