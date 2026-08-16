//! Configuration management for OneAI CLI.
//!
//! Reads configuration from `~/.oneai/config.toml`, with fallback to
//! environment variables and defaults. Priority order:
//!   CLI arguments > environment variables > config.toml > defaults

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Full OneAI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OneaiConfig {
    /// LLM provider configuration (legacy single-provider section; still works
    /// for backward compat — surfaced as one "default" entry when `[[providers]]`
    /// is empty).
    #[serde(default)]
    pub provider: ProviderConfig,
    /// Multi-provider list (TOML `[[providers]]`). The app-server builds a
    /// `ProviderPool` from these at launch so any of them is live-switchable
    /// from the composer. When empty, the legacy `[provider]` section is used.
    #[serde(default)]
    pub providers: Vec<ProviderEntryConfig>,
    /// The active provider name (which `[[providers]]` entry the pool routes
    /// to at launch). Absent ⇒ the first entry (priority 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
    /// Domain configuration.
    #[serde(default)]
    pub domain: DomainConfig,
    /// UI configuration.
    #[serde(default)]
    pub ui: UiConfig,
    /// Sampling / generation parameters (temperature, top_p, max_tokens,
    /// thinking_budget, stop_sequences). All fields optional — unset fields
    /// inherit the agent-loop's scenario default.
    #[serde(default)]
    pub generation: oneai_core::GenerationConfig,
    /// Embedding provider configuration. Default is zero-config `provider =
    /// "auto"` (probes env keys / local Ollama; absent → keyword-recall). Most
    /// users leave this section out entirely.
    #[serde(default)]
    pub embedding: oneai_core::EmbeddingConfig,
}

/// One entry in the `[[providers]]` list — a named, switchable provider
/// configuration. `kind` is the cloud protocol ("openai"/"anthropic"/"gemini")
/// or local ("ollama"); absent ⇒ openai-compatible. `model`/`api_key`/
/// `base_url` map onto `oneai_core::ModelConfig`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderEntryConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// LLM provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// API key for the LLM provider.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Base URL for the LLM provider API.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Model name to use.
    #[serde(default = "default_model")]
    pub model: String,
}

fn default_model() -> String {
    "gpt-4".to_string()
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: None,
            model: default_model(),
        }
    }
}

/// Domain pack configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainConfig {
    /// Default domain pack name.
    #[serde(default = "default_domain")]
    pub default_pack: String,
}

fn default_domain() -> String {
    "coding".to_string()
}

impl Default for DomainConfig {
    fn default() -> Self {
        Self {
            default_pack: default_domain(),
        }
    }
}

/// UI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Theme: "dark" or "light".
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_theme() -> String {
    "dark".to_string()
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
        }
    }
}

impl Default for OneaiConfig {
    fn default() -> Self {
        Self {
            provider: ProviderConfig::default(),
            providers: Vec::new(),
            active_provider: None,
            domain: DomainConfig::default(),
            ui: UiConfig::default(),
            generation: oneai_core::GenerationConfig::new(),
            embedding: oneai_core::EmbeddingConfig::auto(),
        }
    }
}

