//! Embedding provider adapters + zero-config auto-detection resolver.
//!
//! Borrows OpenClaw's "declare intent, auto-resolve" philosophy:
//! - Each provider is an [`EmbeddingProviderAdapter`] carrying static metadata
//!   (`default_model`, `requires_api_key`, `auth_env_var`) + an
//!   [`available`][EmbeddingProviderAdapter::available] static probe that
//!   **never makes embedding API calls** — it only checks env keys and (for
//!   local providers) a short TCP reachability probe.
//! - [`EmbeddingResolver::resolve`] walks the auto-detection chain (embedding-
//!   specific keys, **never** reusing the LLM provider's key — embedding and
//!   chat are separate capabilities) and picks the first provider that is both
//!   available and constructible. If none is available, it returns `Ok(None)`
//!   so memory recall falls back to keyword matching instead of hard-failing.
//! - Build-time + runtime fallback share one `should_continue` classifier
//!   (rate-limit / 5xx / transport / missing-key → skip to next; other → raise).
//!
//! The runtime primary→fallback switch itself lives in
//! [`EmbeddingServiceRegistry`](crate::EmbeddingServiceRegistry); the resolver
//! just wires the registry's primary + optional fallback.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{EmbeddingProvider, EmbeddingService};
use oneai_core::{EmbeddingConfig, EmbeddingModel};

use crate::embedding::{
    EmbeddingServiceRegistry, FastEmbedService, OllamaEmbeddingService, OpenAIEmbeddingService,
    VoyageEmbeddingService,
};

// ─── Availability + EnvProbe ────────────────────────────────────────────────

