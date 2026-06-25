//! Model context window resolution — 3-layer fallback (mirrors opencode's design).
//!
//! Resolves a model's context-window size with a strict priority order:
//!
//! 1. **User manual config (L1)** — `ONEAI_CONTEXT_WINDOW` env var (global),
//!    per-model profiles (`ContextManagerConfig.profiles` /
//!    `HeuristicTokenCounter::add_profile`), and per-provider-model overrides
//!    (`ModelConfig.extra["context_window"]`).
//! 2. **Provider API dynamic probe (L2)** — `LlmProvider::probe_context_window()`
//!    queries the provider's own metadata endpoint (Ollama `/api/show`,
//!    Anthropic `/v1/models/{id}`, Gemini `models.get`, OpenAI-compat best-effort).
//! 3. **Built-in static model library (L3)** — `BUILTIN_MODEL_CONTEXT`, an
//!    expanded table of known models; if still unknown, falls back to the
//!    existing name-pattern heuristic `infer_context_window_for_tokenizer`.
//!
//! Two resolution paths:
//! - `resolve_cached` (sync): L1 → probe-cache → L3. **Never issues network
//!   requests** — safe inside the sync `TokenCounter::context_window_size`.
//! - `resolve` (async): L1 → live L2 probe (writes cache) → L3. Used by the
//!   async agent-loop trim path and the CLI `token probe` subcommand.
//!
//! This mirrors opencode's `BUILTIN_MODEL_CONTEXT` + 3-layer resolution while
//! fitting OneAI's sync `TokenCounter` trait contract: probing is opt-in at
//! warm-up, and the sync path only reads cached probe results.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::traits::LlmProvider;
use crate::token_counter::infer_context_window_for_tokenizer;
// ─── ModelContextEntry + BUILTIN_MODEL_CONTEXT ───────────────────────────────

/// A single entry in the built-in static model library.
///
/// Mirrors the shape of opencode's `BUILTIN_MODEL_CONTEXT` records: a
/// provider family, a model-id pattern (matched as a case-insensitive
/// substring), and the context-window / max-output token limits.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelContextEntry {
    /// Provider family — "anthropic" | "openai" | "gemini" | "ollama" | "glm" | ...
    pub provider: &'static str,
    /// Model-id pattern, matched as a case-insensitive substring of the model
    /// name (e.g. "claude-opus", "gpt-4.1-nano", "glm-5"). Order matters: more
    /// specific patterns must appear before shorter prefixes in the table.
    pub model_id: &'static str,
    /// Maximum context window size in tokens (input + output combined).
    pub context_window: u32,
    /// Maximum output tokens the model can produce in one response.
    pub max_output_tokens: u32,
}