impl OneaiConfig {
    /// Get the default config file path: `~/.oneai/config.toml`
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".oneai")
            .join("config.toml")
    }

    /// Get the default pack installation path: `~/.oneai/packs/`
    pub fn packs_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".oneai")
            .join("packs")
    }

    /// Load config from the default path, or return defaults if file doesn't exist.
    pub fn load_or_default() -> Self {
        let path = Self::default_path();
        if path.exists() {
            Self::load_from(&path).unwrap_or_else(|e| {
                eprintln!(
                    "Warning: Failed to load config from {}: {}",
                    path.display(),
                    e
                );
                Self::default()
            })
        } else {
            Self::default()
        }
    }

    /// Load config from a specific path.
    pub fn load_from(path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: OneaiConfig = toml::from_str(&content)?;
        Ok(config)
    }

    /// Save config to the default path, creating the directory if needed.
    pub fn save(&self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = Self::default_path();
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)?;
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(path)
    }

    /// Convert to a `ModelConfig`, merging environment variable overrides.
    ///
    /// Priority: env vars > config file > defaults
    pub fn to_model_config(&self) -> Option<oneai_core::ModelConfig> {
        // Environment variables override config file
        let api_key = std::env::var("ONEAI_API_KEY")
            .ok()
            .or(self.provider.api_key.clone());
        let base_url = std::env::var("ONEAI_BASE_URL")
            .ok()
            .or(self.provider.base_url.clone());
        let model = std::env::var("ONEAI_MODEL")
            .ok()
            .unwrap_or(self.provider.model.clone());

        if api_key.is_none() && base_url.is_none() {
            return None;
        }

        Some(oneai_core::ModelConfig {
            api_key,
            base_url,
            model_name: Some(model),
            ..oneai_core::ModelConfig::default()
        })
    }

    /// Merge CLI argument overrides into the config-derived ModelConfig.
    ///
    /// Priority: CLI args > env vars > config file > defaults
    pub fn to_model_config_with_overrides(
        &self,
        model_override: Option<&str>,
    ) -> Option<oneai_core::ModelConfig> {
        let mut config = self.to_model_config();

        // CLI model override takes highest priority
        if let Some(model) = model_override {
            if let Some(ref mut mc) = config {
                mc.model_name = Some(model.to_string());
            } else {
                // No provider config at all, but user specified a model — still need api_key
                // This case means the user wants to use a specific model but hasn't configured
                // a provider. Return None — they need to set ONEAI_API_KEY or config.
                return None;
            }
        }

        config
    }

    /// Get the default domain pack name, with optional CLI override.
    pub fn default_domain_pack(&self, domain_override: Option<&str>) -> String {
        domain_override
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.domain.default_pack.clone())
    }

    // ── Multi-provider (`[[providers]]`) management ───────────────────────────
    //
    // These mutate `self` in place; the caller persists via `save()`. The
    // app-server's `AppProbeImpl` calls them for `provider/add` · `/delete` ·
    // `/set_active`, then rebuilds the live pool — see `cmd_app_server.rs`.

    /// The effective provider list: the `[[providers]]` entries, or — for a
    /// legacy config with only `[provider]` — a single synthesized "default"
    /// entry so the UI still surfaces the configured provider.
    pub fn providers_list(&self) -> Vec<ProviderEntryConfig> {
        if !self.providers.is_empty() {
            return self.providers.clone();
        }
        // Legacy single-provider fallback.
        vec![ProviderEntryConfig {
            name: "default".to_string(),
            kind: None,
            api_key: self.provider.api_key.clone(),
            base_url: self.provider.base_url.clone(),
            model: if self.provider.model.is_empty() {
                None
            } else {
                Some(self.provider.model.clone())
            },
        }]
    }

    /// Add (or replace by name) a provider entry. Live-pool callers rebuild the
    /// affected entry after `save()`.
    pub fn add_provider(&mut self, entry: ProviderEntryConfig) {
        if let Some(pos) = self.providers.iter().position(|e| e.name == entry.name) {
            self.providers[pos] = entry;
        } else {
            self.providers.push(entry);
        }
    }

    /// Remove a provider entry by name. Returns true if removed.
    pub fn remove_provider(&mut self, name: &str) -> bool {
        let before = self.providers.len();
        self.providers.retain(|e| e.name != name);
        if self.active_provider.as_deref() == Some(name) {
            self.active_provider = None;
        }
        self.providers.len() != before
    }

    /// Mark a provider as active (the pool's launch default). No-op if unknown.
    pub fn set_active_provider(&mut self, name: &str) {
        if self.providers.iter().any(|e| e.name == name) {
            self.active_provider = Some(name.to_string());
        }
    }

    /// Build `(name, ModelConfig)` pairs for every entry — used by the
    /// app-server to construct a `ProviderPool` at launch. Env-var overrides
    /// (`ONEAI_API_KEY`/`ONEAI_BASE_URL`/`ONEAI_MODEL`) apply ONLY to a
    /// `[[providers]]` entry that has the matching field unset — so a fully-
    /// specified entry is self-contained, and a partial one inherits the env.
    pub fn to_pool_model_configs(&self) -> Vec<(String, oneai_core::ModelConfig)> {
        self.providers_list()
            .into_iter()
            .map(|e| (e.name.clone(), entry_to_model_config(&e)))
            .collect()
    }
}

/// Map a `ProviderEntryConfig` to a `ModelConfig`. `kind` resolves to a
/// `CloudProviderKind` (openai/anthropic/gemini) — unknown/absent ⇒ OpenAI-
/// compatible. Env vars fill in missing `api_key`/`base_url`/`model`.
fn entry_to_model_config(e: &ProviderEntryConfig) -> oneai_core::ModelConfig {
    use oneai_core::{CloudProviderKind, ModelConfig, ProviderType};
    let cloud_kind = e
        .kind
        .as_deref()
        .and_then(|k| match k.to_lowercase().as_str() {
            "openai" => Some(CloudProviderKind::OpenAI),
            "anthropic" => Some(CloudProviderKind::Anthropic),
            "gemini" => Some(CloudProviderKind::Gemini),
            _ => None,
        });
    let api_key = e
        .api_key
        .clone()
        .or_else(|| std::env::var("ONEAI_API_KEY").ok());
    let base_url = e
        .base_url
        .clone()
        .or_else(|| std::env::var("ONEAI_BASE_URL").ok());
    let model = e
        .model
        .clone()
        .or_else(|| std::env::var("ONEAI_MODEL").ok());
    ModelConfig {
        provider_type: ProviderType::Cloud,
        cloud_kind,
        api_key,
        base_url,
        port: None,
        model_name: model,
        model_path: None,
        extra: std::collections::HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_section_round_trips() {
        let toml_src = r#"
[embedding]
provider = "voyage"
api_key = "pa-test"
model = "voyage-3"
fallback = "openai"
"#;
        let cfg: OneaiConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(
            cfg.embedding.provider,
            oneai_core::EmbeddingProvider::Voyage
        );
        assert_eq!(cfg.embedding.api_key.as_deref(), Some("pa-test"));
        assert_eq!(cfg.embedding.model.as_ref().unwrap().as_str(), "voyage-3");
        assert_eq!(
            cfg.embedding.fallback,
            Some(oneai_core::EmbeddingProvider::OpenAi)
        );
        // re-serialize and parse back — stable round-trip
        let s = toml::to_string_pretty(&cfg).unwrap();
        let cfg2: OneaiConfig = toml::from_str(&s).unwrap();
        assert_eq!(
            cfg2.embedding.provider,
            oneai_core::EmbeddingProvider::Voyage
        );
    }

    #[test]
    fn missing_embedding_section_defaults_to_auto() {
        let cfg: OneaiConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.embedding.provider, oneai_core::EmbeddingProvider::Auto);
        assert!(cfg.embedding.api_key.is_none());
    }
}
