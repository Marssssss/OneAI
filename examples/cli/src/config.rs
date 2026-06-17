//! Configuration management for OneAI CLI.
//!
//! Reads configuration from `~/.oneai/config.toml`, with fallback to
//! environment variables and defaults. Priority order:
//!   CLI arguments > environment variables > config.toml > defaults

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

/// Full OneAI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OneaiConfig {
    /// LLM provider configuration.
    #[serde(default)]
    pub provider: ProviderConfig,
    /// Domain configuration.
    #[serde(default)]
    pub domain: DomainConfig,
    /// UI configuration.
    #[serde(default)]
    pub ui: UiConfig,
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
            domain: DomainConfig::default(),
            ui: UiConfig::default(),
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
                eprintln!("Warning: Failed to load config from {}: {}", path.display(), e);
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
        let api_key = std::env::var("ONEAI_API_KEY").ok().or(self.provider.api_key.clone());
        let base_url = std::env::var("ONEAI_BASE_URL").ok().or(self.provider.base_url.clone());
        let model = std::env::var("ONEAI_MODEL").ok().unwrap_or(self.provider.model.clone());

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
        domain_override.map(|s| s.to_string())
            .unwrap_or_else(|| self.domain.default_pack.clone())
    }
}
