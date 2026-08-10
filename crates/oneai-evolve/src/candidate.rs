//! `CandidateConfig` — the unit of variation for the evolution loop.
//!
//! A candidate is a *hot-loadable configuration*: a [`DomainPackConfig`] (the
//! primary variation substrate, fully serde-able — strings / `Vec<String>` /
//! `HashMap` / enum-names / numerics) plus an [`AgentLoopOverlay`] of the
//! agent-loop knobs the loop may turn (system_prompt / temperature / top_p /
//! thinking_budget / max_tokens / hard_max_iterations / token_budget). The
//! overlay fields are all `Option`; `None` inherits the baseline loop default.
//!
//! `build_app()` is the single hot-load path: `DomainPackSpecFile::from_config`
//! → `validate_and_build(project_dir)` (the same validator the spec tooling
//! uses) → `AppBuilder::domain_pack(pack)`. A candidate whose pack fails
//! validation never becomes an `App` — variation products land only after
//! passing [`oneai_domain::DomainPackValidator`] (E3 relies on this gate).
//!
//! Per design §0.1 the loop mutates **only the spec space** — never Rust code.
//! `skill_overrides` (E3, skill-text dimension) is a placeholder until the
//! `VariationOperator` lands; declared here so the type is stable across E1→E3.

use std::sync::Arc;

use oneai_app::AppBuilder;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::LlmProvider;
use oneai_domain::{DomainPack, DomainPackConfig, DomainPackSpecFile};

/// Sampling / generation overlay knobs propagated into the `AgentLoopConfig`.
///
/// Only fields `AppBuilder` exposes are wired by `build_app`
/// (`temperature` / `top_p` / `max_tokens` / `thinking_budget`); the rest
/// (`hard_max_iterations` / `token_budget`) are carried for E3 once a loop-config
/// injection seam is added. All `Option` → `None` inherits the baseline.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct AgentLoopOverlay {
    /// Override the pack's system prompt (spec-variation, highest leverage).
    pub system_prompt: Option<String>,
    /// Sampling temperature. **Frozen to 0 in eval** (deterministic strategy);
    /// listed for completeness — E3 does not vary it (see design §6.7).
    pub temperature: Option<f32>,
    /// Nucleus sampling mass. Same caveat as `temperature`.
    pub top_p: Option<f32>,
    /// Max output tokens per inference.
    pub max_tokens: Option<u32>,
    /// Extended-thinking token budget (Anthropic-class providers).
    pub thinking_budget: Option<u32>,
    /// Hard ceiling on AgentLoop iterations (E3 axis).
    pub hard_max_iterations: Option<usize>,
    /// Token budget governing the loop's termination (E3 axis).
    pub token_budget: Option<u32>,
}

/// A hot-loadable candidate configuration — one generation-N individual.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct CandidateConfig {
    /// The primary variation substrate (all 7 DomainPack layers, serde-able).
    pub pack_config: DomainPackConfig,
    /// Agent-loop knobs the loop may turn.
    pub loop_overlay: AgentLoopOverlay,
    /// Skill text patches (E3 skill dimension; placeholder until then).
    pub skill_overrides: Vec<String>,
}

impl CandidateConfig {
    /// Build a candidate from a pack config + empty overlay.
    pub fn from_pack_config(pack_config: DomainPackConfig) -> Self {
        Self {
            pack_config,
            loop_overlay: AgentLoopOverlay::default(),
            skill_overrides: Vec::new(),
        }
    }

    /// Set the overlay (builder-style).
    #[must_use]
    pub fn with_overlay(mut self, overlay: AgentLoopOverlay) -> Self {
        self.loop_overlay = overlay;
        self
    }

