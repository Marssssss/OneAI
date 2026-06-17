//! Provider factory — creates the appropriate LlmProvider from a ModelConfig.
//!
//! The factory auto-detects the provider type based on `base_url` when
//! `cloud_kind` is not explicitly set:
//!
//! - `api.openai.com` → OpenAI
//! - `api.anthropic.com` → Anthropic
//! - `generativelanguage.googleapis.com` → Gemini
//! - `dashscope.aliyuncs.com` → OpenAI-compatible (阿里百炼)
//! - `api.deepseek.com` → OpenAI-compatible (DeepSeek)
//! - `open.bigmodel.cn` → OpenAI-compatible (智谱)
//! - `localhost` / `127.0.0.1` → Ollama (Local)
//! - anything else → OpenAI-compatible (most services use OpenAI protocol)

use crate::openai::OpenAIProvider;
use crate::anthropic::AnthropicProvider;
use crate::gemini::GeminiProvider;
use crate::ollama::OllamaProvider;
use oneai_core::{CloudProviderKind, ModelConfig, ProviderType};
use oneai_core::traits::LlmProvider;

/// Factory for creating LlmProvider instances from configuration.
///
/// Automatically detects the provider type from `base_url` when `cloud_kind`
/// is not explicitly specified. Most LLM services today use the OpenAI-compatible
/// protocol, so any unrecognized URL defaults to OpenAI-compatible.
pub struct ProviderFactory;

impl ProviderFactory {
    /// Create the appropriate provider based on the ModelConfig.
    ///
    /// If `cloud_kind` is explicitly set, uses that. Otherwise, auto-detects
    /// from `base_url`. If neither is set, defaults to OpenAI.
    pub fn create(config: ModelConfig) -> Box<dyn LlmProvider> {
        let resolved_config = Self::resolve_provider(config);
        match resolved_config.provider_type {
            ProviderType::Cloud => {
                match resolved_config.cloud_kind {
                    Some(CloudProviderKind::Anthropic) => {
                        Box::new(AnthropicProvider::new(resolved_config))
                    }
                    Some(CloudProviderKind::Gemini) => {
                        Box::new(GeminiProvider::new(resolved_config))
                    }
                    // OpenAI and all OpenAI-compatible services (百炼, DeepSeek, 智谱, etc.)
                    Some(CloudProviderKind::OpenAI) | None => {
                        Box::new(OpenAIProvider::new(resolved_config))
                    }
                }
            }
            ProviderType::Local => {
                Box::new(OllamaProvider::new(resolved_config))
            }
            ProviderType::Transformers => {
                panic!("Transformers provider not yet implemented. Use Local (Ollama) instead.");
            }
        }
    }

    /// Auto-detect provider type from `base_url`.
    ///
    /// Detection logic:
    /// - URLs containing `anthropic.com` → Anthropic protocol
    /// - URLs containing `localhost` / `127.0.0.1` / `0.0.0.0` → Ollama (Local)
    /// - Everything else → OpenAI-compatible protocol (covers OpenAI itself,
    ///   阿里百炼/DashScope, DeepSeek, 智谱/GLM, Mistral, Groq, etc.)
    fn resolve_provider(config: ModelConfig) -> ModelConfig {
        // If cloud_kind is already explicitly set, no auto-detection needed
        if config.cloud_kind.is_some() {
            return config;
        }

        let url = config.resolved_url().to_lowercase();

        // Detect Anthropic
        if url.contains("anthropic.com") {
            return ModelConfig {
                cloud_kind: Some(CloudProviderKind::Anthropic),
                ..config
            };
        }

        // Detect Gemini
        if url.contains("generativelanguage.googleapis.com") || url.contains("aiplatform.googleapis.com") {
            return ModelConfig {
                cloud_kind: Some(CloudProviderKind::Gemini),
                ..config
            };
        }

        // Detect local/Ollama
        if url.contains("localhost") || url.contains("127.0.0.1") || url.contains("0.0.0.0") || url.contains("[::1]") {
            return ModelConfig {
                provider_type: ProviderType::Local,
                cloud_kind: None,
                ..config
            };
        }

        // Everything else → OpenAI-compatible
        // This covers: api.openai.com, dashscope.aliyuncs.com, api.deepseek.com,
        // open.bigmodel.cn, api.mistral.ai, api.groq.com, and any custom endpoint
        ModelConfig {
            cloud_kind: Some(CloudProviderKind::OpenAI),
            ..config
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::CloudProviderKind;

    #[test]
    fn test_detect_openai() {
        let config = ModelConfig::openai_compatible(
            "sk-test".to_string(),
            "https://api.openai.com/v1".to_string(),
            "gpt-4".to_string(),
        );
        let resolved = ProviderFactory::resolve_provider(config);
        assert_eq!(resolved.cloud_kind, Some(CloudProviderKind::OpenAI));
    }

    #[test]
    fn test_detect_bailian() {
        let config = ModelConfig::openai_compatible(
            "sk-test".to_string(),
            "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string(),
            "qwen-plus".to_string(),
        );
        let resolved = ProviderFactory::resolve_provider(config);
        assert_eq!(resolved.cloud_kind, Some(CloudProviderKind::OpenAI));
    }

    #[test]
    fn test_detect_deepseek() {
        let config = ModelConfig::openai_compatible(
            "sk-test".to_string(),
            "https://api.deepseek.com/v1".to_string(),
            "deepseek-chat".to_string(),
        );
        let resolved = ProviderFactory::resolve_provider(config);
        assert_eq!(resolved.cloud_kind, Some(CloudProviderKind::OpenAI));
    }

    #[test]
    fn test_detect_anthropic() {
        let config = ModelConfig {
            provider_type: oneai_core::ProviderType::Cloud,
            cloud_kind: None,
            api_key: Some("sk-ant-test".to_string()),
            base_url: Some("https://api.anthropic.com/v1".to_string()),
            ..ModelConfig::default()
        };
        let resolved = ProviderFactory::resolve_provider(config);
        assert_eq!(resolved.cloud_kind, Some(CloudProviderKind::Anthropic));
    }

    #[test]
    fn test_detect_ollama() {
        let config = ModelConfig {
            provider_type: oneai_core::ProviderType::Cloud,
            cloud_kind: None,
            api_key: None,
            base_url: Some("http://localhost:11434".to_string()),
            ..ModelConfig::default()
        };
        let resolved = ProviderFactory::resolve_provider(config);
        assert_eq!(resolved.provider_type, oneai_core::ProviderType::Local);
    }

    #[test]
    fn test_explicit_cloud_kind_not_overridden() {
        let config = ModelConfig {
            provider_type: oneai_core::ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::Anthropic),
            api_key: Some("sk-test".to_string()),
            base_url: Some("https://api.openai.com/v1".to_string()),
            ..ModelConfig::default()
        };
        let resolved = ProviderFactory::resolve_provider(config);
        // Explicit Anthropic should not be overridden by URL detection
        assert_eq!(resolved.cloud_kind, Some(CloudProviderKind::Anthropic));
    }
}