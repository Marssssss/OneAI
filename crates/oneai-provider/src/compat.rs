//! Provider compatibility profiles — drive provider dispatch from `base_url`.
//!
//! OneAI's `ProviderFactory` historically dispatched with a chain of
//! `if url.contains("anthropic.com") / "localhost" / …` in
//! [`crate::provider_factory::resolve_provider`]. As the OpenAI-compatible
//! surface grows (Ollama / vLLM / LM Studio / Groq / 阿里百炼 / DeepSeek /
//! 智谱), each with subtle protocol quirks, that brittle string-matching is
//! replaced by a [`Compat`] **flagset** resolved once per provider.
//!
//! # Two views (both reproduce prior behavior exactly)
//!
//! - [`Compat::detect`] — *pre-resolution*: given a `base_url` + explicit
//!   `cloud_kind` + `provider_type`, returns the [`Compat`] the factory should
//!   use. This is the url-detection logic `resolve_provider` now delegates to.
//! - [`Compat::from_config`] — *post-resolution*: given an already-normalized
//!   `ModelConfig`, returns the [`Compat`] mirroring the old `create` match.
//!
//! Behavior is preserved: every `ModelConfig` that previously built e.g.
//! `OpenAIProvider` still builds `OpenAIProvider` (see `factory_dispatch_*`
//! tests in `provider_factory.rs`).
//!
//! # Scope note (non-speculative)
//!
//! The existing request-shaping quirks (`response_format` json_schema,
//! Anthropic `cache_control`) are **already gated per-request** via
//! `InferenceRequest.constrained_output` / `prompt_cache_policy` metadata —
//! there is no per-provider `if provider == X` branch to migrate. So this
//! crate stores the flagset (consumed by the `token compat` CLI + provider
//! `capabilities()` + future `Api`/`Provider` split, §4.3) without gating any
//! working request path — per the 戒律: no flag without a consumer.

use oneai_core::{CloudProviderKind, ModelConfig, ProviderType};

// ─── CompatFamily ────────────────────────────────────────────────────────────

/// The protocol family a provider speaks — drives `ProviderFactory` dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CompatFamily {
    /// OpenAI-compatible `/v1/chat/completions` (OpenAI, DeepSeek, 智谱, 百炼,
    /// Groq, vLLM, LM Studio, …).
    OpenAICompat,
    /// Anthropic `/v1/messages` native protocol.
    AnthropicCompat,
    /// Google Gemini `generateContent` native protocol.
    GeminiCompat,
    /// Ollama (OpenAI-compatible `/v1/...` + native `/api/...`).
    OllamaCompat,
}

impl CompatFamily {
    /// Human label.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAICompat => "openai-compatible",
            Self::AnthropicCompat => "anthropic",
            Self::GeminiCompat => "gemini",
            Self::OllamaCompat => "ollama (local)",
        }
    }
}

// ─── AuthStyle ──────────────────────────────────────────────────────────────

/// How the provider authenticates requests — a real per-family property,
/// surfaced by `token compat` and reserved for the §4.3 `Api`/`Provider` split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`.
    #[default]
    Bearer,
    /// `x-api-key: <key>` (Anthropic).
    XApiKey,
    /// `x-goog-api-key` / `?key=` (Gemini).
    GoogleApiKey,
    /// No auth (local Ollama).
    None,
}

// ─── Compat ──────────────────────────────────────────────────────────────────