/// Outcome of a static availability probe (no embedding API calls).
#[derive(Debug, Clone)]
pub enum Availability {
    /// The provider looks usable — attempt [`create`][EmbeddingProviderAdapter::create].
    Available,
    /// A required signal (usually an env key) is absent — skip, don't error.
    Missing(&'static str),
    /// The provider is not usable in this environment — skip, don't error.
    Unavailable(&'static str),
}

/// Read-only environment + reachability probe. Used by adapters' `available()`
/// to decide whether to even attempt `create()`. **Never** issues embedding
/// API requests — only reads env vars and does short TCP connects (cached).
#[derive(Debug, Clone, Default)]
pub struct EnvProbe {
    env: HashMap<String, String>,
}

impl EnvProbe {
    /// Build a probe from the live process environment.
    pub fn from_env() -> Self {
        Self {
            env: std::env::vars().collect(),
        }
    }

    /// Construct an empty probe (for tests).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Inject an env var (for tests).
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Remove an env var (for tests).
    pub fn without_env(mut self, name: &str) -> Self {
        self.env.remove(name);
        self
    }

    /// Read an env var's value (empty values are treated as absent).
    pub fn env_get(&self, name: &str) -> Option<&str> {
        self.env.get(name).filter(|v| !v.is_empty()).map(|v| v.as_str())
    }

    /// Whether a non-empty env var is present.
    pub fn env_key_present(&self, name: &str) -> bool {
        self.env_get(name).is_some()
    }

    /// Short-timeout (300ms) TCP reachability probe, process-cached by
    /// `(host, port)` so a cold build probes each endpoint at most once.
    pub fn tcp_reachable(&self, host: &str, port: u16) -> bool {
        use std::net::ToSocketAddrs;
        let key = (host.to_string(), port);
        {
            let cache = tcp_cache().lock().expect("tcp probe cache poisoned");
            if let Some(&hit) = cache.get(&key) {
                return hit;
            }
        }
        let reached = (host, port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .map(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(300)).is_ok())
            .unwrap_or(false);
        if let Ok(mut cache) = tcp_cache().lock() {
            cache.insert(key, reached);
        }
        reached
    }
}

fn tcp_cache() -> &'static Mutex<HashMap<(String, u16), bool>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, u16), bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

// ─── should_continue classifier ────────────────────────────────────────────

/// Whether a `create` / first-call failure is the kind we should skip past
/// (continue down the auto chain or try the fallback) rather than surface.
///
/// Matches OpenClaw's `manager-embedding-policy.ts` retryable classification:
/// rate-limit / 5xx / transport errors, plus missing-key / auth (so a bad key
/// for one provider lets the chain try the next). Other errors (e.g. dimension
/// mismatch, model-not-found on an explicitly chosen provider) surface.
pub fn should_continue(err: &OneAIError) -> bool {
    let msg = err.to_string().to_lowercase();
    const PATTERNS: &[&str] = &[
        // service-side retryable
        "429", "rate limit", "rate_limit", "rate-limit", "too many requests",
        "resource has been exhausted", "tokens per day", "quota",
        "500", "502", "503", "504", "505", "cloudflare",
        // transport
        "econnreset", "econnrefused", "etimedout", "epipe", "und_err",
        "socket hang up", "socket terminated", "connection reset",
        "connection refused", "connection aborted", "connection timed out",
        "ehostunreach", "enetunreach", "econnaborted", "eai_again",
        "timed out", "network error", "fetch failed",
        // auth / missing key → try the next provider instead of hard-failing
        "401", "403", "unauthorized", "forbidden", "api key", "apikey", "missing",
    ];
    PATTERNS.iter().any(|p| msg.contains(p))
}

// ─── EmbeddingProviderAdapter trait ─────────────────────────────────────────

/// One embedding provider's metadata + factory.
///
/// Adding a new provider = implement this trait + register it in
/// [`EmbeddingProviderRegistry::builtin`]. No core enum changes needed.
pub trait EmbeddingProviderAdapter: Send + Sync {
    /// Which provider this adapter handles.
    fn id(&self) -> EmbeddingProvider;

    /// Model used when `EmbeddingConfig.model` is `None`.
    fn default_model(&self) -> &str;

    /// Whether this provider needs an API key.
    fn requires_api_key(&self) -> bool;

    /// The env var carrying this provider's embedding key (`None` for keyless
    /// local providers). Used for auto-detection only; `create()` resolves the
    /// key itself from config-or-env.
    fn auth_env_var(&self) -> Option<&'static str>;

    /// Static availability probe (no embedding API calls).
    fn available(&self, probe: &EnvProbe, config: &EmbeddingConfig) -> Availability;

    /// Whether a `create`/first-call failure should skip to the next provider.
    fn should_continue_on(&self, err: &OneAIError) -> bool {
        should_continue(err)
    }

    /// Build the service. `config.api_key` is resolved by the caller
    /// (config → env fallback) before this is called.
    fn create(&self, config: &EmbeddingConfig, probe: &EnvProbe) -> Result<Arc<dyn EmbeddingService>>;
}

// Resolve the effective model: explicit override → adapter default.
fn resolve_model(config: &EmbeddingConfig, default: &str) -> EmbeddingModel {
    config
        .model
        .clone()
        .unwrap_or_else(|| EmbeddingModel::new(default))
}

// Resolve the effective api key: config → provider env var. Empty strings
// are treated as absent so an explicit `EmbeddingConfig::openai("".into())`
// (or a blank config field) still falls back / skips gracefully.
fn resolve_api_key(config: &EmbeddingConfig, env_var: &str, probe: &EnvProbe) -> Option<String> {
    config
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| probe.env_get(env_var).map(String::from))
}

/// Whether a non-empty api key is available for a key-requiring provider
/// (explicit config field, or the provider's env var).
fn key_available(config: &EmbeddingConfig, env_var: &str, probe: &EnvProbe) -> bool {
    config.api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false)
        || probe.env_key_present(env_var)
}

// ─── Adapters ───────────────────────────────────────────────────────────────

/// OpenAI official embedding API.
pub struct OpenAiAdapter;
/// Voyage embedding API (`api.voyageai.com`).
pub struct VoyageAdapter;
/// Ollama local embedding API.
pub struct OllamaAdapter;
/// FastEmbed local ONNX.
pub struct FastEmbedAdapter;
/// OpenAI-compatible relay/gateway (explicit base_url + key).
pub struct OpenAiCompatAdapter;

