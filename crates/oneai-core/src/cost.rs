//! Cost & Usage management — tracking LLM inference costs, enforcing budgets,
//! and providing cost reporting for production deployment.
//!
//! Key concepts:
//! - `CostTracker`: Records usage per inference call, accumulates session/global totals
//! - `ModelPricingCatalog`: Per-model pricing data for cost computation
//! - `UsageRecord`: Single inference call usage data
//! - `CostSummary`: Aggregated cost view (per session, per model, or global)
//! - `BudgetStatus`: Budget enforcement result
//! - `CostBudgetConfig`: Session budget limits
//! - `InMemoryCostTracker`: Thread-safe in-memory implementation
//!
//! The CostTracker is wired into AgentLoop — after each inference call,
//! usage is automatically recorded. Budget enforcement checks happen
//! before each iteration, and the loop terminates when the budget is exceeded.
//!
//! For persistent cost tracking, use `SqliteCostTracker` from oneai-persistence.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;

// ─── CostTracker trait ───────────────────────────────────────────────────────

/// Trait for tracking LLM inference costs and enforcing budgets.
///
/// Implementations record each inference call's token usage and computed cost,
/// accumulate per-session and global totals, and check budget limits.
///
/// The default implementation is `InMemoryCostTracker` — thread-safe,
/// suitable for single-process sessions. For persistent cost tracking
/// across restarts, use `SqliteCostTracker` from `oneai-persistence`.
#[async_trait::async_trait]
pub trait CostTracker: Send + Sync {
    /// Record usage from an inference call.
    async fn record_usage(&self, record: UsageRecord) -> Result<()>;

    /// Get the total cost for a specific session.
    async fn session_cost(&self, session_id: &str) -> Result<CostSummary>;

    /// Get the global cost across all sessions.
    async fn global_cost(&self) -> Result<CostSummary>;

    /// Check whether the budget allows another inference call for a session.
    ///
    /// Returns `BudgetStatus` with remaining budget info and whether
    /// the budget is exceeded. The AgentLoop checks this before each iteration.
    async fn check_budget(&self, session_id: &str) -> Result<BudgetStatus>;

    /// Get cost breakdown by model for a specific session.
    async fn cost_by_model(&self, session_id: &str) -> Result<HashMap<String, CostSummary>>;

    /// Get cost breakdown by model globally.
    async fn cost_by_model_global(&self) -> Result<HashMap<String, CostSummary>>;

    /// Get all usage records for a session (for export/reporting).
    async fn session_records(&self, session_id: &str) -> Result<Vec<UsageRecord>>;

    /// Get all usage records globally (for export/reporting).
    async fn global_records(&self) -> Result<Vec<UsageRecord>>;

    /// Clear cost data for a specific session.
    async fn clear_session(&self, session_id: &str) -> Result<()>;

    /// Clear all cost data.
    async fn clear_all(&self) -> Result<()>;
}

// ─── UsageRecord ─────────────────────────────────────────────────────────────

/// A single inference call usage record.
///
/// Contains all the information needed to compute and track costs:
/// which model was used, how many tokens, and the computed cost in USD.
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

    /// The computed cost in USD for this call.
    pub cost_usd: f64,

    /// When this call occurred.
    pub timestamp: DateTime<Utc>,

    /// Additional metadata (e.g., embedding cost, tool call type).
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
        cost_usd: f64,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            model: model.into(),
            provider: provider.into(),
            prompt_tokens,
            completion_tokens,
            cost_usd,
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        }
    }

    /// Create a usage record with a specific timestamp (for loading from storage).
    ///
    /// This is used by persistent cost trackers when loading records
    /// from a database. The timestamp is preserved from storage rather
    /// than using the current time.
    pub fn with_timestamp(
        session_id: impl Into<String>,
        model: impl Into<String>,
        provider: impl Into<String>,
        prompt_tokens: u32,
        completion_tokens: u32,
        cost_usd: f64,
        timestamp: DateTime<Utc>,
        metadata: HashMap<String, String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            model: model.into(),
            provider: provider.into(),
            prompt_tokens,
            completion_tokens,
            cost_usd,
            timestamp,
            metadata,
        }
    }

    /// Create from an InferenceResponse's usage data.
    ///
    /// Uses the `ModelPricingCatalog` to compute the cost.
    pub fn from_response(
        session_id: impl Into<String>,
        model: impl Into<String>,
        provider: impl Into<String>,
        prompt_tokens: u32,
        completion_tokens: u32,
        catalog: &ModelPricingCatalog,
    ) -> Self {
        let model_str = model.into();
        let cost_usd = catalog.compute_cost(&model_str, prompt_tokens, completion_tokens);
        Self::new(session_id, model_str, provider.into(), prompt_tokens, completion_tokens, cost_usd)
    }

    /// Add metadata to this record.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

