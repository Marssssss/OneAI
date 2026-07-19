//! FactExtractor — turn discarded conversation turns into durable atomic facts.
//!
//! On compression, the `ContextCompressor` produces a `discarded_messages`
//! segment (the older turns summarized away). `FactExtractor` runs an LLM over
//! that segment, guided by the active `MemoryProfile.extraction_schema`, to
//! extract atomic `MemoryFact`s. These are conflict-resolved into the archival
//! tier — closing the "压缩即丢失" gap: information compressed out of context
//! is not lost, it becomes searchable long-term memory.
//!
//! Output contract: the LLM is asked for a JSON array of
//! `{"fact_type","subject","predicate","content"}`. Parsing is tolerant (strips
//! code fences, extracts the first `[...]` span) and fails safe — a malformed
//! response yields zero facts rather than an error, so a bad extraction never
//! breaks the compression path.

use std::sync::Arc;

use oneai_core::{Conversation, FactType, InferenceRequest, MemoryFact, Message, Role};
use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;

/// LLM-backed extractor of atomic facts from a conversation segment.
pub struct FactExtractor {
    provider: Arc<dyn LlmProvider>,
    schema: Vec<FactType>,
}

/// A single fact as emitted by the LLM, before enrichment into `MemoryFact`.
#[derive(Debug, Clone, serde::Deserialize)]
struct RawFact {
    fact_type: String,
    subject: String,
    predicate: String,
    content: String,
    /// Optional salience override in [0.0, 1.0]; falls back to the per-type default.
    #[serde(default)]
    importance: Option<f32>,
}

impl FactExtractor {
    /// Create an extractor with the given LLM provider and domain schema.
    pub fn new(provider: Arc<dyn LlmProvider>, schema: Vec<FactType>) -> Self {
        Self { provider, schema }
    }

    /// Whether the extractor has a non-empty schema to guide extraction.
    pub fn has_schema(&self) -> bool {
        !self.schema.is_empty()
    }

    /// Extract atomic facts from a segment of conversation messages.
    ///
    /// `user_id` and `session_id` namespace the resulting facts (user-scope
    /// for cross-session habits, session-scope for episodic context). Returns
    /// an empty vec if the schema is empty, the segment is empty, or the LLM
    /// response can't be parsed (fail-safe).
    pub async fn extract(
        &self,
        messages: &[Message],
        user_id: &str,
        session_id: &str,
    ) -> Result<Vec<MemoryFact>> {
        if self.schema.is_empty() || messages.is_empty() {
            return Ok(Vec::new());
        }

        let segment = messages.iter()
            .map(|m| {
                let role = match m.role {
                    Role::System => "System",
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::Tool => "Tool",
                    _ => "User",
                };
                format!("[{}]: {}", role, m.text_content())
            })
            .collect::<Vec<_>>()
            .join("\n");

        if segment.trim().is_empty() {
            return Ok(Vec::new());
        }

        let schema_list = self.schema.iter()
            .map(|f| format!("  - {}", f.as_str()))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "You are a memory extractor. Read the conversation segment below and extract \
            durable atomic facts that would be useful to remember for future turns and \
            sessions. Only extract facts whose type is in this schema:\n{}\n\n\
            Output ONLY a JSON array (no prose, no code fences) of objects with fields \
            \"fact_type\" (one of the schema above), \"subject\" (what the fact is about, \
            e.g. \"user.package_manager\" or \"auth.module\"), \"predicate\" (the assertion, \
            e.g. \"prefers\", \"decided_to\", \"status_is\"), \"content\" (the value), and an \
            optional \"importance\" (float 0.0–1.0, how salient this fact is for future \
            recall; omit to use the per-type default). Decisions and key outcomes should \
            rate high; incidental observations low. If there are no extractable facts, \
            output [].\n\nConversation segment:\n{}",
            schema_list, segment
        );

        let mut conv = Conversation::new();
        conv.add_message(Message::system(prompt));
        let request = InferenceRequest {
            conversation: conv,
            tools: vec![],
            max_tokens: Some(512),
            temperature: Some(0.0),
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        };

        let response = self.provider.infer(request).await?;
        let text = response.message.text_content();
        Ok(parse_facts(&text, &self.schema, user_id, session_id))
    }
}