/// The built-in static model library — L3 fallback for context-window resolution.
///
/// Searched in order by `builtin_lookup`; the first substring match wins, so
/// entries are arranged specific → general within each family. Values reflect
/// publicly documented limits as of mid-2026; for the authoritative number,
/// the L2 provider probe (or L1 user override) takes precedence.
pub static BUILTIN_MODEL_CONTEXT: &[ModelContextEntry] = &[
    // ── Anthropic ───────────────────────────────────────────────────────────
    ModelContextEntry { provider: "anthropic", model_id: "claude-opus",      context_window: 200_000, max_output_tokens: 32_000 },
    ModelContextEntry { provider: "anthropic", model_id: "claude-sonnet",    context_window: 200_000, max_output_tokens: 16_000 },
    ModelContextEntry { provider: "anthropic", model_id: "claude-haiku",     context_window: 200_000, max_output_tokens: 8_192 },
    // ── OpenAI ──────────────────────────────────────────────────────────────
    ModelContextEntry { provider: "openai", model_id: "gpt-4.1-nano",   context_window: 1_000_000, max_output_tokens: 32_000 },
    ModelContextEntry { provider: "openai", model_id: "gpt-4.1-mini",   context_window: 1_000_000, max_output_tokens: 32_000 },
    ModelContextEntry { provider: "openai", model_id: "gpt-4.1",        context_window: 1_000_000, max_output_tokens: 32_000 },
    ModelContextEntry { provider: "openai", model_id: "gpt-4o-mini",    context_window: 128_000,   max_output_tokens: 16_384 },
    ModelContextEntry { provider: "openai", model_id: "gpt-4o",         context_window: 128_000,   max_output_tokens: 16_384 },
    ModelContextEntry { provider: "openai", model_id: "o4-mini",        context_window: 200_000,   max_output_tokens: 100_000 },
    ModelContextEntry { provider: "openai", model_id: "o3-pro",         context_window: 200_000,   max_output_tokens: 100_000 },
    ModelContextEntry { provider: "openai", model_id: "o3-mini",        context_window: 200_000,   max_output_tokens: 100_000 },
    ModelContextEntry { provider: "openai", model_id: "o3",             context_window: 200_000,   max_output_tokens: 100_000 },
    // ── Google Gemini ───────────────────────────────────────────────────────
    ModelContextEntry { provider: "gemini", model_id: "gemini-2.5-pro",    context_window: 2_000_000, max_output_tokens: 8_192 },
    ModelContextEntry { provider: "gemini", model_id: "gemini-2.5-flash",  context_window: 1_000_000, max_output_tokens: 8_192 },
    ModelContextEntry { provider: "gemini", model_id: "gemini-2.0-flash",  context_window: 1_000_000, max_output_tokens: 8_192 },
    // ── GLM (智谱) ──────────────────────────────────────────────────────────
    ModelContextEntry { provider: "glm", model_id: "glm-5",  context_window: 203_000, max_output_tokens: 16_384 },
    ModelContextEntry { provider: "glm", model_id: "glm-4",  context_window: 128_000, max_output_tokens: 4_096 },
    // ── DeepSeek ────────────────────────────────────────────────────────────
    ModelContextEntry { provider: "deepseek", model_id: "deepseek-reasoner", context_window: 64_000, max_output_tokens: 32_000 },
    ModelContextEntry { provider: "deepseek", model_id: "deepseek-chat",     context_window: 128_000, max_output_tokens: 8_192 },
    // ── Qwen ────────────────────────────────────────────────────────────────
    ModelContextEntry { provider: "qwen", model_id: "qwen3",   context_window: 128_000, max_output_tokens: 8_192 },
    ModelContextEntry { provider: "qwen", model_id: "qwen2.5", context_window: 128_000, max_output_tokens: 8_192 },
    // ── Llama (via Ollama) ──────────────────────────────────────────────────
    ModelContextEntry { provider: "llama", model_id: "llama3.3", context_window: 128_000, max_output_tokens: 4_096 },
    ModelContextEntry { provider: "llama", model_id: "llama3.1", context_window: 128_000, max_output_tokens: 4_096 },
    ModelContextEntry { provider: "llama", model_id: "llama3",   context_window: 8_192,   max_output_tokens: 4_096 },
];

/// Look up a model in the built-in static library by case-insensitive substring match.
///
/// Returns the first `BUILTIN_MODEL_CONTEXT` entry whose `model_id` is a
/// substring of `model` (lowercased). The table is ordered specific → general
/// so e.g. `"gpt-4.1-nano"` matches before `"gpt-4.1"` before `"gpt-4"`.
pub fn builtin_lookup(model: &str) -> Option<ModelContextEntry> {
    let lower = model.to_lowercase();
    BUILTIN_MODEL_CONTEXT
        .iter()
        .find(|entry| lower.contains(entry.model_id))
        .copied()
}

// ─── ContextSource ───────────────────────────────────────────────────────────

/// Which layer supplied the resolved context-window value.
///
/// Surfaced by `resolve_with_source` so the CLI can show "this number came
/// from the provider API" vs "from the built-in library" vs "user override".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContextSource {
    /// L1: `ONEAI_CONTEXT_WINDOW` env var (global override).
    UserEnv,
    /// L1: per-model profile from `ContextManagerConfig.profiles` / `add_profile`.
    UserProfile,
    /// L1: per-provider-model override via `ModelConfig.extra["context_window"]`.
    UserProviderExtra,
    /// L2: live provider API probe (cached after first resolution).
    ProviderApi,
    /// L3: `BUILTIN_MODEL_CONTEXT` static library.
    BuiltinLibrary,
    /// L3 (final fallback): name-pattern heuristic.
    NameHeuristic,
}