// ─── CostSummary ─────────────────────────────────────────────────────────────

/// Aggregated cost summary — per session, per model, or global.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CostSummary {
    /// Total tokens (prompt + completion).
    pub total_tokens: u64,

    /// Prompt tokens.
    pub prompt_tokens: u64,

    /// Completion tokens.
    pub completion_tokens: u64,

    /// Total cost in USD.
    pub total_cost_usd: f64,

    /// Number of inference calls.
    pub call_count: u64,

    /// Average cost per call in USD.
    pub avg_cost_per_call: f64,

    /// Timestamp of the first call.
    pub first_call: DateTime<Utc>,

    /// Timestamp of the most recent call.
    pub last_call: DateTime<Utc>,
}

impl CostSummary {
    /// Create an empty cost summary.
    pub fn empty() -> Self {
        Self {
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_cost_usd: 0.0,
            call_count: 0,
            avg_cost_per_call: 0.0,
            first_call: Utc::now(),
            last_call: Utc::now(),
        }
    }

    /// Create a cost summary from a list of usage records.
    pub fn from_records(records: &[UsageRecord]) -> Self {
        if records.is_empty() {
            return Self::empty();
        }

        let total_tokens = records.iter().map(|r| r.prompt_tokens as u64 + r.completion_tokens as u64).sum();
        let prompt_tokens = records.iter().map(|r| r.prompt_tokens as u64).sum();
        let completion_tokens = records.iter().map(|r| r.completion_tokens as u64).sum();
        let total_cost_usd = records.iter().map(|r| r.cost_usd).sum();
        let call_count = records.len() as u64;
        let avg_cost_per_call = if call_count > 0 { total_cost_usd / call_count as f64 } else { 0.0 };
        let first_call = records.iter().map(|r| r.timestamp).min().unwrap_or(Utc::now());
        let last_call = records.iter().map(|r| r.timestamp).max().unwrap_or(Utc::now());

        Self {
            total_tokens,
            prompt_tokens,
            completion_tokens,
            total_cost_usd,
            call_count,
            avg_cost_per_call,
            first_call,
            last_call,
        }
    }

    /// Add a single usage record to this summary.
    pub fn add_record(&mut self, record: &UsageRecord) {
        self.total_tokens += record.prompt_tokens as u64 + record.completion_tokens as u64;
        self.prompt_tokens += record.prompt_tokens as u64;
        self.completion_tokens += record.completion_tokens as u64;
        self.total_cost_usd += record.cost_usd;
        self.call_count += 1;
        self.avg_cost_per_call = self.total_cost_usd / self.call_count as f64;
        self.last_call = record.timestamp;
        if self.call_count == 1 {
            self.first_call = record.timestamp;
        }
    }
}

impl Default for CostSummary {
    fn default() -> Self {
        Self::empty()
    }
}

// ─── BudgetStatus ────────────────────────────────────────────────────────────

/// Budget enforcement result — whether another inference call is allowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BudgetStatus {
    /// Remaining budget in USD.
    pub remaining_usd: f64,

    /// Remaining token budget.
    pub remaining_tokens: u64,

    /// Whether the budget has been exceeded.
    pub budget_exceeded: bool,

    /// The budget limit in USD (if configured).
    pub budget_limit_usd: Option<f64>,

    /// The budget limit in tokens (if configured).
    pub budget_limit_tokens: Option<u64>,

    /// The budget limit in calls (if configured).
    pub budget_limit_calls: Option<u64>,

    /// Number of calls made so far in this session.
    pub calls_made: u64,
}

impl BudgetStatus {
    /// Create a budget status indicating unlimited budget (no limits set).
    pub fn unlimited(calls_made: u64) -> Self {
        Self {
            remaining_usd: f64::INFINITY,
            remaining_tokens: u64::MAX,
            budget_exceeded: false,
            budget_limit_usd: None,
            budget_limit_tokens: None,
            budget_limit_calls: None,
            calls_made,
        }
    }

    /// Whether another inference call is allowed.
    pub fn is_allowed(&self) -> bool {
        !self.budget_exceeded
    }
}

// ─── CostBudgetConfig ────────────────────────────────────────────────────────