/// Parse the LLM's JSON-array response into enriched `MemoryFact`s.
///
/// Tolerant: strips ```json fences, finds the first `[...]` span, and ignores
/// any unparseable elements. Returns an empty vec on any failure.
fn parse_facts(text: &str, schema: &[FactType], user_id: &str, session_id: &str) -> Vec<MemoryFact> {
    let json_text = extract_json_array(text);
    let parsed: Vec<RawFact> = match serde_json::from_str(&json_text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let now = chrono::Utc::now();
    parsed.into_iter()
        .filter(|r| {
            // Keep only facts whose type is in the schema (defense against drift).
            schema.iter().any(|s| s.as_str() == r.fact_type.as_str())
        })
        .map(|r| {
            let fact_type = FactType::new(r.fact_type.clone());
            let importance = r.importance
                .filter(|v| (0.0..=1.0).contains(v))
                .unwrap_or_else(|| default_importance_for_type(fact_type.as_str()));
            MemoryFact {
                id: format!("fact_{}", uuid::Uuid::new_v4()),
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
                fact_type,
                subject: r.subject,
                predicate: r.predicate,
                content: r.content,
                embedding: None,
                metadata: std::collections::HashMap::new(),
                importance,
                created_at: now,
                updated_at: now,
                version: 1,
                superseded: false,
                superseded_at: None,
                pinned: false,
            }
        })
        .collect()
}

/// Per-category default importance (salience) for extracted facts.
///
/// Decisions and episodics are the highest-salience memories (they shape
/// future reasoning); open tasks and user tooling prefs are medium; anything
/// else defaults to the baseline. The agent can revise these via the
/// self-managed memory tools.
fn default_importance_for_type(fact_type: &str) -> f32 {
    match fact_type {
        "decision" | "episodic" => 0.85,
        "critical_file" => 0.75,
        "open_task" | "user_tooling_pref" | "user_interest" => 0.65,
        _ => 0.5,
    }
}

/// Extract the first `[...]` JSON-array span from a possibly-noisy LLM response.
fn extract_json_array(text: &str) -> String {
    let trimmed = text.trim();
    // Strip ```json ... ``` fences if present.
    let stripped = trimmed.trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    if stripped.starts_with('[') {
        return stripped.to_string();
    }
    // Fall back to the first `[` .. matching `]` span.
    if let (Some(start), Some(end)) = (stripped.find('['), stripped.rfind(']')) {
        if end > start {
            return stripped[start..=end].to_string();
        }
    }
    stripped.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::{
        InferenceResponse, ModelCapability, ModelConfig, ProviderType, TokenUsage,
    };
    use std::collections::HashMap;

    /// Mock provider that returns a canned response.
    struct MockExtractorProvider { response: String }
    impl MockExtractorProvider { fn new(r: impl Into<String>) -> Self { Self { response: r.into() } } }

    #[async_trait::async_trait]
    impl LlmProvider for MockExtractorProvider {
        async fn infer(&self, _req: InferenceRequest) -> Result<InferenceResponse> {
            Ok(InferenceResponse {
                message: Message::assistant(self.response.clone()),
                usage: TokenUsage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0, ..Default::default()},
                model: "mock-extractor".to_string(),
                metadata: HashMap::new(),
            })
        }
        async fn infer_stream(
            &self, _req: InferenceRequest,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = oneai_core::InferenceStreamChunk> + Send>>> {
            Err(oneai_core::error::OneAIError::Provider("no stream".into()))
        }
        fn capabilities(&self) -> ModelCapability {
            ModelCapability { supports_multimodal: false, supports_streaming: false, supports_tools: false, context_window_size: 4096, max_output_tokens: 512 }
        }
        fn config(&self) -> &ModelConfig {
            static CONFIG: std::sync::OnceLock<ModelConfig> = std::sync::OnceLock::new();
            CONFIG.get_or_init(|| ModelConfig { provider_type: ProviderType::Local, cloud_kind: None, api_key: None, base_url: None, port: None, model_name: Some("mock-extractor".into()), model_path: None, ..Default::default() })
        }
    }

    fn coding_schema() -> Vec<FactType> {
        vec![
            FactType::new("user_tooling_pref"),
            FactType::new("decision"),
            FactType::new("open_task"),
        ]
    }

    #[tokio::test]
    async fn extract_parses_json_array() {
        let resp = r#"```json
        [
          {"fact_type":"user_tooling_pref","subject":"user.package_manager","predicate":"prefers","content":"pnpm"},
          {"fact_type":"decision","subject":"auth.module","predicate":"decided_to","content":"use JWT"}
        ]
        ```"#;
        let ext = FactExtractor::new(Arc::new(MockExtractorProvider::new(resp)), coding_schema());
        let msgs = vec![Message::user("I use pnpm. Let's use JWT for auth.")];
        let facts = ext.extract(&msgs, "alice", "s1").await.unwrap();
        assert_eq!(facts.len(), 2);
        assert!(facts.iter().any(|f| f.subject == "user.package_manager" && f.content == "pnpm"));
        assert!(facts.iter().any(|f| f.subject == "auth.module" && f.content == "use JWT"));
        assert_eq!(facts[0].user_id, "alice");
    }

    #[tokio::test]
    async fn extract_filters_unknown_fact_types() {
        // LLM emits a fact_type not in schema → dropped.
        let resp = r#"[{"fact_type":"bogus","subject":"x","predicate":"y","content":"z"}]"#;
        let ext = FactExtractor::new(Arc::new(MockExtractorProvider::new(resp)), coding_schema());
        let facts = ext.extract(&[Message::user("hi")], "alice", "s1").await.unwrap();
        assert!(facts.is_empty());
    }

    #[tokio::test]
    async fn extract_fails_safe_on_malformed_json() {
        let ext = FactExtractor::new(Arc::new(MockExtractorProvider::new("not json at all")), coding_schema());
        let facts = ext.extract(&[Message::user("hi")], "alice", "s1").await.unwrap();
        assert!(facts.is_empty()); // no crash, no facts
    }

    #[tokio::test]
    async fn extract_empty_when_no_schema() {
        let ext = FactExtractor::new(Arc::new(MockExtractorProvider::new("[]")), Vec::new());
        let facts = ext.extract(&[Message::user("hi")], "alice", "s1").await.unwrap();
        assert!(facts.is_empty());
    }

    #[test]
    fn parse_handles_bare_array() {
        let facts = parse_facts(
            r#"[{"fact_type":"decision","subject":"a","predicate":"b","content":"c"}]"#,
            &coding_schema(),
            "u", "s",
        );
        assert_eq!(facts.len(), 1);
    }
}
