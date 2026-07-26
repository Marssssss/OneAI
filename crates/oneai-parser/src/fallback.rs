//! Fallback self-correction loop — Layer 3 of the 3-layer parsing defense.
//!
//! If Layers 1 and 2 fail, this layer catches the parser exception,
//! generates an implicit error message, and re-feeds it to the model
//! for self-correction. Up to `max_retries` attempts are made.

use oneai_core::{ContentBlock, InferenceRequest, Message, OneAIError, ParsedOutput, ParsingLayer};
use oneai_core::error::ParserError;
use oneai_core::traits::LlmProvider;

use crate::fuzzy::FuzzyJsonRepair;

/// The fallback self-correction loop.
pub struct FallbackLoop {
    /// Maximum number of self-correction retries.
    max_retries: usize,
}

impl FallbackLoop {
    /// Create a new fallback loop with default max retries (3).
    pub fn new() -> Self {
        Self { max_retries: 3 }
    }

    /// Create a fallback loop with a custom max retries.
    pub fn with_max_retries(max_retries: usize) -> Self {
        Self { max_retries }
    }

    /// Attempt self-correction by re-feeding the error to the model.
    ///
    /// Takes the raw failed output, the original request, and an LLM provider.
    /// Generates an error message instructing the model to fix its output format,
    /// then re-requests inference. Up to `max_retries` attempts are made; each
    /// attempt's corrected output is validated via Layer 2 fuzzy repair, and
    /// on success the repaired text is returned. Previously the loop body
    /// hardcoded a single attempt regardless of `max_retries` — the configured
    /// retry count was a lie.
    pub async fn self_correct(
        &self,
        provider: &dyn LlmProvider,
        original_request: &InferenceRequest,
        failed_output: &str,
        error_description: &str,
    ) -> std::result::Result<ParsedOutput, OneAIError> {
        let fuzzy = FuzzyJsonRepair::new();
        let mut last_error = error_description.to_string();
        let mut last_output = failed_output.to_string();

        for attempt in 1..=self.max_retries {
            // Build a corrected request: replay the failed output, then a
            // system message describing the format error and asking for a
            // clean re-emit (deterministic temperature for correction).
            let mut corrected_conversation = original_request.conversation.clone();
            corrected_conversation.add_message(Message::assistant(last_output.clone()));
            corrected_conversation.add_message(Message::system(format!(
                "You just output content with a format error: {last_error}\n\
                 The raw output was: {}\n\
                 Please严格按照正确的格式重新输出，不要包含任何多余的文字。",
                &last_output[..last_output.len().min(500)]
            )));

            let corrected_request = InferenceRequest {
                conversation: corrected_conversation,
                tools: original_request.tools.clone(),
                max_tokens: original_request.max_tokens,
                temperature: Some(0.0),
                top_p: original_request.top_p,
                stop_sequences: original_request.stop_sequences.clone(),
                constrained_output: original_request.constrained_output.clone(),
                thinking_budget: None,
                metadata: original_request.metadata.clone(),
            };

            let response = provider.infer(corrected_request).await.map_err(|e| {
                OneAIError::Parser(ParserError::FallbackExhausted {
                    retries: attempt,
                    reason: format!(
                        "provider error during self-correction attempt {attempt}: {e}"
                    ),
                })
            })?;

            let text = response.message.text_content();

            // Validate the corrected output via Layer 2 fuzzy repair.
            if fuzzy.repair_and_parse(&text).is_ok() {
                return Ok(ParsedOutput {
                    content: vec![ContentBlock::Text { text }],
                    parsing_layer: ParsingLayer::FallbackSelfCorrection,
                    fallback_retries: attempt,
                });
            }

            // Still malformed — feed this attempt's output + error forward
            // into the next retry so the model sees its continued failure.
            last_output = text;
            last_error = format!("attempt {attempt} still failed the format check");
        }

        Err(OneAIError::Parser(ParserError::FallbackExhausted {
            retries: self.max_retries,
            reason: format!(
                "Model failed to self-correct after {} retries",
                self.max_retries
            ),
        }))
    }

