//! Memory reflection — post-session reflection and episodic memory generation.
//!
//! At the end of a session (or when explicitly triggered), the MemoryReflection
//! system collects the session's STM entries, sends them to an LLM for
//! reflective analysis, and stores the resulting "episodic memory" in LTM.
//!
//! This creates a **STM ↔ LTM closed loop**:
//! 1. STM entries are evicted → LTM stores them (existing flow)
//! 2. At session end → LLM reflects on STM → generates EpisodicMemory → stored in LTM (NEW)
//! 3. On new turn → LTM recalls relevant memories → injected into STM context (NEW, see inject_ltm_context)
//!
//! ## EpisodicMemory
//!
//! An EpisodicMemory is a compressed, reflective summary of a session's
//! key insights, decisions, and outcomes. It's stored in LTM with metadata
//! that distinguishes it from raw conversation entries:
//! - `type: "episodic"` — marks it as a reflection-derived entry
//! - `session_id` — links back to the originating session
//! - `reflection` — the LLM-generated reflective summary
//!
//! ## When Reflection Happens
//!
//! Reflection is triggered:
//! - Automatically at session end (if MemoryReflectionConfig.auto_reflect is true)
//! - Manually via `MemoryManager.reflect()` or `AppSession.reflect_memory()`
//! - Only when an LLM provider is available for the reflection prompt

use std::sync::Arc;

use oneai_core::{Conversation, FactType, InferenceRequest, MemoryEntry, MemoryFact, Message};
use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;

// ─── MemoryReflectionConfig ──────────────────────────────────────────

/// Configuration for the memory reflection system.
#[derive(Debug, Clone)]
pub struct MemoryReflectionConfig {
    /// Whether to automatically trigger reflection at session end.
    pub auto_reflect: bool,

    /// Maximum tokens for the reflection prompt (budget for LLM call).
    pub max_reflection_tokens: u32,

    /// Temperature for the reflection LLM call (lower = more focused).
    pub reflection_temperature: f32,

    /// Whether to include the original conversation entries alongside the reflection.
    /// If true, the raw entries are also stored in LTM (they may already be there
    /// from eviction, so this doubles them with the episodic marker).
    pub include_original_entries: bool,

    /// Mid-session reflection trigger (§12.3, Generative-Agents-style):
    /// when the cumulative importance of newly-archived facts since the last
    /// reflection reaches this threshold AND `trigger_interval_turns` have
    /// elapsed, reflect mid-session rather than only at session end. `0.0`
    /// disables mid-session reflection (session-end-only, the legacy
    /// behavior). Defaults to `150.0` (the Generative Agents importance-sum
    /// threshold, scaled to OneAI's [0,1] importance × accumulated count).
    pub reflectance_threshold: f32,

    /// Minimum turns between two mid-session reflections (avoids reflecting
    /// every turn once the threshold is met). Defaults to `10`. Only consulted
    /// when `reflectance_threshold > 0.0`.
    pub trigger_interval_turns: u32,
}

impl Default for MemoryReflectionConfig {
    fn default() -> Self {
        Self {
            auto_reflect: true,
            max_reflection_tokens: 512,
            reflection_temperature: 0.0,
            include_original_entries: false,
            reflectance_threshold: 150.0,
            trigger_interval_turns: 10,
        }
    }
}

// ─── EpisodicMemory ──────────────────────────────────────────────

/// A reflective episodic memory — a compressed summary of a session's insights.
///
/// This is stored in LTM with metadata that distinguishes it from raw
/// conversation entries, enabling recall strategies to prioritize
/// reflective insights over raw conversation data.
///
/// The episodic memory contains:
/// - The LLM-generated reflection (key insights, decisions, outcomes)
/// - Metadata linking it back to the originating session
/// - Optional embedding for semantic recall
#[derive(Debug, Clone)]
pub struct EpisodicMemory {
    /// Unique ID for this episodic memory.
    pub id: String,