/// Configuration for session budget limits.
///
/// Budgets can be set in USD, tokens, or call count. When any limit is exceeded,
/// the AgentLoop terminates the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CostBudgetConfig {
    /// Maximum cost in USD for a session.
    pub max_cost_usd: Option<f64>,

    /// Maximum total tokens for a session.
    pub max_tokens: Option<u64>,

    /// Maximum number of inference calls for a session.
    pub max_calls: Option<u64>,

    /// Per-model cost limit in USD (optional overrides).
    #[serde(default)]
    pub per_model_limits: HashMap<String, f64>,
}

impl Default for CostBudgetConfig {
    fn default() -> Self {
        Self {
            max_cost_usd: None,
            max_tokens: None,
            max_calls: None,
            per_model_limits: HashMap::new(),
        }
    }
}

impl CostBudgetConfig {
    /// Create an unlimited budget (no limits).
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// Create a budget limited by cost in USD.
    pub fn with_cost_limit(max_cost_usd: f64) -> Self {
        Self {
            max_cost_usd: Some(max_cost_usd),
            max_tokens: None,
            max_calls: None,
            per_model_limits: HashMap::new(),
        }
    }

    /// Create a budget limited by token count.
    pub fn with_token_limit(max_tokens: u64) -> Self {
        Self {
            max_cost_usd: None,
            max_tokens: Some(max_tokens),
            max_calls: None,
            per_model_limits: HashMap::new(),
        }
    }

    /// Create a budget limited by call count.
    pub fn with_call_limit(max_calls: u64) -> Self {
        Self {
            max_cost_usd: None,
            max_tokens: None,
            max_calls: Some(max_calls),
            per_model_limits: HashMap::new(),
        }
    }

    /// Add a per-model cost limit.
    pub fn with_model_limit(mut self, model: impl Into<String>, max_cost_usd: f64) -> Self {
        self.per_model_limits.insert(model.into(), max_cost_usd);
        self
    }

    /// Check whether a cost summary exceeds any of the budget limits.
    pub fn is_exceeded(&self, summary: &CostSummary) -> bool {
        if let Some(max_cost) = self.max_cost_usd {
            if summary.total_cost_usd >= max_cost {
                return true;
            }
        }
        if let Some(max_tokens) = self.max_tokens {
            if summary.total_tokens >= max_tokens {
                return true;
            }
        }
        if let Some(max_calls) = self.max_calls {
            if summary.call_count >= max_calls {
                return true;
            }
        }
        false
    }

    /// Compute budget status from a cost summary and budget config.
    pub fn compute_status(&self, summary: &CostSummary) -> BudgetStatus {
        BudgetStatus {
            remaining_usd: self.max_cost_usd.map_or(f64::INFINITY, |limit| limit - summary.total_cost_usd),
            remaining_tokens: self.max_tokens.map_or(u64::MAX, |limit| limit.saturating_sub(summary.total_tokens)),
            budget_exceeded: self.is_exceeded(summary),
            budget_limit_usd: self.max_cost_usd,
            budget_limit_tokens: self.max_tokens,
            budget_limit_calls: self.max_calls,
            calls_made: summary.call_count,
        }
    }
}

// ─── ModelPricingEntry ───────────────────────────────────────────────────────

/// Pricing entry for a specific model — cost per 1K tokens in USD.
///
/// Each model has separate pricing for prompt (input) and completion (output) tokens.
/// Some models also have embedding pricing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelPricingEntry {
    /// Model name (e.g., "gpt-4o", "claude-opus-4").
    pub model_name: String,

    /// Provider name (e.g., "openai", "anthropic").
    pub provider: String,

    /// Cost per 1K prompt (input) tokens in USD.
    pub prompt_per_1k_usd: f64,

    /// Cost per 1K completion (output) tokens in USD.
    pub completion_per_1k_usd: f64,

    /// Cost per 1K embedding tokens in USD (if applicable).
    pub embedding_per_1k_usd: Option<f64>,
}

impl ModelPricingEntry {
    /// Create a new pricing entry.
    pub fn new(
        model_name: impl Into<String>,
        provider: impl Into<String>,
        prompt_per_1k_usd: f64,
        completion_per_1k_usd: f64,
    ) -> Self {
        Self {
            model_name: model_name.into(),
            provider: provider.into(),
            prompt_per_1k_usd,
            completion_per_1k_usd,
            embedding_per_1k_usd: None,
        }
    }

