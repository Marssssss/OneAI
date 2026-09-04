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
    /// Permission-decision audit log path (gap-analysis P1 #9). When set
    /// (e.g. `~/.oneai/permission-audit.jsonl`), every terminal tool
    /// permission decision (policy deny / auto-approve, Guardian verdict,
    /// gate approve/abort/revise, direct execution) is appended as one JSON
    /// line. Empty / absent → no audit trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_audit_log: Option<String>,
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
            permission_audit_log: None,
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

    /// Build the permission-audit sink from `permission_audit_log` (gap P1
    /// #9). `~` expands to the home dir. Returns `None` when unset/empty, or
    /// when the file cannot be opened (warned on stderr — auditing must
    /// never break startup).
    pub fn permission_audit_log_sink(
        &self,
    ) -> Option<std::sync::Arc<dyn oneai_core::audit::PermissionAuditLog>> {
        let raw = self.permission_audit_log.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        let path = if let Some(rest) = raw.strip_prefix("~/") {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(rest)
        } else if raw == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
        } else {
            PathBuf::from(raw)
        };
        match oneai_core::audit::JsonlAuditLog::new(&path) {
            Ok(log) => Some(std::sync::Arc::new(log)),
            Err(e) => {
                eprintln!(
                    "Warning: cannot open permission audit log {}: {} — continuing without audit",
                    path.display(),
                    e
                );
                None
            }
        }
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
    ///
    /// Writes atomically (temp file + `rename`) so a crash or a concurrent
    /// process can never observe a half-written `config.toml`. This matters
    /// because several OneAI processes share this file (the `oneai web`
    /// server, the macOS/Windows sidecars, and `provider/*` RPC writers); a
    /// torn `fs::write` corrupts the TOML, and the next `load_or_default`
    /// then silently discards every configured provider.
    pub fn save(&self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = Self::default_path();
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)?;
        let content = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)?;
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

    /// Set the default domain pack name (persisted to `domain.default_pack`).
    /// Mirror of `set_active_provider` — the launch default for `--domain`.
    pub fn set_domain(&mut self, name: &str) {
        self.domain.default_pack = name.to_string();
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

/// Resolve a `kind` string to `(provider_type, cloud_kind)` — the shared
/// mapping behind both the launch pool build (`entry_to_model_config`) and
/// the live `provider/add`·`provider/models` paths
/// (`cmd_app_server::entry_to_model_config_strict`).
///
/// openai/anthropic/gemini set the cloud kind explicitly; `ollama` (or
/// `local`) forces `ProviderType::Local` so the factory picks the Ollama
/// family regardless of host (a remote Ollama box must NOT auto-detect as
/// OpenAI-compatible — issue #37). Absent/unknown kind leaves the cloud kind
/// unset so `ProviderFactory::resolve_provider` still auto-detects from the
/// base-URL host (legacy behavior for hand-edited configs).
pub fn resolve_provider_kind(
    kind: Option<&str>,
) -> (
    oneai_core::ProviderType,
    Option<oneai_core::CloudProviderKind>,
) {
    use oneai_core::{CloudProviderKind, ProviderType};
    match kind.map(|k| k.trim().to_lowercase()).as_deref() {
        Some("openai") => (ProviderType::Cloud, Some(CloudProviderKind::OpenAI)),
        Some("anthropic") => (ProviderType::Cloud, Some(CloudProviderKind::Anthropic)),
        Some("gemini") => (ProviderType::Cloud, Some(CloudProviderKind::Gemini)),
        Some("ollama") | Some("local") => (ProviderType::Local, None),
        _ => (ProviderType::Cloud, None),
    }
}

/// The canonical endpoint per provider family — fills a missing `base_url`
/// (issue #37: an entry with only kind + api_key must still work; the
/// factory's `resolved_url()` returns "" when base_url/port are both unset,
/// which is a dead entry). Matches the `ModelConfig::openai/anthropic/
/// gemini/ollama` constructors.
pub fn default_base_url_for(
    provider_type: oneai_core::ProviderType,
    cloud_kind: Option<oneai_core::CloudProviderKind>,
) -> Option<&'static str> {
    use oneai_core::{CloudProviderKind, ProviderType};
    match cloud_kind {
        Some(CloudProviderKind::OpenAI) => Some("https://api.openai.com/v1"),
        Some(CloudProviderKind::Anthropic) => Some("https://api.anthropic.com/v1"),
        Some(CloudProviderKind::Gemini) => Some("https://generativelanguage.googleapis.com/v1beta"),
        None => match provider_type {
            ProviderType::Local => Some("http://localhost:11434"),
            _ => None, // unknown family — host auto-detect needs a URL anyway
        },
    }
}