    /// The session ID that generated this reflection.
    pub session_id: String,

    /// The LLM-generated reflective summary.
    pub reflection: String,

    /// Key insights extracted from the session (by the LLM).
    pub key_insights: Vec<String>,

    /// Decisions made during the session (by the LLM analysis).
    pub decisions: Vec<String>,

    /// Outcome summary (success/failure/partial and what was accomplished).
    pub outcome: String,

    /// Optional embedding vector for semantic retrieval.
    pub embedding: Option<Vec<f32>>,
}

impl EpisodicMemory {
    /// Convert this episodic memory to a MemoryEntry for storage in LTM.
    ///
    /// The resulting MemoryEntry has metadata that marks it as an episodic
    /// (reflection-derived) entry, distinguishing it from raw conversation data.
    pub fn to_memory_entry(&self) -> MemoryEntry {
        // Build the content string: reflection + insights + decisions
        let mut content = self.reflection.clone();
        if !self.key_insights.is_empty() {
            content.push_str("\n\nKey Insights:\n");
            for insight in &self.key_insights {
                content.push_str(&format!("- {}\n", insight));
            }
        }
        if !self.decisions.is_empty() {
            content.push_str("\nDecisions:\n");
            for decision in &self.decisions {
                content.push_str(&format!("- {}\n", decision));
            }
        }
        if !self.outcome.is_empty() {
            content.push_str(&format!("\nOutcome: {}", self.outcome));
        }

        MemoryEntry {
            id: self.id.clone(),
            content,
            timestamp: chrono::Utc::now(),
            embedding: self.embedding.clone(),
            metadata: std::collections::HashMap::from([
                ("type".to_string(), "episodic".to_string()),
                ("session_id".to_string(), self.session_id.clone()),
                ("reflection".to_string(), self.reflection.clone()),
                ("outcome".to_string(), self.outcome.clone()),
            ]),
        }
    }

    /// Convert this episodic memory into a canonical `MemoryFact` for the
    /// archival tier (the "提炼型 episodic 中间层", M5).
    ///
    /// The fact is richer than a plain atomic fact (it carries the reflection
    /// summary plus key insights/decisions in `content`) yet far more compact
    /// than raw transcript — exactly the middle档 between "fact 太干" and
    /// "原文太长". High `importance` (0.8) so the three-factor recall surfaces
    /// it ahead of incidental facts. `subject` = originating session,
    /// `predicate` = "reflection".
    pub fn to_fact(&self) -> MemoryFact {
        let mut content = self.reflection.clone();
        if !self.key_insights.is_empty() {
            content.push_str("\n\nKey Insights:\n");
            for insight in &self.key_insights {
                content.push_str(&format!("- {}\n", insight));
            }
        }
        if !self.decisions.is_empty() {
            content.push_str("\nDecisions:\n");
            for decision in &self.decisions {
                content.push_str(&format!("- {}\n", decision));
            }
        }
        if !self.outcome.is_empty() {
            content.push_str(&format!("\nOutcome: {}", self.outcome));
        }
        MemoryFact {
            id: self.id.clone(),
            user_id: String::new(),
            session_id: self.session_id.clone(),
            fact_type: FactType::new("episodic"),
            subject: format!("session.{}", self.session_id),
            predicate: "reflection".to_string(),
            content,
            embedding: self.embedding.clone(),
            metadata: std::collections::HashMap::from([
                ("outcome".to_string(), self.outcome.clone()),
            ]),
            importance: 0.8,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
            superseded: false,
            superseded_at: None,
            pinned: false,
        }
    }
}

// ─── MemoryReflection ──────────────────────────────────────────────

