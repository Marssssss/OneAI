//! Generated model catalog — the L3 authoritative source of model metadata.
//!
//! Replaces the hand-maintained `BUILTIN_MODEL_CONTEXT` table as the single
//! source of truth for a model's context window, output limits, and — unlike
//! the old context-only table — real capability flags (`reasoning`,
//! `input_modalities`, `thinking_format`, `supports_strict`, `cache_retention`,
//! routing `tier`).
//!
//! # How the data is produced
//!
//! The catalog is **generated** from `models.snapshot.json` by the workspace
//! `xtask` (`cargo xtask gen-model-data`) into `models.generated.rs`, which is
//! committed. A manifest (`models.manifest.json`) records a structure hash +
//! per-file SHA256 so `cargo xtask check-model-data` (run in CI) detects drift
//! between the snapshot and the committed generated code — exactly like the
//! `Cargo.lock` lockfile gate, this guards reproducibility without freezing
//! the model *list* (the hash is over the canonical structure, not a value
//! snapshot, per戒律 6: no change-detector tests).
//!
//! No network at build time: the crate simply declares the committed static.
//! `cargo xtask fetch-model-data` is an opt-in maintainer refresh from
//! models.dev + provider `/models` (not CI-gated).
//!
//! # Consumers
//!
//! - [`crate::model_context`] — `builtin_lookup` reads this catalog and
//!   projects to `ModelContextEntry` (L3 of the 3-layer resolver).
//! - `oneai_provider` — `compat.rs` + provider `capabilities()` consult the
//!   catalog for real capability values instead of hardcoded defaults.
//! - `oneai_provider::SmartRouter` — capability-aware routing reads real
//!   context windows / tool support from the catalog.

#[path = "models.generated.rs"]
mod generated;

pub use generated::{BUILTIN_MODEL_CONTEXT, CATALOG, CATALOG_JSON};

use crate::smart_router::RoutingTier;
use crate::types::ModelCapability;

// ─── ThinkingFormat ──────────────────────────────────────────────────────────

/// How a model surfaces its reasoning/thinking tokens.
///
/// `Interleaved` = thinking blocks may appear between output blocks
/// (Anthropic extended thinking). `Separate` = a dedicated reasoning channel
/// surfaced apart from the final answer (OpenAI o-series, DeepSeek-R1,
/// Gemini 2.5 thinking). `None` = no first-class thinking surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ThinkingFormat {
    /// No first-class reasoning surface.
    #[default]
    None,
    /// Thinking interleaved with output blocks (Anthropic-style).
    Interleaved,
    /// Separate reasoning channel (OpenAI o-series / DeepSeek-R1 / Gemini 2.5).
    Separate,
}

impl ThinkingFormat {
    /// Parse from the lowercase snapshot string (xtask rendering convention).
    pub fn from_snapshot(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "interleaved" => Self::Interleaved,
            "separate" => Self::Separate,
            _ => Self::None,
        }
    }
}

// ─── ModelEntry ───────────────────────────────────────────────────────────────

/// A single entry in the generated model catalog — the L3 authority.
///
/// Mirrors the shape of `ModelContextEntry` but carries real capability flags.
/// `model_id` is matched as a case-insensitive **substring** of the model name
/// (e.g. `"claude-opus"`, `"gpt-4.1-nano"`); entries are ordered specific →
/// general within a family so the first match wins.
///
/// Constructed only inside the generated static; `#[non_exhaustive]` keeps
/// future fields (vision, audio output, …) addable without a breaking bump.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ModelEntry {
    /// Provider family — "anthropic" | "openai" | "gemini" | "glm" | …
    pub provider: &'static str,
    /// Model-id pattern, matched as a case-insensitive substring.
    pub model_id: &'static str,
    /// Maximum context window size in tokens (input + output combined).
    pub context_window: u32,
    /// Maximum output tokens the model can produce in one response.
    pub max_output_tokens: u32,
    /// Whether the model is a reasoning model (surfaces thinking tokens).
    pub reasoning: bool,
    /// Input modalities the model accepts ("text" / "image" / "audio" / "video").
    pub input_modalities: &'static [&'static str],
    /// How thinking tokens are surfaced.
    pub thinking_format: ThinkingFormat,
    /// Whether the model supports strict JSON-schema constrained output.
    pub supports_strict: bool,
    /// Whether the provider honors prompt-cache retention controls.
    pub cache_retention: bool,
    /// Routing tier classification (feeds SmartRouter quality scoring).
    pub tier: RoutingTier,
}