    /// Create a pricing entry with embedding costs.
    pub fn with_embedding(
        model_name: impl Into<String>,
        provider: impl Into<String>,
        prompt_per_1k_usd: f64,
        completion_per_1k_usd: f64,
        embedding_per_1k_usd: f64,
    ) -> Self {
        Self {
            model_name: model_name.into(),
            provider: provider.into(),
            prompt_per_1k_usd,
            completion_per_1k_usd,
            embedding_per_1k_usd: Some(embedding_per_1k_usd),
        }
    }

    /// Default pricing — used when the model is unknown.
    ///
    /// Uses rough GPT-4 rates ($0.03/1K prompt, $0.06/1K completion)
    /// matching the existing AgentLoopConfig default.
    pub fn default_pricing() -> Self {
        Self::new("unknown", "unknown", 0.03, 0.06)
    }

    /// Free pricing — for local models (Ollama, vLLM).
    pub fn free_pricing() -> Self {
        Self::new("local", "ollama", 0.0, 0.0)
    }

    /// Compute cost for a given token usage.
    pub fn compute_cost(&self, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        (prompt_tokens as f64 / 1000.0) * self.prompt_per_1k_usd
            + (completion_tokens as f64 / 1000.0) * self.completion_per_1k_usd
    }
}

// ─── ModelPricingCatalog ─────────────────────────────────────────────────────

/// Catalog of model pricing data — per-model cost per 1K tokens.
///
/// Pre-loaded with pricing for known models. For unknown models,
/// falls back to default pricing (rough GPT-4 rates).
///
/// Pricing data is approximate and based on publicly available information
/// as of mid-2026. Actual pricing may vary — always check the provider's
/// current pricing page for the latest rates.
#[derive(Debug, Clone)]
pub struct ModelPricingCatalog {
    /// Per-model pricing entries, keyed by model name.
    pricings: HashMap<String, ModelPricingEntry>,

    /// Default pricing for unknown models.
    default: ModelPricingEntry,
}

impl Default for ModelPricingCatalog {
    fn default() -> Self {
        Self::with_known_models()
    }
}

impl ModelPricingCatalog {
    /// Create a catalog pre-loaded with known model pricing.
    ///
    /// Includes pricing for: GPT-4o, GPT-4o-mini, Claude Opus 4,
    /// Claude Sonnet 4, Claude Haiku 4, Gemini 2.5 Pro,
    /// Gemini 2.5 Flash, DeepSeek Chat, DeepSeek Reasoner,
    /// and Ollama (free).
    pub fn with_known_models() -> Self {
        let mut pricings = HashMap::new();

        // OpenAI
        pricings.insert("gpt-4o".to_string(), ModelPricingEntry::new("gpt-4o", "openai", 2.50, 10.00));
        pricings.insert("gpt-4o-mini".to_string(), ModelPricingEntry::new("gpt-4o-mini", "openai", 0.15, 0.60));
        pricings.insert("gpt-4.1".to_string(), ModelPricingEntry::new("gpt-4.1", "openai", 2.0, 8.0));
        pricings.insert("gpt-4.1-mini".to_string(), ModelPricingEntry::new("gpt-4.1-mini", "openai", 0.40, 1.60));
        pricings.insert("gpt-4.1-nano".to_string(), ModelPricingEntry::new("gpt-4.1-nano", "openai", 0.10, 0.40));
        pricings.insert("o3".to_string(), ModelPricingEntry::new("o3", "openai", 10.0, 40.0));
        pricings.insert("o3-mini".to_string(), ModelPricingEntry::new("o3-mini", "openai", 1.10, 4.40));
        pricings.insert("o4-mini".to_string(), ModelPricingEntry::new("o4-mini", "openai", 1.10, 4.40));
        // OpenAI embedding pricing
        pricings.insert("text-embedding-3-small".to_string(),
            ModelPricingEntry::with_embedding("text-embedding-3-small", "openai", 0.0, 0.0, 0.02));
        pricings.insert("text-embedding-3-large".to_string(),
            ModelPricingEntry::with_embedding("text-embedding-3-large", "openai", 0.0, 0.0, 0.13));

        // Anthropic
        pricings.insert("claude-opus-4".to_string(), ModelPricingEntry::new("claude-opus-4", "anthropic", 15.0, 75.0));
        pricings.insert("claude-sonnet-4".to_string(), ModelPricingEntry::new("claude-sonnet-4", "anthropic", 3.0, 15.0));
        pricings.insert("claude-haiku-4".to_string(), ModelPricingEntry::new("claude-haiku-4", "anthropic", 0.80, 4.0));
        pricings.insert("claude-opus-4-8".to_string(), ModelPricingEntry::new("claude-opus-4-8", "anthropic", 15.0, 75.0));
        pricings.insert("claude-sonnet-4-6".to_string(), ModelPricingEntry::new("claude-sonnet-4-6", "anthropic", 3.0, 15.0));

        // Google Gemini
        pricings.insert("gemini-2.5-pro".to_string(), ModelPricingEntry::new("gemini-2.5-pro", "google", 1.25, 10.0));
        pricings.insert("gemini-2.5-flash".to_string(), ModelPricingEntry::new("gemini-2.5-flash", "google", 0.15, 0.60));
        pricings.insert("gemini-2.0-flash".to_string(), ModelPricingEntry::new("gemini-2.0-flash", "google", 0.10, 0.40));

        // DeepSeek
        pricings.insert("deepseek-chat".to_string(), ModelPricingEntry::new("deepseek-chat", "deepseek", 0.14, 0.28));
        pricings.insert("deepseek-reasoner".to_string(), ModelPricingEntry::new("deepseek-reasoner", "deepseek", 0.55, 2.19));

        // Local (free)
        pricings.insert("llama3".to_string(), ModelPricingEntry::free_pricing());
        pricings.insert("llama3.1".to_string(), ModelPricingEntry::free_pricing());
        pricings.insert("llama3.2".to_string(), ModelPricingEntry::free_pricing());
        pricings.insert("mistral".to_string(), ModelPricingEntry::free_pricing());
        pricings.insert("codellama".to_string(), ModelPricingEntry::free_pricing());
        pricings.insert("qwen2".to_string(), ModelPricingEntry::free_pricing());
        pricings.insert("nomic-embed-text".to_string(), ModelPricingEntry::free_pricing());

        Self {
            pricings,
            default: ModelPricingEntry::default_pricing(),
        }
    }