impl ContextSource {
    /// Human-readable label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::UserEnv => "user (env ONEAI_CONTEXT_WINDOW)",
            Self::UserProfile => "user (model profile)",
            Self::UserProviderExtra => "user (ModelConfig.extra)",
            Self::ProviderApi => "provider API probe",
            Self::BuiltinLibrary => "built-in static library",
            Self::NameHeuristic => "name-pattern heuristic",
        }
    }
}

// ─── ModelContextResolver ────────────────────────────────────────────────────

/// The 3-layer context-window resolver — single source of truth.
///
/// Holds the L1 user-override maps and the L2 probe cache. The sync
/// `resolve_cached` path consults L1 → cache → L3 and **never issues network
/// IO**; L2 live probing is driven by callers that already hold an
/// `LlmProvider` (the async agent-loop trim path, `AppSession::warm_model_context`,
/// and the CLI `token probe` subcommand) via `resolve_with_provider`, which
/// writes results into the shared cache for the sync path to read.
///
/// Constructed at `AppBuilder` time from user profiles + per-provider `extra`
/// overrides and attached to the `HeuristicTokenCounter` / `ContextManager`.
#[derive(Debug)]
pub struct ModelContextResolver {
    /// L1: per-model profiles (keyed by exact model name). Immutable after
    /// construction — seeded from `ContextManagerConfig.profiles` at build time.
    user_profiles: HashMap<String, u32>,
    /// L1: per-provider-model overrides from `ModelConfig.extra["context_window"]`.
    /// Interior-mutable so it can be seeded after the provider is resolved
    /// (the provider's config is only available once the provider exists).
    provider_extras: RwLock<HashMap<String, u32>>,
    /// L2: cache of live provider-probed window sizes.
    probe_cache: RwLock<HashMap<String, u32>>,
}