    /// Get the max retries setting.
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }
}

impl Default for FallbackLoop {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::LlmProvider;
    use oneai_core::{
        Conversation, InferenceResponse, ModelCapability, ModelConfig, ProviderType, TokenUsage,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A minimal in-test LLM provider: returns canned assistant text on each
    /// `infer` call and counts how many times it was invoked. `infer_stream`
    /// is unused by `self_correct` and returns an error.
    struct StubProvider {
        replies: Mutex<Vec<String>>,
        infer_calls: AtomicUsize,
    }

    impl StubProvider {
        fn new(replies: Vec<String>) -> Self {
            Self {
                replies: Mutex::new(replies),
                infer_calls: AtomicUsize::new(0),
            }
        }

        fn infer_calls(&self) -> usize {
            self.infer_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for StubProvider {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> std::result::Result<InferenceResponse, OneAIError> {
            self.infer_calls.fetch_add(1, Ordering::SeqCst);
            let text = self.replies.lock().unwrap().pop().unwrap_or_default();
            Ok(InferenceResponse {
                message: Message::assistant(text),
                usage: TokenUsage::default(),
                model: "stub".to_string(),
                metadata: Default::default(),
            })
        }

        async fn infer_stream(
            &self,
            _req: InferenceRequest,
        ) -> std::result::Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = oneai_core::InferenceStreamChunk> + Send>>,
            OneAIError,
        > {
            Err(OneAIError::Other("infer_stream not supported by StubProvider".to_string()))
        }

        fn capabilities(&self) -> ModelCapability {
            ModelCapability::gpt4_class()
        }

        fn config(&self) -> &ModelConfig {
            // `self_correct` never reads config(); a static is the simplest
            // way to hand out a borrowed ModelConfig without lifetime gymnastics.
            static CFG: std::sync::OnceLock<ModelConfig> = std::sync::OnceLock::new();
            CFG.get_or_init(|| ModelConfig {
                provider_type: ProviderType::Cloud,
                cloud_kind: None,
                api_key: None,
                base_url: None,
                port: None,
                model_name: Some("stub".to_string()),
                model_path: None,
                extra: Default::default(),
            })
        }
    }

    fn base_request() -> InferenceRequest {
        let mut conv = Conversation::new();
        conv.add_message(Message::user("emit a tool call"));
        InferenceRequest {
            conversation: conv,
            tools: vec![],
            max_tokens: Some(256),
            temperature: Some(0.0),
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: Default::default(),
        }
    }

    #[tokio::test]
    async fn self_correct_succeeds_on_retry() {
        // First corrected output is valid JSON → success on attempt 1.
        let provider = StubProvider::new(vec!["{\"ok\": true}".to_string()]);
        let loop_ = FallbackLoop::with_max_retries(3);
        let out = loop_
            .self_correct(&provider, &base_request(), "broken", "unclosed brace")
            .await
            .expect("must succeed when corrected output parses");
        assert_eq!(out.fallback_retries, 1);
        assert_eq!(out.parsing_layer, ParsingLayer::FallbackSelfCorrection);
        assert_eq!(provider.infer_calls(), 1);
    }

    #[tokio::test]
    async fn self_correct_retries_up_to_max_then_exhausts() {
        // Every corrected output stays malformed → must retry exactly
        // `max_retries` times (previously the body hardcoded a single attempt
        // regardless of max_retries).
        let provider = StubProvider::new(vec![
            "still broken 3".to_string(),
            "still broken 2".to_string(),
            "still broken 1".to_string(),
        ]);
        let loop_ = FallbackLoop::with_max_retries(3);
        let err = loop_
            .self_correct(&provider, &base_request(), "broken", "unclosed brace")
            .await
            .expect_err("must exhaust when no attempt parses");
        match err {
            OneAIError::Parser(ParserError::FallbackExhausted { retries, .. }) => {
                assert_eq!(retries, 3, "must perform all 3 retries");
            }
            other => panic!("expected FallbackExhausted, got {other:?}"),
        }
        assert_eq!(provider.infer_calls(), 3);
    }
}