/// Map a `ProviderEntryConfig` to a `ModelConfig`. `kind` resolves via
/// [`resolve_provider_kind`]. Env vars fill in missing `api_key`/`base_url`/
/// `model`; a still-missing `base_url` falls back to the family's canonical
/// endpoint ([`default_base_url_for`]).
fn entry_to_model_config(e: &ProviderEntryConfig) -> oneai_core::ModelConfig {
    use oneai_core::ModelConfig;
    let (provider_type, cloud_kind) = resolve_provider_kind(e.kind.as_deref());
    let api_key = e
        .api_key
        .clone()
        .or_else(|| std::env::var("ONEAI_API_KEY").ok());
    let base_url = e
        .base_url
        .clone()
        .or_else(|| std::env::var("ONEAI_BASE_URL").ok())
        .or_else(|| default_base_url_for(provider_type, cloud_kind).map(|s| s.to_string()));
    let model = e
        .model
        .clone()
        .or_else(|| std::env::var("ONEAI_MODEL").ok());
    ModelConfig {
        provider_type,
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

    // ── issue #37: kind resolution + base_url defaults ─────────────────────

    #[test]
    fn resolve_kind_maps_known_families() {
        use oneai_core::{CloudProviderKind, ProviderType};
        assert_eq!(
            resolve_provider_kind(Some("openai")),
            (ProviderType::Cloud, Some(CloudProviderKind::OpenAI))
        );
        assert_eq!(
            resolve_provider_kind(Some("Anthropic")),
            (ProviderType::Cloud, Some(CloudProviderKind::Anthropic))
        );
        assert_eq!(
            resolve_provider_kind(Some("gemini")),
            (ProviderType::Cloud, Some(CloudProviderKind::Gemini))
        );
        // ollama forces Local regardless of host (remote Ollama must not
        // auto-detect as OpenAI-compatible).
        assert_eq!(
            resolve_provider_kind(Some("ollama")),
            (ProviderType::Local, None)
        );
        assert_eq!(
            resolve_provider_kind(Some("local")),
            (ProviderType::Local, None)
        );
        assert_eq!(
            resolve_provider_kind(Some(" ollama ")),
            (ProviderType::Local, None)
        );
        // Unknown/absent → Cloud + auto-detect from host.
        assert_eq!(resolve_provider_kind(None), (ProviderType::Cloud, None));
        assert_eq!(
            resolve_provider_kind(Some("mystery")),
            (ProviderType::Cloud, None)
        );
    }

    #[test]
    fn default_base_url_per_family() {
        use oneai_core::{CloudProviderKind, ProviderType};
        assert_eq!(
            default_base_url_for(ProviderType::Cloud, Some(CloudProviderKind::OpenAI)),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            default_base_url_for(ProviderType::Cloud, Some(CloudProviderKind::Anthropic)),
            Some("https://api.anthropic.com/v1")
        );
        assert_eq!(
            default_base_url_for(ProviderType::Cloud, Some(CloudProviderKind::Gemini)),
            Some("https://generativelanguage.googleapis.com/v1beta")
        );
        assert_eq!(
            default_base_url_for(ProviderType::Local, None),
            Some("http://localhost:11434")
        );
        assert_eq!(default_base_url_for(ProviderType::Cloud, None), None);
    }

    #[test]
    fn entry_config_kind_and_default_base_url_flow_into_model_config() {
        use oneai_core::{CloudProviderKind, ProviderType};
        // ollama entry without base_url → Local + Ollama's canonical endpoint
        // (env vars must not leak into this test).
        std::env::remove_var("ONEAI_BASE_URL");
        std::env::remove_var("ONEAI_API_KEY");
        std::env::remove_var("ONEAI_MODEL");
        let mc = entry_to_model_config(&ProviderEntryConfig {
            name: "local-box".into(),
            kind: Some("ollama".into()),
            api_key: None,
            base_url: None,
            model: Some("qwen2.5:7b".into()),
        });
        assert_eq!(mc.provider_type, ProviderType::Local);
        assert_eq!(mc.cloud_kind, None);
        assert_eq!(mc.base_url.as_deref(), Some("http://localhost:11434"));
        // An explicit base_url always wins.
        let mc = entry_to_model_config(&ProviderEntryConfig {
            name: "remote-box".into(),
            kind: Some("ollama".into()),
            api_key: None,
            base_url: Some("http://192.168.1.10:11434".into()),
            model: None,
        });
        assert_eq!(mc.provider_type, ProviderType::Local);
        assert_eq!(mc.base_url.as_deref(), Some("http://192.168.1.10:11434"));
        // kind=openai without base_url → canonical OpenAI endpoint.
        let mc = entry_to_model_config(&ProviderEntryConfig {
            name: "oai".into(),
            kind: Some("openai".into()),
            api_key: Some("sk".into()),
            base_url: None,
            model: None,
        });
        assert_eq!(mc.cloud_kind, Some(CloudProviderKind::OpenAI));
        assert_eq!(mc.base_url.as_deref(), Some("https://api.openai.com/v1"));
    }

    // ── config.toml round-trip integrity ────────────────────────────────────
    // `provider/add` · `/delete` · `/set_active` all `save()` the whole config.
    // A save that emits un-parseable TOML silently wipes every provider on the
    // next load (load_or_default → Default), so the round-trip MUST be lossless.

    #[test]
    fn default_config_round_trips_through_pretty_toml() {
        let s = toml::to_string_pretty(&OneaiConfig::default()).unwrap();
        let _: OneaiConfig = toml::from_str(&s).unwrap_or_else(|e| {
            panic!("default config re-parse failed: {e}\n--- emitted ---\n{s}")
        });
    }

    #[test]
    fn populated_config_round_trips_through_pretty_toml() {
        let mut cfg = OneaiConfig::default();
        cfg.add_provider(ProviderEntryConfig {
            name: "bailian".into(),
            kind: Some("openai".into()),
            api_key: Some("sk-x".into()),
            base_url: Some("https://example.com/v1".into()),
            model: Some("qwen3-max".into()),
        });
        cfg.add_provider(ProviderEntryConfig {
            name: "local".into(),
            kind: Some("ollama".into()),
            api_key: None,
            base_url: None,
            model: None,
        });
        cfg.set_active_provider("bailian");
        // Exercise the exact add → set_active → delete → save sequence the
        // app-server probe runs.
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: OneaiConfig = toml::from_str(&s).unwrap_or_else(|e| {
            panic!("populated config re-parse failed: {e}\n--- emitted ---\n{s}")
        });
        assert_eq!(back.providers.len(), 2);
        assert_eq!(back.active_provider.as_deref(), Some("bailian"));
    }
}