impl ModelEntry {
    /// Project to the lightweight context-window entry the resolver reads.
    ///
    /// `ModelContextEntry` is the stable L3 record type kept for API
    /// compatibility; this projection drops the capability flags.
    pub fn to_context_entry(self) -> crate::model_context::ModelContextEntry {
        crate::model_context::ModelContextEntry {
            provider: self.provider,
            model_id: self.model_id,
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
        }
    }

    /// True if the model lists `modality` among its inputs.
    pub fn accepts_input(self, modality: &str) -> bool {
        self.input_modalities.iter().any(|m| m == &modality)
    }
}

// ─── Lookup ──────────────────────────────────────────────────────────────────

/// Look up a model in the generated catalog by case-insensitive substring.
///
/// First match wins (table ordered specific → general), matching the prior
/// `model_context::builtin_lookup` contract so the L3 resolver is a drop-in
/// authority replacement.
pub fn lookup(model: &str) -> Option<&'static ModelEntry> {
    let lower = model.to_lowercase();
    CATALOG.iter().find(|entry| lower.contains(entry.model_id))
}

// ─── Capability projection ────────────────────────────────────────────────────

/// Build a [`ModelCapability`] from the catalog, if `model` is known.
///
/// Providers and the SmartRouter call this to replace hardcoded
/// `gpt4_class()`/`claude_class()` defaults with real per-model values. Returns
/// `None` for unknown models so callers keep their existing fallback.
pub fn capability_snapshot(model: &str) -> Option<ModelCapability> {
    let entry = lookup(model)?;
    Some(ModelCapability {
        supports_multimodal: entry.accepts_input("image")
            || entry.accepts_input("audio")
            || entry.accepts_input("video"),
        supports_streaming: true,
        supports_tools: entry.supports_strict || entry.reasoning,
        context_window_size: entry.context_window,
        max_output_tokens: entry.max_output_tokens,
    })
}

// ─── Manifest ────────────────────────────────────────────────────────────────

/// The committed manifest (structure hash + per-file SHA256), as JSON.
///
/// Surfaced so `cargo xtask check-model-data` (and any tool) can verify the
/// generated code matches the snapshot without re-running the generator at
/// build time.
pub static CATALOG_MANIFEST: &str = include_str!("models.manifest.json");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_anthropic_specific_before_general() {
        let e = lookup("claude-opus-4-8").unwrap();
        assert_eq!(e.provider, "anthropic");
        assert_eq!(e.context_window, 200_000);
        assert!(e.reasoning);
        assert!(e.accepts_input("image"));
    }

    #[test]
    fn lookup_gpt_nano_before_4_1() {
        // "gpt-4.1-nano" must match the nano entry, not "gpt-4.1".
        let e = lookup("gpt-4.1-nano-2025-04-14").unwrap();
        assert_eq!(e.model_id, "gpt-4.1-nano");
        assert_eq!(e.context_window, 1_000_000);
    }

    #[test]
    fn lookup_case_insensitive() {
        let e = lookup("GPT-4O-MINI").unwrap();
        assert_eq!(e.model_id, "gpt-4o-mini");
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("totally-unknown-model-xyz").is_none());
    }

    #[test]
    fn capability_snapshot_known_model() {
        let cap = capability_snapshot("claude-sonnet-4-6").unwrap();
        assert_eq!(cap.context_window_size, 200_000);
        assert_eq!(cap.max_output_tokens, 16_000);
        assert!(cap.supports_multimodal);
        assert!(cap.supports_tools);
        assert!(cap.supports_streaming);
    }

    #[test]
    fn capability_snapshot_unknown_is_none() {
        assert!(capability_snapshot("mystery-model-xyz").is_none());
    }

    #[test]
    fn thinking_format_parse() {
        assert_eq!(
            ThinkingFormat::from_snapshot("interleaved"),
            ThinkingFormat::Interleaved
        );
        assert_eq!(
            ThinkingFormat::from_snapshot("SEPARATE"),
            ThinkingFormat::Separate
        );
        assert_eq!(ThinkingFormat::from_snapshot("none"), ThinkingFormat::None);
        assert_eq!(ThinkingFormat::from_snapshot("??"), ThinkingFormat::None);
    }

    #[test]
    fn to_context_entry_projects_context_fields() {
        let e = lookup("glm-5.1-plus").unwrap();
        let c = e.to_context_entry();
        assert_eq!(c.provider, "glm");
        assert_eq!(c.context_window, 203_000);
        assert_eq!(c.max_output_tokens, 16_384);
    }

    #[test]
    fn catalog_nonempty_and_ordered_within_family() {
        // Structure invariant (not a value freeze): each provider appears, and
        // within a family more-specific patterns precede shorter prefixes.
        assert!(!CATALOG.is_empty());
        assert!(lookup("claude-opus").is_some());
        assert!(lookup("gpt-4.1-nano").is_some());
    }
}