impl ModelContextResolver {
    /// Create a new resolver with the given L1 user-profile maps.
    pub fn new(
        user_profiles: HashMap<String, u32>,
        provider_extras: HashMap<String, u32>,
    ) -> Self {
        Self {
            user_profiles,
            provider_extras: RwLock::new(provider_extras),
            probe_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Create an empty resolver (no L1 overrides).
    pub fn empty() -> Self {
        Self::new(HashMap::new(), HashMap::new())
    }

    /// Register an L1 per-provider-model `extra["context_window"]` override
    /// (interior-mutable — safe to call after construction).
    pub fn add_provider_extra(&self, model: String, context_window: u32) {
        if let Ok(mut extras) = self.provider_extras.write() {
            extras.insert(model, context_window);
        }
    }

    /// Seed the L2 probe cache (used by warm-up to pre-populate probed values).
    pub fn seed_probe_cache(&self, model: &str, context_window: u32) {
        if let Ok(mut cache) = self.probe_cache.write() {
            cache.insert(model.to_string(), context_window);
        }
    }

    // ── L1 checks (sync) ──────────────────────────────────────────────────

    /// Check L1 user-override channels. Returns `(value, source)` if any hit.
    fn check_user_override(&self, model: &str) -> Option<(u32, ContextSource)> {
        // (a) global env var — highest priority.
        if let Ok(size) = std::env::var("ONEAI_CONTEXT_WINDOW") {
            if let Ok(val) = size.parse::<u32>() {
                return Some((val, ContextSource::UserEnv));
            }
        }
        // (b) per-provider-model extra override.
        if let Ok(extras) = self.provider_extras.read() {
            if let Some(v) = extras.get(model) {
                return Some((*v, ContextSource::UserProviderExtra));
            }
        }
        // (c) per-model profile override.
        if let Some(v) = self.user_profiles.get(model) {
            return Some((*v, ContextSource::UserProfile));
        }
        None
    }

    /// Read the L2 probe cache (sync, no network).
    fn check_probe_cache(&self, model: &str) -> Option<u32> {
        self.probe_cache.read().ok()?.get(model).copied()
    }

    /// L3 fallback: built-in library → name-pattern heuristic.
    fn fallback_l3(model: &str) -> (u32, ContextSource) {
        if let Some(entry) = builtin_lookup(model) {
            return (entry.context_window, ContextSource::BuiltinLibrary);
        }
        (
            infer_context_window_for_tokenizer(model),
            ContextSource::NameHeuristic,
        )
    }

    // ── sync path ─────────────────────────────────────────────────────────

    /// Resolve the context window for `model` without any network IO.
    ///
    /// Priority: L1 (env / extra / profile) → L2 probe cache → L3 (builtin / heuristic).
    pub fn resolve_cached(&self, model: &str) -> u32 {
        self.resolve_with_source(model).0
    }

    /// Like `resolve_cached` but also reports which layer supplied the value.
    pub fn resolve_with_source(&self, model: &str) -> (u32, ContextSource) {
        if let Some((v, src)) = self.check_user_override(model) {
            return (v, src);
        }
        if let Some(v) = self.check_probe_cache(model) {
            return (v, ContextSource::ProviderApi);
        }
        Self::fallback_l3(model)
    }

    // ── async path (caller supplies the provider) ────────────────────────

    /// Resolve the context window, probing `provider` live when L1/L3 miss.
    ///
    /// On a successful L2 probe, the result is written to the shared probe
    /// cache so subsequent sync `resolve_cached` calls see it without re-probing.
    /// If the probe returns `None`, falls back to L3.
    pub async fn resolve_with_provider(&self, model: &str, provider: &Arc<dyn LlmProvider>) -> u32 {
        self.resolve_with_source_with_provider(model, provider).await.0
    }

    /// Async resolve, also reporting the source layer (for CLI display).
    pub async fn resolve_with_source_with_provider(
        &self,
        model: &str,
        provider: &Arc<dyn LlmProvider>,
    ) -> (u32, ContextSource) {
        // L1 first (no probe needed).
        if let Some((v, src)) = self.check_user_override(model) {
            return (v, src);
        }
        // L2: serve from cache if already probed.
        if let Some(v) = self.check_probe_cache(model) {
            return (v, ContextSource::ProviderApi);
        }
        // L2: live probe.
        if let Some(probed) = provider.probe_context_window().await {
            self.seed_probe_cache(model, probed);
            return (probed, ContextSource::ProviderApi);
        }
        // L3 fallback.
        Self::fallback_l3(model)
    }
}

impl Default for ModelContextResolver {
    fn default() -> Self {
        Self::empty()
    }
}

// ─── Test helper: serialize env-var tests ────────────────────────────────────
//
// `ONEAI_CONTEXT_WINDOW` is a process-global env var; tests that set it must
// not run concurrently (they'd corrupt each other's reads). This mutex is
// shared with `token_counter::tests::test_infer_context_window_env_override`.
#[cfg(test)]
pub(crate) static ENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── builtin_lookup ──────────────────────────────────────────────────

    #[test]
    fn test_builtin_lookup_exact_family() {
        let e = builtin_lookup("claude-opus-4-8").unwrap();
        assert_eq!(e.provider, "anthropic");
        assert_eq!(e.context_window, 200_000);
    }

    #[test]
    fn test_builtin_lookup_specific_before_general() {
        // "gpt-4.1-nano" must match the nano entry, not "gpt-4.1" or "gpt-4o".
        let e = builtin_lookup("gpt-4.1-nano-2025-04-14").unwrap();
        assert_eq!(e.model_id, "gpt-4.1-nano");
        assert_eq!(e.context_window, 1_000_000);
    }

    #[test]
    fn test_builtin_lookup_glm() {
        let e = builtin_lookup("glm-5.1-plus").unwrap();
        assert_eq!(e.provider, "glm");
        assert_eq!(e.context_window, 203_000);
    }

    #[test]
    fn test_builtin_lookup_case_insensitive() {
        let e = builtin_lookup("GPT-4O-MINI").unwrap();
        assert_eq!(e.model_id, "gpt-4o-mini");
    }

    #[test]
    fn test_builtin_lookup_unknown_returns_none() {
        assert!(builtin_lookup("totally-unknown-model-xyz").is_none());
    }

    // ── resolver L1 / L3 (sync) ─────────────────────────────────────────

    #[test]
    fn test_resolver_l3_builtin_when_no_overrides() {
        // Serialized — resolve_with_source reads ONEAI_CONTEXT_WINDOW at L1.
        let _g = ENV_TEST_MUTEX.lock().unwrap();
        let r = ModelContextResolver::empty();
        let (v, src) = r.resolve_with_source("claude-sonnet-4-6");
        assert_eq!(v, 200_000);
        assert_eq!(src, ContextSource::BuiltinLibrary);
    }

    #[test]
    fn test_resolver_l3_heuristic_when_unknown() {
        // Serialized — resolve_with_source reads ONEAI_CONTEXT_WINDOW at L1.
        let _g = ENV_TEST_MUTEX.lock().unwrap();
        let r = ModelContextResolver::empty();
        let (v, src) = r.resolve_with_source("some-mystery-model");
        assert_eq!(v, 128_000); // default heuristic
        assert_eq!(src, ContextSource::NameHeuristic);
    }

    #[test]
    fn test_resolver_l1_user_profile_wins_over_builtin() {
        // Serialized — resolve_with_source reads ONEAI_CONTEXT_WINDOW at L1.
        let _g = ENV_TEST_MUTEX.lock().unwrap();
        let mut profiles = HashMap::new();
        profiles.insert("claude-sonnet-4-6".to_string(), 1_000_000u32);
        let r = ModelContextResolver::new(profiles, HashMap::new());
        let (v, src) = r.resolve_with_source("claude-sonnet-4-6");
        assert_eq!(v, 1_000_000);
        assert_eq!(src, ContextSource::UserProfile);
    }

    #[test]
    fn test_resolver_l1_provider_extra_wins_over_builtin() {
        // Serialized — resolve_with_source reads ONEAI_CONTEXT_WINDOW at L1.
        let _g = ENV_TEST_MUTEX.lock().unwrap();
        let mut extras = HashMap::new();
        extras.insert("gpt-4o".to_string(), 999_999u32);
        let r = ModelContextResolver::new(HashMap::new(), extras);
        let (v, src) = r.resolve_with_source("gpt-4o");
        assert_eq!(v, 999_999);
        assert_eq!(src, ContextSource::UserProviderExtra);
    }

    #[test]
    fn test_resolver_l1_env_highest_priority() {
        // UserProfile + Builtin both exist for this model; env must win.
        // Serialized — `ONEAI_CONTEXT_WINDOW` is process-global.
        let _g = ENV_TEST_MUTEX.lock().unwrap();
        let mut profiles = HashMap::new();
        profiles.insert("claude-opus-4-8".to_string(), 500_000u32);
        let r = ModelContextResolver::new(profiles, HashMap::new());
        std::env::set_var("ONEAI_CONTEXT_WINDOW", "777777");
        let (v, src) = r.resolve_with_source("claude-opus-4-8");
        std::env::remove_var("ONEAI_CONTEXT_WINDOW");
        assert_eq!(v, 777_777);
        assert_eq!(src, ContextSource::UserEnv);
    }

    // ── resolver L2 cache (sync read) ───────────────────────────────────

    #[test]
    fn test_resolver_probe_cache_serves_sync() {
        // Serialized — resolve_with_source reads ONEAI_CONTEXT_WINDOW at L1.
        let _g = ENV_TEST_MUTEX.lock().unwrap();
        let r = ModelContextResolver::empty();
        r.seed_probe_cache("llama3.3", 131_072);
        let (v, src) = r.resolve_with_source("llama3.3");
        assert_eq!(v, 131_072);
        assert_eq!(src, ContextSource::ProviderApi);
    }

    #[test]
    fn test_resolver_l1_still_beats_probe_cache() {
        // Serialized — touches ONEAI_CONTEXT_WINDOW.
        let _g = ENV_TEST_MUTEX.lock().unwrap();
        let r = ModelContextResolver::empty();
        r.seed_probe_cache("claude-opus-4-8", 50_000);
        std::env::set_var("ONEAI_CONTEXT_WINDOW", "888888");
        let (v, src) = r.resolve_with_source("claude-opus-4-8");
        std::env::remove_var("ONEAI_CONTEXT_WINDOW");
        assert_eq!(v, 888_888);
        assert_eq!(src, ContextSource::UserEnv);
    }
}