    /// Create an empty catalog (only default pricing for unknown models).
    pub fn empty() -> Self {
        Self {
            pricings: HashMap::new(),
            default: ModelPricingEntry::default_pricing(),
        }
    }

    /// Add a pricing entry to the catalog.
    pub fn add_pricing(&mut self, entry: ModelPricingEntry) {
        self.pricings.insert(entry.model_name.clone(), entry);
    }

    /// Look up pricing for a model.
    ///
    /// If the model is not in the catalog, returns the default pricing.
    /// The default uses rough GPT-4 rates ($0.03/1K prompt, $0.06/1K completion).
    pub fn lookup(&self, model_name: &str) -> &ModelPricingEntry {
        // Try exact match first
        if let Some(entry) = self.pricings.get(model_name) {
            return entry;
        }

        // Try partial match (e.g., "gpt-4o-2024-05-13" matches "gpt-4o")
        for (key, entry) in &self.pricings {
            if model_name.starts_with(key) || key.starts_with(model_name) {
                return entry;
            }
        }

        &self.default
    }

    /// Compute cost for a given model and token usage.
    pub fn compute_cost(&self, model_name: &str, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        self.lookup(model_name).compute_cost(prompt_tokens, completion_tokens)
    }

    /// Get all known model names.
    pub fn known_models(&self) -> Vec<String> {
        self.pricings.keys().cloned().collect()
    }

    /// Get all pricing entries.
    pub fn all_pricings(&self) -> &[ModelPricingEntry] {
        // Can't return &[] directly since HashMap doesn't have ordered access
        // but we can collect into a vec and return as slice
        // For simplicity, we provide a method that returns a Vec
        unimplemented!("Use known_models() + lookup() instead")
    }

    /// Get all pricing entries as a vector.
    pub fn pricing_entries(&self) -> Vec<&ModelPricingEntry> {
        self.pricings.values().collect()
    }

    /// Number of known models in the catalog.
    pub fn model_count(&self) -> usize {
        self.pricings.len()
    }
}

// ─── InMemoryCostTracker ─────────────────────────────────────────────────────

/// Thread-safe in-memory cost tracker — suitable for single-process sessions.
///
/// Stores usage records per session_id in a HashMap, with global aggregation.
/// Budget checking uses a `CostBudgetConfig` (default: unlimited).
///
/// For persistent cost tracking across restarts, use `SqliteCostTracker`
/// from `oneai-persistence`.
pub struct InMemoryCostTracker {
    /// Per-session usage records.
    sessions: tokio::sync::RwLock<HashMap<String, Vec<UsageRecord>>>,

