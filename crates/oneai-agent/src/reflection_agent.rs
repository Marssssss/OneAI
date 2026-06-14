//! Reflection Agent — result verification paradigm.
//!
//! The ReflectionAgent evaluates the results from a Plan/ReAct pipeline
//! and determines whether they are accurate and complete.
//! If issues are found, it suggests corrections or requests a retry.

use std::collections::HashMap;
use std::sync::Arc;

use oneai_core::{
    Conversation, InferenceRequest, Message,
};
use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;

/// Result of a Reflection agent evaluation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReflectionResult {
    /// Whether the result passes verification.
    pub passed: bool,

    /// Confidence score (0.0 to 1.0).
    pub confidence: f32,

    /// Issues found during reflection (if any).
    #[serde(default)]
    pub issues: Vec<String>,

    /// Suggestions for improvement (if any).
    #[serde(default)]
    pub suggestions: Vec<String>,

    /// The conversation after reflection.
    pub conversation: Conversation,

    /// The raw model response.
    pub raw_response: String,
}

/// Configuration for a Reflection agent.
#[derive(Debug, Clone)]
pub struct ReflectionConfig {
    /// System prompt for the reflection agent.
    pub system_prompt: String,

    /// Temperature for reflection (lower = more analytical).
    pub temperature: Option<f32>,

    /// Maximum tokens for reflection response.
    pub max_tokens: Option<u32>,

    /// Maximum number of retry attempts if reflection fails.
    pub max_retries: usize,
}

impl Default for ReflectionConfig {
    fn default() -> Self {
        Self {
            system_prompt: REFLECTION_SYSTEM_PROMPT.to_string(),
            temperature: Some(0.0),
            max_tokens: Some(1024),
            max_retries: 2,
        }
    }
}

/// Default system prompt for the reflection agent.
const REFLECTION_SYSTEM_PROMPT: &str = "\
You are a result verification assistant. Your job is to evaluate whether a given result \
accurately and completely addresses the original task.

Evaluate the result on these criteria:
1. ACCURACY: Is the factual content correct?
2. COMPLETENESS: Does it address all aspects of the original task?
3. RELEVANCE: Is the result relevant to what was asked?

Output your evaluation as a JSON object with this exact format:
```json
{
  \"passed\": true/false,
  \"confidence\": 0.0-1.0,
  \"issues\": [\"list of issues found, if any\"],
  \"suggestions\": [\"list of suggestions for improvement, if any\"]
}
```

Be thorough but fair. A result should pass if it reasonably addresses the task, \
even if it's not perfect. Only flag issues that significantly affect the answer's usefulness.";

/// Reflection Agent — verifies and improves results.
pub struct ReflectionAgent {
    /// The LLM provider.
    provider: Arc<dyn LlmProvider>,

    /// Configuration.
    config: ReflectionConfig,
}

impl ReflectionAgent {
    /// Create a new Reflection agent.
    pub fn new(provider: Arc<dyn LlmProvider>, config: ReflectionConfig) -> Self {
        Self { provider, config }
    }

    /// Create with default configuration.
    pub fn with_defaults(provider: Arc<dyn LlmProvider>) -> Self {
        Self::new(provider, ReflectionConfig::default())
    }