impl EmbeddingProviderAdapter for OpenAiAdapter {
    fn id(&self) -> EmbeddingProvider { EmbeddingProvider::OpenAi }
    fn default_model(&self) -> &str { "text-embedding-3-small" }
    fn requires_api_key(&self) -> bool { true }
    fn auth_env_var(&self) -> Option<&'static str> { Some("OPENAI_API_KEY") }
    fn available(&self, probe: &EnvProbe, config: &EmbeddingConfig) -> Availability {
        if key_available(config, "OPENAI_API_KEY", probe) {
            Availability::Available
        } else {
            Availability::Missing("OPENAI_API_KEY")
        }
    }
    fn create(&self, config: &EmbeddingConfig, probe: &EnvProbe) -> Result<Arc<dyn EmbeddingService>> {
        let key = resolve_api_key(config, "OPENAI_API_KEY", probe)
            .ok_or_else(|| OneAIError::Embedding("OpenAI embedding requires OPENAI_API_KEY".into()))?;
        let model = resolve_model(config, self.default_model());
        let base = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        Ok(Arc::new(OpenAIEmbeddingService::with_base_url(key, model, base)))
    }
}

impl EmbeddingProviderAdapter for VoyageAdapter {
    fn id(&self) -> EmbeddingProvider { EmbeddingProvider::Voyage }
    fn default_model(&self) -> &str { "voyage-3" }
    fn requires_api_key(&self) -> bool { true }
    fn auth_env_var(&self) -> Option<&'static str> { Some("VOYAGE_API_KEY") }
    fn available(&self, probe: &EnvProbe, config: &EmbeddingConfig) -> Availability {
        if key_available(config, "VOYAGE_API_KEY", probe) {
            Availability::Available
        } else {
            Availability::Missing("VOYAGE_API_KEY")
        }
    }
    fn create(&self, config: &EmbeddingConfig, probe: &EnvProbe) -> Result<Arc<dyn EmbeddingService>> {
        let key = resolve_api_key(config, "VOYAGE_API_KEY", probe)
            .ok_or_else(|| OneAIError::Embedding("Voyage embedding requires VOYAGE_API_KEY".into()))?;
        let model = resolve_model(config, self.default_model());
        let base = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.voyageai.com/v1".to_string());
        Ok(Arc::new(VoyageEmbeddingService::with_base_url(key, model, base)))
    }
}

impl EmbeddingProviderAdapter for OllamaAdapter {
    fn id(&self) -> EmbeddingProvider { EmbeddingProvider::Ollama }
    fn default_model(&self) -> &str { "nomic-embed-text" }
    fn requires_api_key(&self) -> bool { false }
    fn auth_env_var(&self) -> Option<&'static str> { None }
    fn available(&self, probe: &EnvProbe, config: &EmbeddingConfig) -> Availability {
        // base_url override → treat as configured (assume reachable); else TCP-probe default.
        if config.base_url.is_some() {
            return Availability::Available;
        }
        if probe.tcp_reachable("localhost", 11434) {
            Availability::Available
        } else {
            Availability::Unavailable("ollama not reachable at localhost:11434")
        }
    }
    fn create(&self, config: &EmbeddingConfig, _probe: &EnvProbe) -> Result<Arc<dyn EmbeddingService>> {
        let base = config
            .base_url
            .clone()
            .unwrap_or_else(|| "http://localhost:11434".to_string());
        let model = config
            .model
            .as_ref()
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| self.default_model().to_string());
        Ok(Arc::new(OllamaEmbeddingService::with_url_and_model(base, model)))
    }
}

impl EmbeddingProviderAdapter for FastEmbedAdapter {
    fn id(&self) -> EmbeddingProvider { EmbeddingProvider::FastEmbed }
    fn default_model(&self) -> &str { "all-MiniLM-L6-v2" }
    fn requires_api_key(&self) -> bool { false }
    fn auth_env_var(&self) -> Option<&'static str> { None }
    fn available(&self, _probe: &EnvProbe, _config: &EmbeddingConfig) -> Availability {
        // FastEmbed local ONNX: no key, offline-capable after a one-time model
        // download. It is the auto-chain's last-resort so users with no embedding
        // key / no local Ollama still get real semantic recall. The one-time
        // download happens lazily on first `embed()`; if it fails (offline at
        // first use), `MemoryManager`'s fail-safe catches it → keyword recall.
        Availability::Available
    }
    fn create(&self, config: &EmbeddingConfig, _probe: &EnvProbe) -> Result<Arc<dyn EmbeddingService>> {
        let model = resolve_model(config, self.default_model());
        Ok(Arc::new(FastEmbedService::with_model(model)))
    }
}

