//! Constrained decoding abstraction — Layer 1 of the 3-layer parsing defense.
//!
//! This module defines the trait for constrained decoding (BNF grammar, JSON Schema, etc.)
//! The actual implementation is deferred until a constrained-decoding provider is connected.
//! When available, constrained decoding guarantees correct output format at generation time,
//! making Layers 2 and 3 unnecessary.

use oneai_core::{ConstrainedMode, InferenceRequest, OneAIError};

/// Constrained decoder trait — Layer 1.
///
/// Implementations activate BNF/JSON Schema grammar constraints on providers
/// that support them (LiteRT-LM, Ollama structured output, llama.cpp grammar).
pub trait ConstrainedDecoder: Send + Sync {
    /// Whether constrained decoding is available for the current provider.
    fn is_available(&self) -> bool;

    /// Apply constrained decoding to an inference request.
    fn apply_constraint(
        &self,
        req: &mut InferenceRequest,
        mode: ConstrainedMode,
        grammar: &str,
    ) -> std::result::Result<(), OneAIError>;
}

/// A stub constrained decoder that always reports "not available".
/// Used when no constrained-decoding provider is connected.
pub struct StubConstrainedDecoder;

impl ConstrainedDecoder for StubConstrainedDecoder {
    fn is_available(&self) -> bool {
        false
    }

    fn apply_constraint(
        &self,
        _req: &mut InferenceRequest,
        _mode: ConstrainedMode,
        _grammar: &str,
    ) -> std::result::Result<(), OneAIError> {
        Err(OneAIError::Parser(oneai_core::error::ParserError::ConstrainedNotSupported(
            "No constrained decoder configured".to_string(),
        )))
    }
}