    /// Validate + build this candidate into a running [`oneai_app::App`].
    ///
    /// The provider is injected by the caller (the [`EvolutionLoop`](crate::EvolutionLoop)
    /// wraps it in a [`oneai_eval::RecordingProvider`] so trajectories are
    /// captured; tests may pass a bare [`oneai_agent::MockProvider`]-shaped
    /// `Arc<dyn LlmProvider>` directly).
    ///
    /// Steps:
    /// 1. `DomainPackSpecFile::from_config(cfg).validate_and_build(project_dir)`
    ///    — the canonical spec gate. An invalid candidate (unknown tool name,
    ///    out-of-range decay, etc.) fails here and never reaches `AppBuilder`.
    /// 2. `AppBuilder` with provider + noop gate + in-memory trace + default
    ///    parser/usage/token-counter + the built `DomainPack`.
    /// 3. Apply the overlay's generation knobs the builder exposes.
    pub async fn build_app(
        &self,
        provider: Arc<dyn LlmProvider>,
        project_dir: &str,
    ) -> Result<AppHandle> {
        let pack: DomainPack = DomainPackSpecFile::from_config(self.pack_config.clone())
            .validate_and_build(project_dir)
            .map_err(|vr| {
                OneAIError::Config(format!(
                    "candidate pack failed validation: {} error(s), {} warning(s): {}",
                    vr.errors().len(),
                    vr.warnings().len(),
                    vr.errors()
                        .iter()
                        .map(|e| e.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                ))
            })?;

        let mut builder = AppBuilder::new()
            .provider(provider)
            .noop_interaction_gate()
            .trace_in_memory()
            .default_parser()
            .default_usage_tracker()
            .default_token_counter()
            .domain_pack(pack);

        // Overlay: generation knobs the builder exposes. (system_prompt already
        // lives in pack_config.system_prompt; loop knobs land in E3.)
        let g = &self.loop_overlay;
        if let Some(t) = g.temperature {
            builder = builder.temperature(t);
        }
        if let Some(p) = g.top_p {
            builder = builder.top_p(p);
        }
        if let Some(m) = g.max_tokens {
            builder = builder.max_tokens(m);
        }
        if let Some(tb) = g.thinking_budget {
            builder = builder.thinking_budget(Some(tb));
        }

        let app = builder.build().await?;
        Ok(AppHandle(app))
    }
}

/// Thin handle over [`oneai_app::App`] — newtyped so the `oneai-app` re-export
/// stays private to this crate (callers drive through [`EvolutionLoop`](crate::EvolutionLoop),
/// not the raw `App`). Not `Clone` (the loop consumes it once per run; an
/// `App` isn't cheaply cloneable).
pub struct AppHandle(pub oneai_app::App);

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_domain::DomainPackConfig;

    fn coding_seed_config() -> DomainPackConfig {
        // Minimal valid coding seed: a couple of tools, a permission profile,
        // a compression template, and the default memory profile (E0).
        DomainPackConfig {
            name: "coding_seed".to_string(),
            description: "Coding pack seed for evolve tests".to_string(),
            tools: vec!["read_file".to_string(), "calculator".to_string()],
            tool_decorators: std::collections::HashMap::new(),
            context_sources: vec![],
            permission_profile: oneai_domain::PermissionProfileConfig {
                auto_approve: vec!["read_file".to_string(), "calculator".to_string()],
                require_confirmation: vec![],
                deny_by_default: vec![],
                ..Default::default()
            },
            paradigm_strategies: vec![],
            compression_template: oneai_domain::CompressionTemplateConfig {
                name: "coding".to_string(),
                preserve_fields: vec!["critical_files".to_string()],
                truncate_rules: std::collections::HashMap::new(),
            },
            system_prompt: "You are a coding agent.".to_string(),
            memory_profile: Default::default(),
        }
    }

    #[test]
    fn candidate_carries_overlay() {
        let cfg = coding_seed_config();
        let c = CandidateConfig::from_pack_config(cfg).with_overlay(AgentLoopOverlay {
            temperature: Some(0.0),
            thinking_budget: Some(1024),
            ..Default::default()
        });
        assert_eq!(c.loop_overlay.temperature, Some(0.0));
        assert_eq!(c.loop_overlay.thinking_budget, Some(1024));
    }

    #[test]
    fn seed_config_validates() {
        // The seed must pass the DomainPackValidator (the same gate build_app
        // runs) — catches unknown tool names / bad memory bounds before any
        // app is built.
        let cfg = coding_seed_config();
        let spec = DomainPackSpecFile::from_config(cfg);
        let result = spec.validate();
        assert!(
            result.is_valid(),
            "seed config should validate, got errors: {:?}",
            result.errors()
        );
    }
}