impl EmbeddingProviderAdapter for OpenAiCompatAdapter {
    fn id(&self) -> EmbeddingProvider { EmbeddingProvider::OpenAiCompat }
    fn default_model(&self) -> &str { "text-embedding-3-small" }
    fn requires_api_key(&self) -> bool { true }
    fn auth_env_var(&self) -> Option<&'static str> { Some("ONEAI_EMBEDDING_API_KEY") }
    fn available(&self, probe: &EnvProbe, config: &EmbeddingConfig) -> Availability {
        let has_key = key_available(config, "ONEAI_EMBEDDING_API_KEY", probe);
        let has_base = config.base_url.as_deref().map(|b| !b.is_empty()).unwrap_or(false)
            || probe.env_key_present("ONEAI_EMBEDDING_BASE_URL");
        match (has_key, has_base) {
            (true, true) => Availability::Available,
            (false, _) => Availability::Missing("ONEAI_EMBEDDING_API_KEY"),
            (_, false) => Availability::Missing("ONEAI_EMBEDDING_BASE_URL"),
        }
    }
    fn create(&self, config: &EmbeddingConfig, probe: &EnvProbe) -> Result<Arc<dyn EmbeddingService>> {
        let key = resolve_api_key(config, "ONEAI_EMBEDDING_API_KEY", probe).ok_or_else(|| {
            OneAIError::Embedding("openai-compat embedding requires ONEAI_EMBEDDING_API_KEY".into())
        })?;
        let base = config
            .base_url
            .clone()
            .or_else(|| probe.env_get("ONEAI_EMBEDDING_BASE_URL").map(String::from))
            .ok_or_else(|| OneAIError::Embedding("openai-compat embedding requires base_url".into()))?;
        let model = resolve_model(config, self.default_model());
        Ok(Arc::new(OpenAIEmbeddingService::with_base_url(key, model, base)))
    }
}

// ─── EmbeddingProviderRegistry ──────────────────────────────────────────────

/// Registry of built-in embedding provider adapters.
pub struct EmbeddingProviderRegistry {
    adapters: HashMap<EmbeddingProvider, Box<dyn EmbeddingProviderAdapter>>,
}

impl EmbeddingProviderRegistry {
    /// The built-in adapter set (OpenAI / Voyage / Ollama / FastEmbed / OpenAI-compat).
    pub fn builtin() -> Self {
        let mut adapters: HashMap<EmbeddingProvider, Box<dyn EmbeddingProviderAdapter>> = HashMap::new();
        adapters.insert(EmbeddingProvider::OpenAi, Box::new(OpenAiAdapter));
        adapters.insert(EmbeddingProvider::Voyage, Box::new(VoyageAdapter));
        adapters.insert(EmbeddingProvider::Ollama, Box::new(OllamaAdapter));
        adapters.insert(EmbeddingProvider::FastEmbed, Box::new(FastEmbedAdapter));
        adapters.insert(EmbeddingProvider::OpenAiCompat, Box::new(OpenAiCompatAdapter));
        Self { adapters }
    }

    /// Look up an adapter by provider id.
    pub fn get(&self, provider: EmbeddingProvider) -> Option<&dyn EmbeddingProviderAdapter> {
        self.adapters.get(&provider).map(|b| b.as_ref())
    }
}

/// Process-wide built-in registry.
fn builtin_registry() -> &'static EmbeddingProviderRegistry {
    static REG: OnceLock<EmbeddingProviderRegistry> = OnceLock::new();
    REG.get_or_init(EmbeddingProviderRegistry::builtin)
}

// ─── EmbeddingResolver ──────────────────────────────────────────────────────

/// The auto-detection chain order (first available+constructible wins).
///
/// Order rationale: explicit embedding relay > Voyage (independent key) >
/// OpenAI official (key may double as chat key, so ranked after Voyage) >
/// Ollama (local) > FastEmbed (offline, when implemented).
const AUTO_CHAIN: &[EmbeddingProvider] = &[
    EmbeddingProvider::OpenAiCompat,
    EmbeddingProvider::Voyage,
    EmbeddingProvider::OpenAi,
    EmbeddingProvider::Ollama,
    EmbeddingProvider::FastEmbed,
];

/// Zero-config embedding resolver: turns an [`EmbeddingConfig`] into a
/// fallback-aware [`EmbeddingServiceRegistry`] (or `None`).
#[derive(Debug, Clone)]
pub struct EmbeddingResolver;

