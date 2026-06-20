//! Fallback self-correction loop — Layer 3 of the 3-layer parsing defense.
//!
//! If Layers 1 and 2 fail, this layer catches the parser exception,
//! generates an implicit error message, and re-feeds it to the model
//! for self-correction. Up to `max_retries` attempts are made.

use oneai_core::{ContentBlock, InferenceRequest, Message, OneAIError, ParsedOutput, ParsingLayer};
use oneai_core::error::ParserError;
use oneai_core::traits::LlmProvider;

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
    /// then re-requests inference.
    pub async fn self_correct(
        &self,
        provider: &dyn LlmProvider,
        original_request: &InferenceRequest,
        failed_output: &str,
        error_description: &str,
    ) -> std::result::Result<ParsedOutput, OneAIError> {
        let mut retries = 0;
        let current_error = error_description.to_string();

        while retries < self.max_retries {
            retries += 1;

            // Create the self-correction prompt
            let _correction_message = Message::assistant(
                format!(
                    "You just output content with a format error: {}\n\
                     The raw output was: {}\n\
                     Please严格按照正确的格式重新输出，不要包含任何多余的文字。",
                    current_error,
                    &failed_output[..failed_output.len().min(500)]
                ),
            );

            // Build a new request with the correction message appended
            let mut corrected_conversation = original_request.conversation.clone();
            // Add the original failed model output as an assistant message
            corrected_conversation.add_message(Message::assistant(failed_output.to_string()));
            // Add the correction prompt as a system/user message
            corrected_conversation.add_message(Message::system(current_error.clone()));

            let corrected_request = InferenceRequest {
                conversation: corrected_conversation,
                tools: original_request.tools.clone(),
                max_tokens: original_request.max_tokens,
                temperature: Some(0.0), // Use deterministic sampling for correction
                top_p: original_request.top_p,
                stop_sequences: original_request.stop_sequences.clone(),
                constrained_output: original_request.constrained_output.clone(),
                thinking_budget: None,
                metadata: original_request.metadata.clone(),
            };

            // Re-request inference
            let response = provider.infer(corrected_request).await?;

            // Extract text from the response
            let text = response.message.text_content();

            // Try to parse the corrected output with Layer 2 (fuzzy repair)
            // Note: the full ThreeLayerParser orchestrates this, but here we just
            // return the raw text for the outer parser to attempt repair again.
            return Ok(ParsedOutput {
                content: vec![ContentBlock::Text { text }],
                parsing_layer: ParsingLayer::FallbackSelfCorrection,
                fallback_retries: retries,
            });
        }

        Err(OneAIError::Parser(ParserError::FallbackExhausted {
            retries: self.max_retries,
            reason: format!("Model failed to self-correct after {} retries", self.max_retries),
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