/// Memory reflection engine — uses an LLM to reflect on session memory.
///
/// Collects STM entries, sends them to an LLM for reflective analysis,
/// and stores the resulting EpisodicMemory in LTM.
///
/// This is inspired by how human memory works: after an experience,
/// the brain consolidates and reflects on what happened, extracting
/// key patterns and insights for future use. OneAI mirrors this with
/// LLM-powered reflection that distills raw conversation data into
/// actionable episodic memories.
pub struct MemoryReflection {
    /// LLM provider for generating reflections.
    summarizer: Arc<dyn LlmProvider>,
    /// Configuration.
    config: MemoryReflectionConfig,
}

impl MemoryReflection {
    /// Create a new MemoryReflection engine with an LLM provider.
    pub fn new(summarizer: Arc<dyn LlmProvider>) -> Self {
        Self {
            summarizer,
            config: MemoryReflectionConfig::default(),
        }
    }

    /// Create with custom configuration.
    pub fn with_config(
        summarizer: Arc<dyn LlmProvider>,
        config: MemoryReflectionConfig,
    ) -> Self {
        Self {
            summarizer,
            config,
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &MemoryReflectionConfig {
        &self.config
    }

    /// §12.3: whether a mid-session reflection should fire now, based on the
    /// cumulative importance of facts archived since the last reflection and
    /// the turns elapsed since then. Mirrors Generative Agents' importance-sum
    /// threshold gating (with a turn-interval floor to avoid every-turn
    /// reflection). Mid-session reflection is disabled when
    /// `reflectance_threshold <= 0.0`.
    pub fn should_reflect(&self, accumulated_importance: f32, turns_since_last: u32) -> bool {
        if self.config.reflectance_threshold <= 0.0 {
            return false;
        }
        accumulated_importance >= self.config.reflectance_threshold
            && turns_since_last >= self.config.trigger_interval_turns
    }

    /// Reflect on a session's memory entries and generate an episodic memory.
    ///
    /// This method:
    /// 1. Collects the session's STM entries (conversation history)
    /// 2. Builds a reflection prompt asking the LLM to analyze the session
    /// 3. Calls the LLM to generate a reflective summary
    /// 4. Parses the response into structured insights/decisions/outcome
    /// 5. Creates an EpisodicMemory and converts it to a MemoryEntry
    ///
    /// Returns the EpisodicMemory (caller stores it in LTM via MemoryManager).
    pub async fn reflect(
        &self,
        session_id: &str,
        stm_entries: &[MemoryEntry],
    ) -> Result<EpisodicMemory> {
        self.reflect_with_prior(session_id, stm_entries, None).await
    }

    /// Reflect with an optional summary of prior episodic facts (§12.3
    /// recursive-reflection雏形): when the manager has accumulated earlier
    /// episodic memories in the archival tier, their distilled content is
    /// fed in so the new reflection can build on (and reference) prior
    /// insights instead of treating each reflection in isolation. This is
    /// the seed of Generative-Agents-style recursive reflection trees.
    pub async fn reflect_with_prior(
        &self,
        session_id: &str,
        stm_entries: &[MemoryEntry],
        prior_episodic_summary: Option<&str>,
    ) -> Result<EpisodicMemory> {
        if stm_entries.is_empty() {
            return Ok(EpisodicMemory {
                id: format!("epi_{}", uuid::Uuid::new_v4()),
                session_id: session_id.to_string(),
                reflection: "Empty session — no reflection needed.".to_string(),
                key_insights: Vec::new(),
                decisions: Vec::new(),
                outcome: "empty".to_string(),
                embedding: None,
            });
        }

        // Build the text to reflect on
        let session_text = stm_entries.iter()
            .map(|entry| {
                let role = entry.metadata.get("role").map(|s| s.as_str()).unwrap_or("memory");
                format!("[{}]: {}", role, entry.content)
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Build the reflection prompt
        let mut reflection_prompt = "You are a memory reflection system. Analyze the conversation below \
            and extract: (1) Key Insights — the most important facts, patterns, and learnings, \
            (2) Decisions — the key choices made during the session, \
            (3) Outcome — a brief summary of whether the session succeeded, partially succeeded, \
            or failed, and what was accomplished. \
            Format your response as:\n\
            REFLECTION: [your reflective summary]\n\
            INSIGHTS: [comma-separated list]\n\
            DECISIONS: [comma-separated list]\n\
            OUTCOME: [success/partial/failure + brief description]".to_string();
        if let Some(prior) = prior_episodic_summary {
            if !prior.trim().is_empty() {
                reflection_prompt.push_str(&format!(
                    "\n\nYou have accumulated these earlier episodic reflections for this user. \
                    Build on them — reference and extend prior insights rather than restating \
                    them, and note how this session advances or revises them:\n{}", prior
                ));
            }
        }

        // Request reflection from the LLM
        let mut reflection_conv = Conversation::new();
        reflection_conv.add_message(Message::system(reflection_prompt));
        reflection_conv.add_message(Message::user(format!(
            "Reflect on this session (session_id: {}):\n\n{}", session_id, session_text
        )));

        let request = InferenceRequest {
            conversation: reflection_conv,
            tools: vec![],
            max_tokens: Some(self.config.max_reflection_tokens),
            temperature: Some(self.config.reflection_temperature),
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        };

        let response = self.summarizer.infer(request).await?;
        let reflection_text = response.message.text_content();

        // Parse the structured response
        let (reflection, key_insights, decisions, outcome) = parse_reflection_response(&reflection_text);

        Ok(EpisodicMemory {
            id: format!("epi_{}", uuid::Uuid::new_v4()),
            session_id: session_id.to_string(),
            reflection,
            key_insights,
            decisions,
            outcome,
            embedding: None, // Embeddings would be computed by the embedding service later
        })
    }
}

// ─── Parse reflection response ──────────────────────────────────────

/// Parse the structured reflection response from the LLM.
///
/// Expected format:
/// ```text
/// REFLECTION: [reflective summary]
/// INSIGHTS: [comma-separated list]
/// DECISIONS: [comma-separated list]
/// OUTCOME: [success/partial/failure + brief description]
/// ```
///
/// If parsing fails, falls back to treating the entire response as the reflection.
fn parse_reflection_response(text: &str) -> (String, Vec<String>, Vec<String>, String) {
    let mut reflection = String::new();
    let mut key_insights = Vec::new();
    let mut decisions = Vec::new();
    let mut outcome = String::new();

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("REFLECTION:") {
            reflection = line["REFLECTION:".len()..].trim().to_string();
        } else if line.starts_with("INSIGHTS:") {
            let insights_str = line["INSIGHTS:".len()..].trim();
            key_insights = insights_str.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if line.starts_with("DECISIONS:") {
            let decisions_str = line["DECISIONS:".len()..].trim();
            decisions = decisions_str.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if line.starts_with("OUTCOME:") {
            outcome = line["OUTCOME:".len()..].trim().to_string();
        }
    }

    // Fallback: if no structured fields were found, treat entire text as reflection
    if reflection.is_empty() && key_insights.is_empty() && decisions.is_empty() && outcome.is_empty() {
        reflection = text.to_string();
        outcome = "unknown".to_string();
    }

    (reflection, key_insights, decisions, outcome)
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use oneai_core::{InferenceRequest, InferenceResponse, TokenUsage, ModelCapability, ModelConfig};
    use oneai_core::traits::LlmProvider;
    use oneai_core::error::Result;
    use oneai_core::ProviderType;

    fn make_entry(id: &str, content: &str, role: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            content: content.to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([
                ("role".to_string(), role.to_string()),
            ]),
        }
    }

    /// Simple mock provider for reflection tests.
    struct MockReflectionProvider {
        response_text: String,
    }

    impl MockReflectionProvider {
        fn new(response_text: &str) -> Self {
            Self { response_text: response_text.to_string() }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockReflectionProvider {
        async fn infer(&self, _req: InferenceRequest) -> Result<InferenceResponse> {
            Ok(InferenceResponse {
                message: oneai_core::Message::assistant(self.response_text.clone()),
                usage: TokenUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
            ..Default::default()},
                model: "mock-reflection".to_string(),
                metadata: HashMap::new(),
            })
        }

        async fn infer_stream(
            &self,
            _req: InferenceRequest,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = oneai_core::InferenceStreamChunk> + Send>>> {
            Err(oneai_core::error::OneAIError::Provider("Streaming not supported in mock".to_string()))
        }

        fn capabilities(&self) -> ModelCapability {
            ModelCapability {
                supports_multimodal: false,
                supports_streaming: false,
                supports_tools: false,
                context_window_size: 4096,
                max_output_tokens: 512,
            }
        }

        fn config(&self) -> &ModelConfig {
            static CONFIG: std::sync::OnceLock<ModelConfig> = std::sync::OnceLock::new();
            CONFIG.get_or_init(|| ModelConfig {
                provider_type: ProviderType::Local,
                cloud_kind: None,
                api_key: None,
                base_url: None,
                port: None,
                model_name: Some("mock-reflection".to_string()),
                model_path: None,
                extra: HashMap::new(),
            })
        }
    }

    #[test]
    fn test_parse_reflection_response_structured() {
        let text = "REFLECTION: The session explored Rust programming concepts.\n\
            INSIGHTS: Rust has strong type safety, Ownership model prevents memory leaks\n\
            DECISIONS: Use Rust for the backend, Choose async runtime over sync\n\
            OUTCOME: success — completed all coding tasks";

        let (reflection, insights, decisions, outcome) = parse_reflection_response(text);
        assert_eq!(reflection, "The session explored Rust programming concepts.");
        assert_eq!(insights.len(), 2);
        assert_eq!(decisions.len(), 2);
        assert!(outcome.starts_with("success"));
    }

    #[test]
    fn test_parse_reflection_response_unstructured() {
        let text = "This was a great session. We learned about Rust and decided to use it for the backend.";
        let (reflection, _insights, _decisions, outcome) = parse_reflection_response(text);
        assert_eq!(reflection, text);
        assert_eq!(outcome, "unknown");
    }

    #[test]
    fn test_parse_reflection_response_partial() {
        let text = "REFLECTION: Some reflection here.\nOUTCOME: partial — some tasks completed";
        let (reflection, insights, _decisions, outcome) = parse_reflection_response(text);
        assert_eq!(reflection, "Some reflection here.");
        assert!(insights.is_empty());
        assert!(outcome.starts_with("partial"));
    }

    #[test]
    fn test_episodic_memory_to_entry() {
        let episodic = EpisodicMemory {
            id: "epi_123".to_string(),
            session_id: "sess_456".to_string(),
            reflection: "We explored Rust".to_string(),
            key_insights: vec!["Rust is fast".to_string(), "Ownership model".to_string()],
            decisions: vec!["Use Rust backend".to_string()],
            outcome: "success".to_string(),
            embedding: None,
        };

        let entry = episodic.to_memory_entry();
        assert_eq!(entry.id, "epi_123");
        assert_eq!(entry.metadata.get("type").unwrap(), "episodic");
        assert_eq!(entry.metadata.get("session_id").unwrap(), "sess_456");
        assert!(entry.content.contains("We explored Rust"));
        assert!(entry.content.contains("Key Insights:"));
        assert!(entry.content.contains("Rust is fast"));
    }

    #[test]
    fn test_episodic_memory_to_fact() {
        // M5: the episodic middle layer becomes a canonical archival fact.
        let episodic = EpisodicMemory {
            id: "epi_123".to_string(),
            session_id: "sess_456".to_string(),
            reflection: "We explored Rust".to_string(),
            key_insights: vec!["Rust is fast".to_string()],
            decisions: vec!["Use Rust backend".to_string()],
            outcome: "success".to_string(),
            embedding: None,
        };

        let fact = episodic.to_fact();
        assert_eq!(fact.id, "epi_123");
        assert_eq!(fact.fact_type.as_str(), "episodic");
        assert_eq!(fact.subject, "session.sess_456");
        assert_eq!(fact.predicate, "reflection");
        assert!(fact.content.contains("We explored Rust"));
        assert!(fact.content.contains("Use Rust backend"));
        // High salience so the three-factor recall surfaces it.
        assert!((fact.importance - 0.8).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_memory_reflection_with_mock() {
        let mock = Arc::new(MockReflectionProvider::new(
            "REFLECTION: Explored Rust concepts\n\
            INSIGHTS: Rust is memory-safe, Ownership prevents leaks\n\
            DECISIONS: Use Rust for backend\n\
            OUTCOME: success — all tasks completed"
        ));

        let reflection = MemoryReflection::new(mock);

        let entries = vec![
            make_entry("1", "What is Rust?", "user"),
            make_entry("2", "Rust is a programming language with ownership model", "assistant"),
        ];

        let result = reflection.reflect("sess_test", &entries).await.unwrap();
        assert_eq!(result.session_id, "sess_test");
        assert!(result.reflection.contains("Explored Rust concepts"));
        assert_eq!(result.key_insights.len(), 2);
        assert_eq!(result.decisions.len(), 1);
        assert!(result.outcome.starts_with("success"));
    }

    #[tokio::test]
    async fn test_memory_reflection_empty_session() {
        let mock = Arc::new(MockReflectionProvider::new("no reflection"));
        let reflection = MemoryReflection::new(mock);

        let result = reflection.reflect("sess_empty", &[]).await.unwrap();
        assert!(result.reflection.contains("Empty session"));
        assert!(result.key_insights.is_empty());
    }

    #[test]
    fn test_reflection_config_default() {
        let config = MemoryReflectionConfig::default();
        assert!(config.auto_reflect);
        assert_eq!(config.max_reflection_tokens, 512);
        assert_eq!(config.reflection_temperature, 0.0);
        assert!(!config.include_original_entries);
        // §12.3 defaults
        assert!((config.reflectance_threshold - 150.0).abs() < 1e-6);
        assert_eq!(config.trigger_interval_turns, 10);
    }

    // ─── §12.3: threshold-triggered reflection ──────────────────────────────

    #[test]
    fn should_reflect_threshold_and_interval_gating() {
        let mock = Arc::new(MockReflectionProvider::new("REFLECTION: x\nOUTCOME: success"));
        let r = MemoryReflection::new(mock);

        // Below threshold → no.
        assert!(!r.should_reflect(10.0, 20));
        // Threshold met but not enough turns → no.
        assert!(!r.should_reflect(200.0, 5));
        // Both met → yes.
        assert!(r.should_reflect(200.0, 10));
    }

    #[test]
    fn should_reflect_disabled_when_threshold_zero() {
        let mock = Arc::new(MockReflectionProvider::new("REFLECTION: x\nOUTCOME: success"));
        let mut r = MemoryReflection::new(mock);
        r.config.reflectance_threshold = 0.0;
        // Even huge accumulated importance doesn't trigger when disabled.
        assert!(!r.should_reflect(10000.0, 100));
    }

    #[tokio::test]
    async fn reflect_with_prior_includes_prior_summary() {
        // Smoke: reflect_with_prior with a non-empty prior summary still parses
        // the canned structured response (proves the augmented prompt path runs).
        let mock = Arc::new(MockReflectionProvider::new(
            "REFLECTION: Built on prior insight\nINSIGHTS: extended insight\nDECISIONS: refine approach\nOUTCOME: success"
        ));
        let r = MemoryReflection::new(mock);
        let entries = vec![make_entry("1", "work", "user"), make_entry("2", "done", "assistant")];
        let result = r.reflect_with_prior("s", &entries, Some("- prior episodic: decided Rust")).await.unwrap();
        assert!(result.reflection.contains("Built on prior insight"));
        assert_eq!(result.key_insights.len(), 1);
    }
}
