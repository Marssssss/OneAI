//! Provider factory — creates the appropriate LlmProvider from a ModelConfig.

use crate::openai::OpenAIProvider;
use crate::anthropic::AnthropicProvider;
use crate::ollama::OllamaProvider;
use oneai_core::{CloudProviderKind, ModelConfig, ProviderType};
use oneai_core::traits::LlmProvider;

/// Factory for creating LlmProvider instances from configuration.
pub struct ProviderFactory;

impl ProviderFactory {
    /// Create the appropriate provider based on the ModelConfig.
    ///
    /// Returns a boxed LlmProvider that can be used for inference.
    pub fn create(config: ModelConfig) -> Box<dyn LlmProvider> {
        match config.provider_type {
            ProviderType::Cloud => {
                match config.cloud_kind {
                    Some(CloudProviderKind::OpenAI) => {
                        Box::new(OpenAIProvider::new(config))
                    }
                    Some(CloudProviderKind::Anthropic) => {
                        Box::new(AnthropicProvider::new(config))
                    }
                    None => {
                        // Default to OpenAI-compatible for unspecified cloud providers
                        Box::new(OpenAIProvider::new(config))
                    }
                }
            }
            ProviderType::Local => {
                Box::new(OllamaProvider::new(config))
            }
            ProviderType::Transformers => {
                // Transformers provider not yet implemented — fall back to Ollama
                panic!("Transformers provider not yet implemented. Use Local (Ollama) instead.");
            }
        }
    }
}