impl EmbeddingResolver {
    /// Resolve a config against the live process environment.
    pub fn resolve(config: &EmbeddingConfig) -> Result<Option<EmbeddingServiceRegistry>> {
        Self::resolve_with(config, &EnvProbe::from_env())
    }

    /// Resolve a config against an explicit probe (testable).
    pub fn resolve_with(config: &EmbeddingConfig, probe: &EnvProbe) -> Result<Option<EmbeddingServiceRegistry>> {
        let reg = builtin_registry();
        let (primary, fallback) = if config.provider == EmbeddingProvider::Auto {
            Self::resolve_auto(config, probe, reg)?
        } else {
            Self::resolve_explicit(config, probe, reg)?
        };
        match (primary, fallback) {
            (None, None) => {
                tracing::info!("no embedding provider available; memory recall falls back to keyword matching");
                Ok(None)
            }
            (Some(p), None) => Ok(Some(EmbeddingServiceRegistry::new(p))),
            (None, Some(f)) => Ok(Some(EmbeddingServiceRegistry::new(f))),
            (Some(p), Some(f)) => Ok(Some(EmbeddingServiceRegistry::new(p).with_fallback(f))),
        }
    }

    /// Walk the auto chain; return (primary, next-available fallback).
    fn resolve_auto(
        config: &EmbeddingConfig,
        probe: &EnvProbe,
        reg: &EmbeddingProviderRegistry,
    ) -> Result<(Option<Arc<dyn EmbeddingService>>, Option<Arc<dyn EmbeddingService>>)> {
        let mut created: Vec<Arc<dyn EmbeddingService>> = Vec::new();
        for &p in AUTO_CHAIN {
            let adapter = match reg.get(p) {
                Some(a) => a,
                None => continue,
            };
            match adapter.available(probe, config) {
                Availability::Available => match adapter.create(config, probe) {
                    Ok(svc) => {
                        tracing::info!(provider = %p, model = ?config.model, "auto-detected embedding provider");
                        created.push(svc);
                        if created.len() >= 2 {
                            break;
                        }
                    }
                    Err(e) if adapter.should_continue_on(&e) => {
                        tracing::warn!(provider = %p, error = %e, "embedding provider create failed, continuing auto chain");
                        continue;
                    }
                    Err(e) => return Err(e),
                },
                Availability::Missing(reason) | Availability::Unavailable(reason) => {
                    tracing::debug!(provider = %p, reason, "embedding provider skipped in auto chain");
                }
            }
        }
        let mut iter = created.into_iter();
        Ok((iter.next(), iter.next()))
    }

    /// Explicit provider (+ optional explicit fallback).
    fn resolve_explicit(
        config: &EmbeddingConfig,
        probe: &EnvProbe,
        reg: &EmbeddingProviderRegistry,
    ) -> Result<(Option<Arc<dyn EmbeddingService>>, Option<Arc<dyn EmbeddingService>>)> {
        let primary = Self::resolve_one(config, config.provider, probe, reg)?;
        let fallback = match config.fallback {
            Some(fb) if fb != config.provider => Self::resolve_one(config, fb, probe, reg)?,
            _ => None,
        };
        Ok((primary, fallback))
    }