/// Resolved compatibility flagset for a provider endpoint.
///
/// Built once at provider construction; stored on each provider struct. Flags
/// are real per-family properties (consumed by `token compat` CLI display +
/// provider `capabilities()`), not speculative hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Compat {
    /// Protocol family — the dispatch key.
    pub family: CompatFamily,
    /// Honors strict JSON-schema constrained output (`response_format` /
    /// `format` json_schema).
    pub supports_strict_json_schema: bool,
    /// Honors explicit prompt-cache retention controls.
    pub supports_prompt_cache: bool,
    /// Uses Anthropic-style `cache_control: ephemeral` breakpoint blocks.
    pub cache_via_control_block: bool,
    /// Surfaces thinking via a `thinking` request field (Anthropic interleaved).
    pub thinking_via_field: bool,
    /// Surfaces reasoning via a separate `reasoning_content` channel
    /// (OpenAI o-series / DeepSeek-R1).
    pub reasoning_via_field: bool,
    /// Exposes a native chat API alongside the OpenAI-compat surface (Ollama).
    pub native_chat_api: bool,
    /// Request authentication style.
    pub auth: AuthStyle,
}

impl Compat {
    /// The default flagset for a family.
    pub fn default_for(family: CompatFamily) -> Self {
        match family {
            CompatFamily::OpenAICompat => Self {
                family,
                supports_strict_json_schema: true,
                supports_prompt_cache: true,
                cache_via_control_block: false,
                thinking_via_field: false,
                reasoning_via_field: true,
                native_chat_api: false,
                auth: AuthStyle::Bearer,
            },
            CompatFamily::AnthropicCompat => Self {
                family,
                supports_strict_json_schema: true,
                supports_prompt_cache: true,
                cache_via_control_block: true,
                thinking_via_field: true,
                reasoning_via_field: false,
                native_chat_api: false,
                auth: AuthStyle::XApiKey,
            },
            CompatFamily::GeminiCompat => Self {
                family,
                supports_strict_json_schema: true,
                supports_prompt_cache: true,
                cache_via_control_block: false,
                thinking_via_field: false,
                reasoning_via_field: false,
                native_chat_api: false,
                auth: AuthStyle::GoogleApiKey,
            },
            CompatFamily::OllamaCompat => Self {
                family,
                supports_strict_json_schema: true,
                supports_prompt_cache: false,
                cache_via_control_block: false,
                thinking_via_field: false,
                reasoning_via_field: true,
                native_chat_api: true,
                auth: AuthStyle::None,
            },
        }
    }

    /// Pre-resolution detection — the url/host rules `resolve_provider` uses.
    ///
    /// Mirrors `provider_factory::resolve_provider` exactly: an explicit
    /// `cloud_kind` wins (no url probing); otherwise host substrings decide.
    /// `provider_type::Local` is honored as Ollama regardless of host.
    pub fn detect(
        base_url: &str,
        cloud_kind: Option<CloudProviderKind>,
        provider_type: ProviderType,
    ) -> Self {
        // Local deployments always dispatch to Ollama (mirrors `create`'s
        // `ProviderType::Local => OllamaProvider` arm, which ignores cloud_kind).
        if matches!(provider_type, ProviderType::Local) {
            return Self::default_for(CompatFamily::OllamaCompat);
        }
        // Explicit cloud_kind wins (mirrors resolve_provider's early return).
        if let Some(kind) = cloud_kind {
            let family = match kind {
                CloudProviderKind::Anthropic => CompatFamily::AnthropicCompat,
                CloudProviderKind::Gemini => CompatFamily::GeminiCompat,
                CloudProviderKind::OpenAI => CompatFamily::OpenAICompat,
            };
            return Self::default_for(family);
        }
        // Auto-detect from host. Identical host rules to resolve_provider.
        let url = base_url.to_lowercase();
        let family = if url.contains("anthropic.com") {
            CompatFamily::AnthropicCompat
        } else if url.contains("generativelanguage.googleapis.com")
            || url.contains("aiplatform.googleapis.com")
        {
            CompatFamily::GeminiCompat
        } else if url.contains("localhost")
            || url.contains("127.0.0.1")
            || url.contains("0.0.0.0")
            || url.contains("[::1]")
        {
            CompatFamily::OllamaCompat
        } else {
            // Everything else → OpenAI-compatible (OpenAI, 百炼, DeepSeek,
            // 智谱, Mistral, Groq, vLLM, LM Studio, …).
            CompatFamily::OpenAICompat
        };
        Self::default_for(family)
    }