    /// Global usage records (all sessions).
    global: tokio::sync::RwLock<Vec<UsageRecord>>,

    /// Budget configuration.
    budget_config: CostBudgetConfig,
}

impl InMemoryCostTracker {
    /// Create a new in-memory cost tracker with unlimited budget.
    pub fn new() -> Self {
        Self {
            sessions: tokio::sync::RwLock::new(HashMap::new()),
            global: tokio::sync::RwLock::new(Vec::new()),
            budget_config: CostBudgetConfig::unlimited(),
        }
    }

    /// Create with a specific budget configuration.
    pub fn with_budget(budget_config: CostBudgetConfig) -> Self {
        Self {
            sessions: tokio::sync::RwLock::new(HashMap::new()),
            global: tokio::sync::RwLock::new(Vec::new()),
            budget_config,
        }
    }
}

impl Default for InMemoryCostTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl CostTracker for InMemoryCostTracker {
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

    async fn session_cost(&self, session_id: &str) -> Result<CostSummary> {
        let sessions = self.sessions.read().await;
        let records = sessions.get(session_id).cloned().unwrap_or_default();
        Ok(CostSummary::from_records(&records))
    }

    async fn global_cost(&self) -> Result<CostSummary> {
        let global = self.global.read().await;
        Ok(CostSummary::from_records(&global))
    }

    async fn check_budget(&self, session_id: &str) -> Result<BudgetStatus> {
        let sessions = self.sessions.read().await;
        let records = sessions.get(session_id).cloned().unwrap_or_default();
        let summary = CostSummary::from_records(&records);
        Ok(self.budget_config.compute_status(&summary))
    }

    async fn cost_by_model(&self, session_id: &str) -> Result<HashMap<String, CostSummary>> {
        let sessions = self.sessions.read().await;
        let records = sessions.get(session_id).cloned().unwrap_or_default();

        let mut by_model: HashMap<String, Vec<UsageRecord>> = HashMap::new();
        for record in records {
            by_model.entry(record.model.clone()).or_default().push(record);
        }

        Ok(by_model.into_iter()
            .map(|(model, records)| (model, CostSummary::from_records(&records)))
            .collect())
    }

    async fn cost_by_model_global(&self) -> Result<HashMap<String, CostSummary>> {
        let global = self.global.read().await;

        let mut by_model: HashMap<String, Vec<UsageRecord>> = HashMap::new();
        for record in global.iter() {
            by_model.entry(record.model.clone()).or_default().push(record.clone());
        }

        Ok(by_model.into_iter()
            .map(|(model, records)| (model, CostSummary::from_records(&records)))
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
        let record = UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5);
        assert_eq!(record.session_id, "sess1");
        assert_eq!(record.model, "gpt-4o");
        assert_eq!(record.prompt_tokens, 100);
        assert_eq!(record.completion_tokens, 50);
        assert_eq!(record.cost_usd, 0.5);
        assert!(record.metadata.is_empty());
    }

    #[test]
    fn test_usage_record_from_response() {
        let catalog = ModelPricingCatalog::with_known_models();
        let record = UsageRecord::from_response("sess1", "gpt-4o", "openai", 1000, 500, &catalog);
        assert_eq!(record.session_id, "sess1");
        assert_eq!(record.prompt_tokens, 1000);
        assert_eq!(record.completion_tokens, 500);
        // gpt-4o: $2.50/1K prompt + $10.00/1K completion
        let expected = (1000.0 / 1000.0) * 2.50 + (500.0 / 1000.0) * 10.00;
        assert!((record.cost_usd - expected).abs() < 0.01);
    }