    fn resolve_one(
        config: &EmbeddingConfig,
        provider: EmbeddingProvider,
        probe: &EnvProbe,
        reg: &EmbeddingProviderRegistry,
    ) -> Result<Option<Arc<dyn EmbeddingService>>> {
        let adapter = reg.get(provider).ok_or_else(|| {
            OneAIError::Embedding(format!("unknown embedding provider: {provider}"))
        })?;
        match adapter.available(probe, config) {
            Availability::Available => match adapter.create(config, probe) {
                Ok(svc) => Ok(Some(svc)),
                Err(e) if adapter.should_continue_on(&e) => {
                    tracing::warn!(provider = %provider, error = %e, "embedding provider unavailable, recall falls back to keyword matching");
                    Ok(None)
                }
                Err(e) => Err(e),
            },
            Availability::Missing(reason) | Availability::Unavailable(reason) => {
                tracing::warn!(provider = %provider, reason, "embedding provider unavailable, recall falls back to keyword matching");
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_available_with_env_key() {
        let probe = EnvProbe::empty().with_env("OPENAI_API_KEY", "sk-x");
        assert!(matches!(OpenAiAdapter.available(&probe, &EmbeddingConfig::default()), Availability::Available));
    }

    #[test]
    fn openai_missing_without_key() {
        let probe = EnvProbe::empty();
        assert!(matches!(OpenAiAdapter.available(&probe, &EmbeddingConfig::default()), Availability::Missing(_)));
    }

    #[test]
    fn voyage_uses_voyage_key_not_anthropic() {
        // ANTHROPIC_API_KEY must NOT satisfy the voyage adapter — embedding keys are independent of LLM keys.
        let probe = EnvProbe::empty().with_env("ANTHROPIC_API_KEY", "sk-ant");
        assert!(matches!(VoyageAdapter.available(&probe, &EmbeddingConfig::default()), Availability::Missing(_)));
        let probe = probe.with_env("VOYAGE_API_KEY", "pa-xxx");
        assert!(matches!(VoyageAdapter.available(&probe, &EmbeddingConfig::default()), Availability::Available));
    }

    #[test]
    fn openai_compat_needs_key_and_base() {
        let probe = EnvProbe::empty()
            .with_env("ONEAI_EMBEDDING_API_KEY", "k")
            .with_env("ONEAI_EMBEDDING_BASE_URL", "https://relay/v1");
        assert!(matches!(OpenAiCompatAdapter.available(&probe, &EmbeddingConfig::default()), Availability::Available));
        // missing base url → missing
        let probe = EnvProbe::empty().with_env("ONEAI_EMBEDDING_API_KEY", "k");
        assert!(matches!(OpenAiCompatAdapter.available(&probe, &EmbeddingConfig::default()), Availability::Missing(_)));
    }

    #[test]
    fn fastembed_included_in_auto() {
        // FastEmbed (local ONNX) is keyless + offline-capable after a one-time
        // download, so it is the auto-chain's last-resort: no-key users still
        // get real semantic recall rather than keyword matching.
        let probe = EnvProbe::empty();
        assert!(matches!(FastEmbedAdapter.available(&probe, &EmbeddingConfig::default()), Availability::Available));
    }

    #[test]
    fn auto_falls_back_to_fastembed_when_no_keys() {
        // Auto with no keys/ollama → FastEmbed (real local ONNX), NOT None.
        let probe = EnvProbe::empty();
        let resolved = EmbeddingResolver::resolve_with(&EmbeddingConfig::default(), &probe).unwrap();
        let reg = resolved.expect("auto should resolve to FastEmbed as last resort");
        assert_eq!(reg.model().as_str(), "all-MiniLM-L6-v2");
    }

    #[test]
    fn auto_picks_voyage_over_openai() {
        // both keys present → voyage earlier in chain than openai
        let probe = EnvProbe::empty()
            .with_env("VOYAGE_API_KEY", "pa")
            .with_env("OPENAI_API_KEY", "sk");
        let resolved = EmbeddingResolver::resolve_with(&EmbeddingConfig::default(), &probe).unwrap();
        let reg = resolved.expect("registry should exist");
        assert_eq!(reg.model().as_str(), "voyage-3", "voyage precedes openai in the auto chain");
    }

    #[test]
    fn explicit_openai_without_key_returns_none_gracefully() {
        // explicit provider but no key → Ok(None) (keyword recall), NOT a hard error
        let probe = EnvProbe::empty();
        let cfg = EmbeddingConfig::openai("".to_string()); // empty key treated absent downstream
        let resolved = EmbeddingResolver::resolve_with(&cfg, &probe).unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_explicit_openai_with_key() {
        let cfg = EmbeddingConfig::openai("sk-test".to_string());
        let resolved = EmbeddingResolver::resolve_with(&cfg, &EnvProbe::empty()).unwrap();
        let reg = resolved.expect("registry should exist");
        assert_eq!(reg.model().as_str(), "text-embedding-3-small");
    }

    #[test]
    fn should_continue_classifies_rate_limit_and_transport() {
        let e = OneAIError::Embedding("OpenAI embedding API error: status 429 — rate limit".into());
        assert!(should_continue(&e));
        let e = OneAIError::Embedding("HTTP error: error sending request: connection reset".into());
        assert!(should_continue(&e));
    }

    #[test]
    fn should_not_continue_on_dimension_mismatch() {
        let e = OneAIError::Embedding("dimension mismatch: expected 1536 got 768".into());
        assert!(!should_continue(&e));
    }
}