    /// Post-resolution view from a (possibly normalized) `ModelConfig`.
    ///
    /// Mirrors the old `create` match: `Local => Ollama`; `Cloud` +
    /// `cloud_kind` decides (`None => OpenAI`).
    pub fn from_config(config: &ModelConfig) -> Self {
        Self::detect(
            &config.resolved_url(),
            config.cloud_kind,
            config.provider_type,
        )
    }
}

impl Default for Compat {
    fn default() -> Self {
        Self::default_for(CompatFamily::OpenAICompat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::Cloud,
            cloud_kind: None,
            api_key: None,
            base_url: Some(url.to_string()),
            port: None,
            model_name: None,
            model_path: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn detect_explicit_cloud_kind_wins_over_host() {
        // localhost but explicit OpenAI cloud_kind → OpenAI (mirrors
        // resolve_provider early-return).
        let c = Compat::detect(
            "http://localhost:11434",
            Some(CloudProviderKind::OpenAI),
            ProviderType::Cloud,
        );
        assert_eq!(c.family, CompatFamily::OpenAICompat);
    }

    #[test]
    fn detect_anthropic_host() {
        let c = Compat::detect("https://api.anthropic.com/v1", None, ProviderType::Cloud);
        assert_eq!(c.family, CompatFamily::AnthropicCompat);
        assert_eq!(c.auth, AuthStyle::XApiKey);
        assert!(c.cache_via_control_block);
        assert!(c.thinking_via_field);
    }

    #[test]
    fn detect_gemini_host() {
        let c = Compat::detect(
            "https://generativelanguage.googleapis.com/v1beta",
            None,
            ProviderType::Cloud,
        );
        assert_eq!(c.family, CompatFamily::GeminiCompat);
        assert_eq!(c.auth, AuthStyle::GoogleApiKey);
    }

    #[test]
    fn detect_localhost_is_ollama() {
        let c = Compat::detect("http://localhost:11434", None, ProviderType::Cloud);
        assert_eq!(c.family, CompatFamily::OllamaCompat);
        assert!(c.native_chat_api);
        assert_eq!(c.auth, AuthStyle::None);
    }

    #[test]
    fn detect_local_provider_type_is_ollama_regardless_of_host() {
        // ProviderType::Local + a non-localhost url + no cloud_kind → Ollama
        // (mirrors create's Local arm).
        let c = Compat::detect("https://api.openai.com/v1", None, ProviderType::Local);
        assert_eq!(c.family, CompatFamily::OllamaCompat);
    }

    #[test]
    fn detect_unknown_host_defaults_openai_compat() {
        let c = Compat::detect("https://api.deepseek.com/v1", None, ProviderType::Cloud);
        assert_eq!(c.family, CompatFamily::OpenAICompat);
        assert_eq!(c.auth, AuthStyle::Bearer);
        assert!(c.supports_strict_json_schema);
    }

    #[test]
    fn from_config_matches_detect() {
        let config = cfg("https://api.anthropic.com/v1");
        assert_eq!(
            Compat::from_config(&config).family,
            CompatFamily::AnthropicCompat
        );
    }

    #[test]
    fn default_for_sets_distinct_auth_per_family() {
        assert_eq!(
            Compat::default_for(CompatFamily::OpenAICompat).auth,
            AuthStyle::Bearer
        );
        assert_eq!(
            Compat::default_for(CompatFamily::AnthropicCompat).auth,
            AuthStyle::XApiKey
        );
        assert_eq!(
            Compat::default_for(CompatFamily::GeminiCompat).auth,
            AuthStyle::GoogleApiKey
        );
        assert_eq!(
            Compat::default_for(CompatFamily::OllamaCompat).auth,
            AuthStyle::None
        );
    }
}
