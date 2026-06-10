//! Three-layer output parser — orchestrates all 3 parsing defense layers.
//!
//! Layer 1: Constrained decoding (BNF grammar) — guarantees correct format at generation.
//! Layer 2: Fuzzy JSON repair — repairs malformed output.
//! Layer 3: Fallback self-correction — re-feeds error to model for re-generation.

use crate::constrained::ConstrainedDecoder;
use crate::fallback::FallbackLoop;
use crate::fuzzy::FuzzyJsonRepair;
use oneai_core::{
    ContentBlock, InferenceRequest, OneAIError, ParsedOutput, ParsingLayer,
};
use oneai_core::error::ParserError;
use oneai_core::traits::{LlmProvider, OutputParser};
use async_trait::async_trait;

/// The complete 3-layer parsing defense orchestrator.
pub struct ThreeLayerParser {
    /// Layer 1: Constrained decoder (optional — not all providers support it).
    constrained: Box<dyn ConstrainedDecoder>,

    /// Layer 2: Fuzzy JSON repair engine (always active).
    fuzzy: FuzzyJsonRepair,

    /// Layer 3: Fallback self-correction loop.
    fallback: FallbackLoop,

    /// LLM provider for Layer 3 self-correction re-requests.
    provider: Option<Box<dyn LlmProvider>>,
}

impl ThreeLayerParser {
    /// Create a new parser with default settings and no constrained decoder.
    pub fn new() -> Self {
        Self {
            constrained: Box::new(crate::constrained::StubConstrainedDecoder),
            fuzzy: FuzzyJsonRepair::new(),
            fallback: FallbackLoop::new(),
            provider: None,
        }
    }

    /// Create a parser with a specific constrained decoder (Layer 1).
    pub fn with_constrained_decoder(decoder: Box<dyn ConstrainedDecoder>) -> Self {
        Self {
            constrained: decoder,
            fuzzy: FuzzyJsonRepair::new(),
            fallback: FallbackLoop::new(),
            provider: None,
        }
    }

    /// Set the LLM provider for Layer 3 self-correction.
    pub fn with_provider(mut self, provider: Box<dyn LlmProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Set custom max retries for the fallback loop.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.fallback = FallbackLoop::with_max_retries(max_retries);
        self
    }

    /// Parse tool calls from model output.
    ///
    /// Attempts to extract ContentBlock::ToolCall from the raw output.
    pub fn parse_tool_calls(&self, raw: &str) -> std::result::Result<Vec<ContentBlock>, OneAIError> {
        // Try parsing as JSON first
        let parsed = self.fuzzy.repair_and_parse(raw)?;

        // Look for tool_calls in the parsed JSON
        if let Some(tool_calls) = parsed.get("tool_calls") {
            if let Some(calls) = tool_calls.as_array() {
                let mut result = Vec::new();
                for call in calls {
                    let id = call
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = call
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = call
                        .get("arguments")
                        .or_else(|| call.get("args"))
                        .map(|v| v.to_string())
                        .unwrap_or("{}".to_string());

                    result.push(ContentBlock::ToolCall { id, name, args });
                }
                return Ok(result);
            }
        }

        // Single function call format
        if let Some(function) = parsed.get("function_call").or_else(|| parsed.get("tool_call")) {
            let id = function
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = function
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = function
                .get("arguments")
                .or_else(|| function.get("args"))
                .map(|v| v.to_string())
                .unwrap_or("{}".to_string());

            return Ok(vec![ContentBlock::ToolCall { id, name, args }]);
        }

        Ok(Vec::new())
    }
}

impl Default for ThreeLayerParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OutputParser for ThreeLayerParser {
    async fn parse<'a>(
        &self,
        raw_output: &str,
        _schema: Option<&'a serde_json::Value>,
    ) -> std::result::Result<ParsedOutput, OneAIError> {
        // If constrained decoding was active (Layer 1), the output should already be correct.
        if self.constrained.is_available() {
            // Layer 1 succeeded — output is guaranteed correct at generation time
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_output) {
                return Ok(ParsedOutput {
                    content: vec![ContentBlock::Text { text: raw_output.to_string() }],
                    parsing_layer: ParsingLayer::ConstrainedDecoding,
                    fallback_retries: 0,
                });
            }
        }

        // Layer 2: Attempt fuzzy JSON repair
        match self.fuzzy.repair_and_parse(raw_output) {
            Ok(val) => {
                // Successfully repaired or directly parsed
                // Check if the result contains tool calls
                let tool_calls = self.parse_tool_calls(raw_output).unwrap_or_default();

                let content = if tool_calls.is_empty() {
                    vec![ContentBlock::Text { text: raw_output.to_string() }]
                } else {
                    tool_calls
                };

                return Ok(ParsedOutput {
                    content,
                    parsing_layer: ParsingLayer::FuzzyRepair,
                    fallback_retries: 0,
                });
            }
            Err(_) => {
                // Layer 2 failed — proceed to Layer 3
            }
        }

        // Layer 3: Fallback self-correction
        if let Some(provider) = &self.provider {
            // We need an original request for self-correction — but we don't have one here.
            // The self-correction requires the original InferenceRequest context.
            // This is handled at the agent loop level, not in the parser directly.
            // For now, return the error — the agent loop will handle Layer 3.
            return Err(OneAIError::Parser(ParserError::FuzzyRepairFailed(
                format!("Fuzzy repair failed, Layer 3 requires agent loop context: {}", &raw_output[..raw_output.len().min(200)]),
            )));
        }

        Err(OneAIError::Parser(ParserError::FuzzyRepairFailed(
            format!("No provider configured for Layer 3 fallback: {}", &raw_output[..raw_output.len().min(200)]),
        )))
    }
}