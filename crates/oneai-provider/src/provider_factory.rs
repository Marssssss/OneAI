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

use crate::anthropic::AnthropicProvider;
use crate::compat::{Compat, CompatFamily};
use crate::gemini::GeminiProvider;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAIProvider;
use oneai_core::traits::LlmProvider;
use oneai_core::{CloudProviderKind, ModelConfig, ProviderType};

/// Factory for creating LlmProvider instances from configuration.
///
/// Dispatches via the resolved [`Compat`] profile (`Compat::from_config`),
/// which is the single authority for "which protocol family does this
/// endpoint speak" — driven by `base_url` host rules in [`Compat::detect`].
/// Most LLM services today use the OpenAI-compatible protocol, so any
/// unrecognized URL defaults to OpenAI-compatible.
pub struct ProviderFactory;

impl ProviderFactory {
    /// Create the appropriate provider based on the ModelConfig.
    ///
    /// Dispatch is **Compat-driven**: the config is first normalized
    /// (`resolve_provider`, which sets `cloud_kind`/`provider_type` from the
    /// host rules in `Compat::detect`), then [`Compat::from_config`] selects
    /// the family, then the matching provider struct is constructed with the
    /// resolved profile. Behavior is identical to the prior `match cloud_kind`
    /// dispatch — see `factory_dispatch_*` tests.
    pub fn create(config: ModelConfig) -> Box<dyn LlmProvider> {
        let mut resolved = Self::resolve_provider(config);
        if matches!(resolved.provider_type, ProviderType::Transformers) {
            panic!("Transformers provider not yet implemented. Use Local (Ollama) instead.");
        }
        let compat = Compat::from_config(&resolved);
        // Normalize the base URL for the resolved family (issue #41): append a
        // missing version segment, collapse duplicates, strip a pasted endpoint.
        // Written back so `provider.config()` (and thus the URL builders and the
        // app-server probe's display) all agree on one effective URL.
        resolved.base_url = resolved
            .base_url
            .map(|u| crate::normalize_base_url(compat.family, &u));
        match compat.family {
            CompatFamily::AnthropicCompat => {
                Box::new(AnthropicProvider::with_compat(resolved, compat))
            }
            CompatFamily::GeminiCompat => Box::new(GeminiProvider::with_compat(resolved, compat)),
            CompatFamily::OllamaCompat => Box::new(OllamaProvider::with_compat(resolved, compat)),
            CompatFamily::OpenAICompat => Box::new(OpenAIProvider::with_compat(resolved, compat)),
        }
    }

    /// Normalize a `ModelConfig` — set `cloud_kind`/`provider_type` from the
    /// host rules in [`Compat::detect`] (the single authority). If
    /// `cloud_kind` is already explicitly set, no auto-detection is applied.
    ///
    /// Detection logic (delegated to `Compat::detect`):
    /// - URLs containing `anthropic.com` → Anthropic protocol
    /// - URLs containing `generativelanguage.googleapis.com` /
    ///   `aiplatform.googleapis.com` → Gemini
    /// - `localhost` / `127.0.0.1` / `0.0.0.0` / `[::1]` → Ollama (Local)
    /// - Everything else → OpenAI-compatible (covers OpenAI itself,
    ///   阿里百炼/DashScope, DeepSeek, 智谱/GLM, Mistral, Groq, etc.)
    fn resolve_provider(config: ModelConfig) -> ModelConfig {
        // If cloud_kind is already explicitly set, no auto-detection needed.
        if config.cloud_kind.is_some() {
            return config;
        }

        let url = config.resolved_url();
        let compat = Compat::detect(&url, config.cloud_kind, config.provider_type);
        match compat.family {
            CompatFamily::AnthropicCompat => ModelConfig {
                cloud_kind: Some(CloudProviderKind::Anthropic),
                ..config
            },
            CompatFamily::GeminiCompat => ModelConfig {
                cloud_kind: Some(CloudProviderKind::Gemini),
                ..config
            },
            CompatFamily::OllamaCompat => ModelConfig {
                provider_type: ProviderType::Local,
                cloud_kind: None,
                ..config
            },
            CompatFamily::OpenAICompat => ModelConfig {
                cloud_kind: Some(CloudProviderKind::OpenAI),
                ..config
            },
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

    // ── Compat-driven dispatch invariants (behavior preserved) ─────────
    //
    // `create` now dispatches via `Compat::from_config`; these assert the
    // family each endpoint resolves to, so a future refactor can't silently
    // reroute a provider. (Invariants on the family mapping, not frozen
    // values — per 戒律 6.)

    fn family_of(config: ModelConfig) -> CompatFamily {
        let resolved = ProviderFactory::resolve_provider(config);
        Compat::from_config(&resolved).family
    }

    #[test]
    fn factory_dispatch_openai_for_openai_host() {
        let cfg = ModelConfig::openai_compatible(
            "sk".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
        );
        assert_eq!(family_of(cfg), CompatFamily::OpenAICompat);
    }

    #[test]
    fn factory_dispatch_openai_for_compat_hosts() {
        // DeepSeek / 百炼 / 智谱 — all OpenAI-compatible.
        for url in [
            "https://api.deepseek.com/v1",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "https://open.bigmodel.cn/api/paas/v4",
        ] {
            let cfg = ModelConfig::openai_compatible("sk".into(), url.into(), "m".into());
            assert_eq!(family_of(cfg), CompatFamily::OpenAICompat, "{url}");
        }
    }

    #[test]
    fn factory_dispatch_anthropic_for_anthropic_host() {
        let cfg = ModelConfig {
            provider_type: oneai_core::ProviderType::Cloud,
            cloud_kind: None,
            api_key: Some("sk".into()),
            base_url: Some("https://api.anthropic.com/v1".into()),
            ..ModelConfig::default()
        };
        assert_eq!(family_of(cfg), CompatFamily::AnthropicCompat);
    }

    #[test]
    fn factory_dispatch_gemini_for_google_host() {
        let cfg = ModelConfig::gemini("sk".into(), "gemini-2.5-pro".into());
        assert_eq!(family_of(cfg), CompatFamily::GeminiCompat);
    }

    #[test]
    fn factory_dispatch_ollama_for_localhost() {
        let cfg = ModelConfig {
            provider_type: oneai_core::ProviderType::Cloud,
            cloud_kind: None,
            api_key: None,
            base_url: Some("http://localhost:11434".into()),
            ..ModelConfig::default()
        };
        assert_eq!(family_of(cfg), CompatFamily::OllamaCompat);
    }

    #[test]
    fn factory_dispatch_ollama_for_explicit_local_type() {
        // ProviderType::Local → Ollama regardless of host.
        let cfg = ModelConfig {
            provider_type: oneai_core::ProviderType::Local,
            cloud_kind: None,
            api_key: None,
            base_url: Some("https://api.openai.com/v1".into()),
            ..ModelConfig::default()
        };
        assert_eq!(family_of(cfg), CompatFamily::OllamaCompat);
    }
}