    #[test]
    fn test_usage_record_with_metadata() {
        let record = UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5)
            .with_metadata("tool", "shell")
            .with_metadata("iteration", "3");
        assert_eq!(record.metadata.get("tool"), Some(&"shell".to_string()));
        assert_eq!(record.metadata.get("iteration"), Some(&"3".to_string()));
    }

    #[test]
    fn test_cost_summary_empty() {
        let summary = CostSummary::empty();
        assert_eq!(summary.total_tokens, 0);
        assert_eq!(summary.total_cost_usd, 0.0);
        assert_eq!(summary.call_count, 0);
    }

    #[test]
    fn test_cost_summary_from_records() {
        let records = vec![
            UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5),
            UsageRecord::new("sess1", "gpt-4o", "openai", 200, 100, 1.0),
        ];
        let summary = CostSummary::from_records(&records);
        assert_eq!(summary.total_tokens, 450); // 150 + 300
        assert_eq!(summary.prompt_tokens, 300);
        assert_eq!(summary.completion_tokens, 150);
        assert_eq!(summary.total_cost_usd, 1.5);
        assert_eq!(summary.call_count, 2);
        assert!((summary.avg_cost_per_call - 0.75).abs() < 0.01);
    }

    #[test]
    fn test_cost_summary_add_record() {
        let mut summary = CostSummary::empty();
        let record = UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5);
        summary.add_record(&record);
        assert_eq!(summary.total_tokens, 150);
        assert_eq!(summary.total_cost_usd, 0.5);
        assert_eq!(summary.call_count, 1);
    }

    #[test]
    fn test_budget_config_unlimited() {
        let config = CostBudgetConfig::unlimited();
        assert!(config.max_cost_usd.is_none());
        assert!(config.max_tokens.is_none());
        assert!(config.max_calls.is_none());
    }

    #[test]
    fn test_budget_config_with_cost_limit() {
        let config = CostBudgetConfig::with_cost_limit(5.0);
        assert_eq!(config.max_cost_usd, Some(5.0));
    }

    #[test]
    fn test_budget_config_with_token_limit() {
        let config = CostBudgetConfig::with_token_limit(100000);
        assert_eq!(config.max_tokens, Some(100000));
    }

    #[test]
    fn test_budget_config_with_call_limit() {
        let config = CostBudgetConfig::with_call_limit(50);
        assert_eq!(config.max_calls, Some(50));
    }

    #[test]
    fn test_budget_config_is_exceeded() {
        let config = CostBudgetConfig::with_cost_limit(5.0);
        let summary = CostSummary::from_records(&[
            UsageRecord::new("sess1", "gpt-4o", "openai", 1000, 500, 5.0),
        ]);
        assert!(config.is_exceeded(&summary));

        let summary2 = CostSummary::from_records(&[
            UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5),
        ]);
        assert!(!config.is_exceeded(&summary2));
    }

    #[test]
    fn test_budget_status_unlimited() {
        let status = BudgetStatus::unlimited(5);
        assert!(!status.budget_exceeded);
        assert!(status.is_allowed());
        assert!(status.remaining_usd.is_infinite());
    }

    #[test]
    fn test_model_pricing_entry_compute_cost() {
        let entry = ModelPricingEntry::new("gpt-4o", "openai", 2.50, 10.00);
        let cost = entry.compute_cost(1000, 500);
        let expected = 1.0 * 2.50 + 0.5 * 10.00;
        assert!((cost - expected).abs() < 0.01);
    }

    #[test]
    fn test_model_pricing_catalog_known_models() {
        let catalog = ModelPricingCatalog::with_known_models();
        assert!(catalog.model_count() > 0);

        // Check exact match
        let gpt4o = catalog.lookup("gpt-4o");
        assert_eq!(gpt4o.model_name, "gpt-4o");
        assert_eq!(gpt4o.provider, "openai");

        // Check partial match (model version suffix)
        let gpt4o_versioned = catalog.lookup("gpt-4o-2024-05-13");
        assert_eq!(gpt4o_versioned.model_name, "gpt-4o");

        // Check unknown model fallback
        let unknown = catalog.lookup("some-unknown-model");
        assert_eq!(unknown.model_name, "unknown");
    }

    #[test]
    fn test_model_pricing_catalog_compute_cost() {
        let catalog = ModelPricingCatalog::with_known_models();

        // Known model
        let cost = catalog.compute_cost("gpt-4o", 1000, 500);
        let expected = 1.0 * 2.50 + 0.5 * 10.00;
        assert!((cost - expected).abs() < 0.01);

        // Unknown model (uses default)
        let unknown_cost = catalog.compute_cost("unknown-model", 1000, 500);
        let default_expected = 1.0 * 0.03 + 0.5 * 0.06;
        assert!((unknown_cost - default_expected).abs() < 0.01);
    }

    #[test]
    fn test_model_pricing_catalog_free_models() {
        let catalog = ModelPricingCatalog::with_known_models();
        let llama = catalog.lookup("llama3");
        assert_eq!(llama.prompt_per_1k_usd, 0.0);
        assert_eq!(llama.completion_per_1k_usd, 0.0);

        let cost = catalog.compute_cost("llama3", 1000, 500);
        assert_eq!(cost, 0.0);
    }

    #[tokio::test]
    async fn test_in_memory_cost_tracker_record_and_query() {
        let tracker = InMemoryCostTracker::new();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "claude-sonnet-4", "anthropic", 200, 100, 1.5)).await.unwrap();

        let session_cost = tracker.session_cost("sess1").await.unwrap();
        assert_eq!(session_cost.call_count, 2);
        assert!((session_cost.total_cost_usd - 2.0).abs() < 0.01);
        assert_eq!(session_cost.total_tokens, 450);

        let global_cost = tracker.global_cost().await.unwrap();
        assert_eq!(global_cost.call_count, 2);
    }

    #[tokio::test]
    async fn test_in_memory_cost_tracker_budget_check() {
        let budget = CostBudgetConfig::with_cost_limit(1.0);
        let tracker = InMemoryCostTracker::with_budget(budget);

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5)).await.unwrap();

        let status = tracker.check_budget("sess1").await.unwrap();
        assert!(!status.budget_exceeded);
        assert!(status.is_allowed());

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.6)).await.unwrap();

        let status = tracker.check_budget("sess1").await.unwrap();
        assert!(status.budget_exceeded);
        assert!(!status.is_allowed());
    }

    #[tokio::test]
    async fn test_in_memory_cost_tracker_per_model_breakdown() {
        let tracker = InMemoryCostTracker::new();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "claude-sonnet-4", "anthropic", 200, 100, 1.5)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 300, 150, 1.0)).await.unwrap();

        let by_model = tracker.cost_by_model("sess1").await.unwrap();
        assert_eq!(by_model.len(), 2);

        let gpt4o = by_model.get("gpt-4o").unwrap();
        assert_eq!(gpt4o.call_count, 2);
        assert!((gpt4o.total_cost_usd - 1.5).abs() < 0.01);

        let claude = by_model.get("claude-sonnet-4").unwrap();
        assert_eq!(claude.call_count, 1);
        assert!((claude.total_cost_usd - 1.5).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_in_memory_cost_tracker_clear() {
        let tracker = InMemoryCostTracker::new();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess2", "gpt-4o", "openai", 100, 50, 0.5)).await.unwrap();

        tracker.clear_session("sess1").await.unwrap();
        let sess1 = tracker.session_cost("sess1").await.unwrap();
        assert_eq!(sess1.call_count, 0);

        let global = tracker.global_cost().await.unwrap();
        assert_eq!(global.call_count, 1); // sess2 still exists

        tracker.clear_all().await.unwrap();
        let global2 = tracker.global_cost().await.unwrap();
        assert_eq!(global2.call_count, 0);
    }

    #[tokio::test]
    async fn test_in_memory_cost_tracker_session_records() {
        let tracker = InMemoryCostTracker::new();

        tracker.record_usage(UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 0.5)).await.unwrap();
        tracker.record_usage(UsageRecord::new("sess1", "claude-sonnet-4", "anthropic", 200, 100, 1.5)).await.unwrap();

        let records = tracker.session_records("sess1").await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].model, "gpt-4o");
        assert_eq!(records[1].model, "claude-sonnet-4");
    }

    #[test]
    fn test_model_pricing_catalog_claude_pricing() {
        let catalog = ModelPricingCatalog::with_known_models();

        // Claude Opus 4: $15/1K prompt, $75/1K completion
        let opus_cost = catalog.compute_cost("claude-opus-4", 1000, 1000);
        let expected = 15.0 + 75.0;
        assert!((opus_cost - expected).abs() < 0.01);

        // Claude Sonnet 4: $3/1K prompt, $15/1K completion
        let sonnet_cost = catalog.compute_cost("claude-sonnet-4", 1000, 1000);
        let expected = 3.0 + 15.0;
        assert!((sonnet_cost - expected).abs() < 0.01);

        // Claude Haiku 4: $0.80/1K prompt, $4/1K completion
        let haiku_cost = catalog.compute_cost("claude-haiku-4", 1000, 1000);
        let expected = 0.80 + 4.0;
        assert!((haiku_cost - expected).abs() < 0.01);
    }

    #[test]
    fn test_budget_config_compute_status() {
        let config = CostBudgetConfig::with_cost_limit(10.0);
        let summary = CostSummary::from_records(&[
            UsageRecord::new("sess1", "gpt-4o", "openai", 100, 50, 3.0),
        ]);
        let status = config.compute_status(&summary);
        assert!(!status.budget_exceeded);
        assert!((status.remaining_usd - 7.0).abs() < 0.01);
        assert_eq!(status.budget_limit_usd, Some(10.0));
        assert_eq!(status.calls_made, 1);
    }
}