    /// Reflect on a result — evaluate whether it adequately addresses the task.
    ///
    /// Takes the original task, the result, and optionally the conversation history.
    pub async fn reflect(
        &self,
        task: &str,
        result: &str,
        conversation: Option<&Conversation>,
    ) -> Result<ReflectionResult> {
        let mut conv = if let Some(c) = conversation {
            c.clone()
        } else {
            Conversation::new()
        };

        conv.add_message(Message::system(self.config.system_prompt.clone()));
        conv.add_message(Message::user(format!(
            "Original task:\n{}\n\nResult to evaluate:\n{}\n\nPlease evaluate this result.",
            task, result
        )));

        let request = InferenceRequest {
            conversation: conv.clone(),
            tools: vec![], // Reflection doesn't need tools
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let response = self.provider.infer(request).await?;
        let raw_response = response.message.text_content();
        conv.add_message(response.message.clone());

        let reflection = parse_reflection_result(&raw_response);

        Ok(ReflectionResult {
            passed: reflection.passed,
            confidence: reflection.confidence,
            issues: reflection.issues,
            suggestions: reflection.suggestions,
            conversation: conv,
            raw_response,
        })
    }

    /// Reflect and retry — if the result doesn't pass, generate a corrected version.
    ///
    /// This implements the self-correction loop:
    /// 1. Reflect on the result
    /// 2. If it doesn't pass, feed the issues back to the model
    /// 3. Get a corrected result
    /// 4. Reflect again
    /// 5. Repeat up to max_retries times
    pub async fn reflect_and_retry(
        &self,
        task: &str,
        initial_result: &str,
        react_agent: &crate::ReActAgent,
        conversation: Option<&Conversation>,
    ) -> Result<(String, ReflectionResult)> {
        let mut current_result = initial_result.to_string();
        let mut retries = 0;

        while retries < self.config.max_retries {
            let reflection = self.reflect(task, &current_result, conversation).await?;

            if reflection.passed {
                return Ok((current_result, reflection));
            }

            retries += 1;
            tracing::warn!(
                "Reflection failed (attempt {}): issues = {:?}",
                retries, reflection.issues
            );

            // If we have suggestions, incorporate them into a retry prompt
            let retry_prompt = if reflection.suggestions.is_empty() {
                format!(
                    "The previous result had these issues: {:?}. Please try again with a corrected approach.\n\nOriginal task: {}",
                    reflection.issues, task
                )
            } else {
                format!(
                    "The previous result had these issues: {:?}\nSuggestions: {:?}\nPlease try again.\n\nOriginal task: {}",
                    reflection.issues, reflection.suggestions, task
                )
            };

            // Run ReAct agent again with the retry prompt
            let mut retry_conv = Conversation::new();
            retry_conv.add_message(Message::user(retry_prompt));

            let retry_result = react_agent.run(retry_conv).await?;
            current_result = retry_result.final_message.text_content();
        }

        // Final reflection even after max retries
        let final_reflection = self.reflect(task, &current_result, conversation).await?;
        Ok((current_result, final_reflection))
    }
}

/// Internal struct for parsed reflection JSON.
struct ParsedReflection {
    passed: bool,
    confidence: f32,
    issues: Vec<String>,
    suggestions: Vec<String>,
}

/// Parse reflection result from the model's response.
fn parse_reflection_result(raw: &str) -> ParsedReflection {
    // Try direct JSON parse
    if let Ok(result) = serde_json::from_str::<ReflectionResult>(raw) {
        return ParsedReflection {
            passed: result.passed,
            confidence: result.confidence,
            issues: result.issues,
            suggestions: result.suggestions,
        };
    }

    // Try extracting JSON from the response
    let start = raw.find('{');
    let end = raw.rfind('}');

    if let (Some(s), Some(e)) = (start, end) {
        let json_fragment = &raw[s..e + 1];
        if let Ok(result) = serde_json::from_str::<ReflectionResult>(json_fragment) {
            return ParsedReflection {
                passed: result.passed,
                confidence: result.confidence,
                issues: result.issues,
                suggestions: result.suggestions,
            };
        }
    }

    // Fallback: try to detect pass/fail from text
    let passed = raw.to_lowercase().contains("passed")
        || raw.to_lowercase().contains("accurate")
        || raw.to_lowercase().contains("correct")
        && !raw.to_lowercase().contains("not accurate")
        && !raw.to_lowercase().contains("incorrect");

    ParsedReflection {
        passed,
        confidence: if passed { 0.6 } else { 0.3 },
        issues: if passed { vec![] } else { vec!["Could not parse reflection result".to_string()] },
        suggestions: vec!["Review the result manually".to_string()],
    }
}