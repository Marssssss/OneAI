//! `gepa.rs` — ④ GEPA variation + Pareto selection (the E3 core).
//!
//! Turns E2's `Diagnosis` set into "vary K candidates → score → Pareto select
//! the non-dominated frontier" for a single generation. The first-axis
//! variation params are the tool triad — `PackSystemPrompt` /
//! `PackToolDecorator` / `PackTool` (design §3.0/E3 首选: the user's original
//! diagnostic goal "which tools were needless / mis-used" names the latter two
//! verbatim; `system_prompt` is the highest-leverage free text). Secondary
//! axes (compression / context / thinking_budget) land later in E3 / E4.
//!
//! ## Variation contract (LLM ↔ Rust)
//!
//! The `LlmVariationOperator` asks its *own* provider (separate from the
//! candidate provider — design §6.3, judge/candidate separation; also avoids
//! MockProvider script-ordering collisions in tests) for a JSON patch-list:
//!
//! ```jsonc
//! {"patches": [
//!   {"param": "pack.system_prompt", "op": "set", "value": "Answer with just the number."},
//!   {"param": "pack.tool_decorators[calculator]", "op": "set", "value": "Use for arithmetic."}
//! ]}
//! ```
//!
//! `param` is a [`ParamRef::path()`] string (not the enum's tagged-JSON form —
//! flatter + friendlier for an LLM to emit). Rust parses it back via
//! [`ParamRef::from_path`] (first-axis only; anything else → drop + `warn`),
//! applies it deterministically, runs the reward-hacking guard, then
//! validates the result through [`DomainPackSpecFile::validate`]. Any failure
//! along that chain drops the candidate (logged) — never panics.
//!
//! ## Scoring + selection
//!
//! Each surviving candidate is scored live on a case subset (design §3.3
//! 难点 C: semantic-variation candidates can't be replayed). The subset
//! prioritizes first + last + previously-failed cases. Three axes feed the
//! Pareto sort: `pass_rate↑`, `total_tokens↓`, `total_latency_ms↓`
//! (reusing the SWE-bench three-axis基底). Non-dominated = frontier.
//!
//! Design: `docs/self-evolution-system-2026-08.md` §3.2/§3.3 + §4 Phase E3.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;

use oneai_core::error::OneAIError;
use oneai_core::traits::LlmProvider;
use oneai_core::{Conversation, InferenceRequest, Message};
use oneai_domain::DomainPackSpecFile;
use oneai_eval::EvalSuite;

use crate::candidate::CandidateConfig;
use crate::subgraph::{Diagnosis, ParamRef};
use crate::trajectory_collector::CaseRun;

// ─── Patch recipe ─────────────────────────────────────────────────────────

/// One mutation recipe — a typed `ParamRef`-path + op + textual value. The
/// LLM emits these; Rust applies them deterministically. `param` is a
/// [`ParamRef::path()`] string so the contract is flat JSON.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Patch {
    /// `ParamRef::path()` string (e.g. `pack.system_prompt`). First-axis only
    /// for E3; unknown paths drop the candidate.
    pub param: String,
    /// Mutation kind.
    pub op: PatchOp,
    /// Textual value — `Set` replaces the field with it; `Add`/`Remove` use
    /// it as the tool/decorator name key (value ignored for `Remove`).
    pub value: String,
}

/// Mutation operation.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PatchOp {
    /// Replace the addressed field's value.
    Set,
    /// Add the named tool / decorator (no-op if already present).
    Add,
    /// Remove the named tool / decorator.
    Remove,
}

/// The LLM's per-call response: one candidate = one patch-list.
#[derive(Debug, Clone, Deserialize)]
struct PatchList {
    patches: Vec<Patch>,
}

// ─── Apply + guard ────────────────────────────────────────────────────────

/// Apply one patch to `cfg` in place. Returns `Err(msg)` on an unsupported
/// param path, an inapplicable op, or a reward-hacking-guard rejection — the
/// caller drops the candidate (never panics).
pub fn apply_patch(patch: &Patch, cfg: &mut CandidateConfig) -> std::result::Result<(), String> {
    let param = ParamRef::from_path(&patch.param)
        .ok_or_else(|| format!("unsupported param path (first-axis only): {}", patch.param))?;
    let pc = &mut cfg.pack_config;
    match (&param, &patch.op) {
        (ParamRef::PackSystemPrompt, PatchOp::Set) => {
            pc.system_prompt = patch.value.clone();
        }
        (ParamRef::PackToolDecorator(name), PatchOp::Set) => {
            if !semantic_guard_decoration(name, &patch.value) {
                return Err(format!(
                    "decorator for '{name}' contradicts the tool's verb (read/write antonym) — rejected by reward-hacking guard"
                ));
            }
            pc.tool_decorators.insert(name.clone(), patch.value.clone());
        }
        (ParamRef::PackToolDecorator(name), PatchOp::Remove) => {
            pc.tool_decorators.remove(name);
        }
        (ParamRef::PackTool(name), PatchOp::Add) => {
            if !pc.tools.iter().any(|t| t == name) {
                pc.tools.push(name.clone());
            }
            // Sync permission: auto-approve the added tool so it's usable
            // headless (noop gate) without a permission gap.
            if !pc.permission_profile.auto_approve.iter().any(|t| t == name) {
                pc.permission_profile.auto_approve.push(name.clone());
            }
        }
        (ParamRef::PackTool(name), PatchOp::Remove) => {
            pc.tools.retain(|t| t != name);
            pc.permission_profile.auto_approve.retain(|t| t != name);
            pc.permission_profile
                .require_confirmation
                .retain(|t| t != name);
            // deny_by_default entries are pattern structs; drop any whose
            // `tool` field exactly matches the removed tool name.
            pc.permission_profile
                .deny_by_default
                .retain(|d| d.tool != *name);
            pc.tool_decorators.remove(name);
        }
        _ => {
            return Err(format!(
                "op {:?} not applicable to {}",
                patch.op, patch.param
            ));
        }
    }
    Ok(())
}

/// Apply a patch-list to a clone of `base`, returning the mutated candidate.
/// Stops at the first failing patch.
pub fn apply_patches(
    patches: &[Patch],
    base: &CandidateConfig,
) -> std::result::Result<CandidateConfig, String> {
    let mut cfg = base.clone();
    for p in patches {
        apply_patch(p, &mut cfg)?;
    }
    Ok(cfg)
}

/// Validate a candidate's pack through the canonical spec gate. `Ok(())` if
/// it's buildable; `Err` carries the validator's first error so the operator
/// can log a concrete reason. Mirrors `CandidateConfig::build_app`'s gate but
/// without the `AppBuilder` cost — we reject invalid candidates *before*
/// scoring them.
pub fn validate_candidate(cfg: &CandidateConfig) -> std::result::Result<(), String> {
    let result = DomainPackSpecFile::from_config(cfg.pack_config.clone()).validate();
    if result.is_valid() {
        Ok(())
    } else {
        let first = result
            .errors()
            .first()
            .map(|e| e.message.clone())
            .unwrap_or_default();
        Err(if first.is_empty() {
            format!(
                "candidate pack failed validation ({} error(s))",
                result.errors().len()
            )
        } else {
            format!("candidate pack failed validation: {first}")
        })
    }
}

/// Reward-hacking guard for `tool_decorators` Set patches: reject a decorator
/// whose value flips the tool's verb (design §4 E3 — "把 read_file 描述成写文件").
/// Name-based antonym check — no external tool-schema table (the crate has
/// none), deterministic, catches the design's example. Conservative by design:
/// a false positive just costs one re-roll.
pub fn semantic_guard_decoration(tool_name: &str, value: &str) -> bool {
    let n = tool_name.to_lowercase();
    let v = value.to_lowercase();
    let contains_any = |s: &str, words: &[&str]| words.iter().any(|w| s.contains(w));
    if n.contains("read") && contains_any(&v, &["write", "delete", "remove"]) {
        return false;
    }
    if n.contains("write") && v.contains("read") {
        return false;
    }
    true
}

// ─── Case subset ──────────────────────────────────────────────────────────

/// Build a case-subset suite for variation evaluation (design §3.3 难点 C +
/// §6.3: cheaper than the full suite, prioritizes first/last/failed cases so
/// the frontier is judged on the hardest + boundary cases). `ratio >= 1.0`
/// returns the full suite unchanged (E3 tests use this for fair seed-vs-candidate
/// comparison under a deterministic mock).
pub fn select_case_subset(suite: &EvalSuite, ratio: f64, failed_ids: &[String]) -> EvalSuite {
    let total = suite.cases.len();
    if ratio >= 1.0 || total <= 1 {
        // Full suite: clone preserving all fields.
        let mut sub = EvalSuite::new(&suite.name);
        sub.description = suite.description.clone();
        sub.cases = suite.cases.clone();
        sub.metrics = suite.metrics.clone();
        sub.domain = suite.domain.clone();
        return sub;
    }
    let cap = ((total as f64) * ratio).ceil() as usize;
    let cap = cap.max(1).min(total);

    // Priority order: previously-failed cases first (they're the signal),
    // then first + last (boundary cases), then fill in original order —
    // dedup preserving first-seen.
    let mut order: Vec<String> = Vec::new();
    let push = |v: &mut Vec<String>, id: String| {
        if !v.contains(&id) {
            v.push(id);
        }
    };
    for f in failed_ids {
        push(&mut order, f.clone());
    }
    if let Some(c) = suite.cases.first() {
        push(&mut order, c.id.clone());
    }
    if let Some(c) = suite.cases.last() {
        push(&mut order, c.id.clone());
    }
    for c in &suite.cases {
        if order.len() >= cap {
            break;
        }
        push(&mut order, c.id.clone());
    }

    let chosen: Vec<_> = order
        .into_iter()
        .take(cap)
        .filter_map(|id| suite.cases.iter().find(|c| c.id == id).cloned())
        .collect();

    let mut sub = EvalSuite::new(&suite.name);
    sub.description = suite.description.clone();
    sub.cases = chosen;
    sub.metrics = suite.metrics.clone();
    sub.domain = suite.domain.clone();
    sub
}

// ─── ScoredCandidate ──────────────────────────────────────────────────────

/// One candidate + its three Pareto axes, scored on the subset. The axes
/// (pass_rate↑, total_tokens↓, total_latency_ms↓) reuse the SWE-bench
/// three-axis基底 (design §3.2).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    /// The mutated config (clone of base + applied patches).
    pub candidate: CandidateConfig,
    /// Fraction of subset cases that passed.
    pub pass_rate: f64,
    /// Prompt + completion tokens summed across subset cases.
    pub total_tokens: u64,
    /// Wall-clock latency summed across subset cases (ms).
    pub total_latency_ms: u64,
}

impl ScoredCandidate {
    /// Construct from the case runs a candidate produced on the subset.
    /// `pass_rate` = passed / total; token + latency axes from each run's
    /// `EvalResult`.
    pub fn from_runs(candidate: CandidateConfig, runs: &[CaseRun]) -> Self {
        let total = runs.len();
        let passed = runs.iter().filter(|r| r.result.passed()).count();
        let pass_rate = if total == 0 {
            0.0
        } else {
            passed as f64 / total as f64
        };
        let total_tokens = runs
            .iter()
            .map(|r| r.result.prompt_tokens + r.result.completion_tokens)
            .sum();
        let total_latency_ms = runs.iter().map(|r| r.result.duration_ms).sum();
        Self {
            candidate,
            pass_rate,
            total_tokens,
            total_latency_ms,
        }
    }
}

// ─── VariationOperator trait + LlmVariationOperator ───────────────────────

/// Produces K mutated candidates from a seed + its diagnoses. The default
/// [`LlmVariationOperator`] asks a dedicated variation provider for patch-lists;
/// a test or custom impl may emit them directly.
#[async_trait]
pub trait VariationOperator: Send + Sync {
    /// Return up to `k` validated, mutation-applied candidates. Implementations
    /// drop any candidate that fails parse / apply / guard / validate (with a
    /// `warn` log) — the returned vec may be shorter than `k`.
    async fn vary(
        &self,
        diagnoses: &[Diagnosis],
        base: &CandidateConfig,
        k: usize,
    ) -> Vec<CandidateConfig>;
}

/// The default operator — drives an LLM to emit patch-lists. The provider is
/// *separate* from the candidate provider (design §6.3).
pub struct LlmVariationOperator {
    variation_provider: Arc<dyn LlmProvider>,
}

impl LlmVariationOperator {
    /// Construct with the dedicated variation provider (the "optimizer model").
    pub fn new(variation_provider: Arc<dyn LlmProvider>) -> Self {
        Self { variation_provider }
    }
}

#[async_trait]
impl VariationOperator for LlmVariationOperator {
    async fn vary(
        &self,
        diagnoses: &[Diagnosis],
        base: &CandidateConfig,
        k: usize,
    ) -> Vec<CandidateConfig> {
        let mut out = Vec::with_capacity(k);
        for i in 0..k {
            match self.generate_one(diagnoses, base, i).await {
                Ok(cfg) => out.push(cfg),
                Err(reason) => warn!(i, %reason, "evolve: dropped variation candidate"),
            }
        }
        out
    }
}

impl LlmVariationOperator {
    /// One LLM call → one candidate. Returns `Err(reason)` (with a logged
    /// drop) on any contract failure: bad JSON, unsupported param path,
    /// apply error, guard rejection, or validator failure.
    async fn generate_one(
        &self,
        diagnoses: &[Diagnosis],
        base: &CandidateConfig,
        index: usize,
    ) -> std::result::Result<CandidateConfig, String> {
        let prompt = variation_prompt(diagnoses, base, index);
        let mut conversation = Conversation::new();
        conversation.add_message(Message::user(prompt));
        let request = InferenceRequest {
            conversation,
            tools: vec![],
            max_tokens: Some(1024),
            temperature: Some(0.0),
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        };
        let response = self
            .variation_provider
            .infer(request)
            .await
            .map_err(|e| format!("variation infer #{index}: {e}"))?;
        let text = response.message.text_content();
        let parsed: PatchList = serde_json::from_str(text.trim())
            .map_err(|e| format!("variation #{index} not valid patch-list JSON: {e}"))?;
        let cfg = apply_patches(&parsed.patches, base)
            .map_err(|e| format!("variation #{index} apply failed: {e}"))?;
        validate_candidate(&cfg).map_err(|e| format!("variation #{index} invalid: {e}"))?;
        Ok(cfg)
    }
}

/// Compose the variation prompt — diagnoses summary + base config's first-axis
/// fields + the JSON contract. The MockProvider scripts the response, so the
/// prose just needs to be parseable; for a real model it frames the task.
fn variation_prompt(diagnoses: &[Diagnosis], base: &CandidateConfig, index: usize) -> String {
    let pc = &base.pack_config;
    let suspect: Vec<String> = diagnoses
        .iter()
        .flat_map(|d| d.suspect_params.iter().map(|p| p.path()))
        .collect();
    let critiques = diagnoses
        .iter()
        .map(|d| {
            format!(
                "- case {}: {}",
                d.case_id,
                d.critique.lines().next().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let decorators = if pc.tool_decorators.is_empty() {
        "(none)".to_string()
    } else {
        pc.tool_decorators
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("; ")
    };
    format!(
        "You are optimizing an AI agent's domain-pack config. Propose one candidate variation \
         (attempt {index}) that fixes the diagnosed failures. Only touch the first-axis params: \
         pack.system_prompt, pack.tool_decorators[<name>], pack.tools[<name>].\n\n\
         Current system_prompt: {:?}\nCurrent tools: {:?}\nCurrent tool_decorators: {decorators}\n\n\
         Diagnoses — suspect params: {}\nCritiques:\n{critiques}\n\n\
         Respond with ONLY a JSON object: {{\"patches\":[{{\"param\":\"pack.system_prompt\",\
         \"op\":\"set\",\"value\":\"...\"}}]}}. op ∈ set|add|remove. No prose outside the JSON.",
        pc.system_prompt,
        pc.tools,
        if suspect.is_empty() {
            "(none)".to_string()
        } else {
            suspect.join(", ")
        },
    )
}

// ─── ParetoSelector trait + NonDominatedSelector ─────────────────────────

/// Multi-objective selector over scored candidates. Default: non-dominated
/// sort on `(pass_rate↑, total_tokens↓, total_latency_ms↓)`.
pub trait ParetoSelector: Send + Sync {
    /// Return the non-dominated frontier (preserving input order), up to `k`.
    fn select(&self, scored: &[ScoredCandidate], k: usize) -> Vec<ScoredCandidate>;
}

/// Three-axis non-dominated sort. A candidate is dominated iff another is
/// ≥ on pass_rate, ≤ on tokens, ≤ on latency, and strictly better on at least
/// one. The frontier is everything not dominated. Capped at `k` (highest
/// pass_rate first, tie-broken by lower tokens).
pub struct NonDominatedSelector;

impl ParetoSelector for NonDominatedSelector {
    fn select(&self, scored: &[ScoredCandidate], k: usize) -> Vec<ScoredCandidate> {
        let dominated = |i: usize| -> bool {
            let a = &scored[i];
            scored.iter().enumerate().any(|(j, b)| {
                i != j
                    && b.pass_rate >= a.pass_rate
                    && b.total_tokens <= a.total_tokens
                    && b.total_latency_ms <= a.total_latency_ms
                    && (b.pass_rate > a.pass_rate
                        || b.total_tokens < a.total_tokens
                        || b.total_latency_ms < a.total_latency_ms)
            })
        };
        let mut frontier: Vec<ScoredCandidate> = scored
            .iter()
            .enumerate()
            .filter(|(i, _)| !dominated(*i))
            .map(|(_, c)| c.clone())
            .collect();
        // Stable ordering: pass_rate desc, then tokens asc.
        frontier.sort_by(|a, b| {
            b.pass_rate
                .partial_cmp(&a.pass_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.total_tokens.cmp(&b.total_tokens))
        });
        frontier.truncate(k);
        frontier
    }
}

// ─── GepaConfig + GepaOptimizer + OptimizationResult ────────────────────

/// GEPA-loop configuration (design §3.2).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct GepaConfig {
    /// Population size K — candidates per generation. Default 4.
    pub population: usize,
    /// Fraction of the suite used for variation evaluation (≤1.0 = full).
    /// Default 0.4 (design §3.3 难点 C "35x fewer rollouts" — graded sampling).
    pub case_subset_ratio: f64,
    /// Convergence target pass rate (used by E4; E3 just records it).
    pub target_pass_rate: f64,
}

impl Default for GepaConfig {
    fn default() -> Self {
        Self {
            population: 4,
            case_subset_ratio: 0.4,
            target_pass_rate: 0.85,
        }
    }
}

impl GepaConfig {
    /// Construct with E3 defaults.
    pub fn new() -> Self {
        Self::default()
    }
    /// Set population K.
    #[must_use]
    pub fn with_population(mut self, k: usize) -> Self {
        self.population = k;
        self
    }
    /// Set the case-subset ratio.
    #[must_use]
    pub fn with_case_subset_ratio(mut self, r: f64) -> Self {
        self.case_subset_ratio = r;
        self
    }
}

/// Wires a variation operator + Pareto selector + config. Owned by the
/// `EvolutionLoop`; the loop drives scoring (it has `collect_runs`), so the
/// optimizer only exposes `vary` + `select` — no circular dependency on the
/// loop type.
pub struct GepaOptimizer {
    /// The variation operator (default `LlmVariationOperator`).
    pub operator: Arc<dyn VariationOperator>,
    /// The Pareto selector (default `NonDominatedSelector`).
    pub selector: Arc<dyn ParetoSelector>,
    /// GEPA config.
    pub config: GepaConfig,
}

impl GepaOptimizer {
    /// Construct with an operator + selector + config.
    pub fn new(
        operator: Arc<dyn VariationOperator>,
        selector: Arc<dyn ParetoSelector>,
        config: GepaConfig,
    ) -> Self {
        Self {
            operator,
            selector,
            config,
        }
    }

    /// Construct with the default LLM operator (separate variation provider)
    /// + non-dominated selector + given config.
    pub fn with_llm_operator(variation_provider: Arc<dyn LlmProvider>, config: GepaConfig) -> Self {
        Self::new(
            Arc::new(LlmVariationOperator::new(variation_provider)),
            Arc::new(NonDominatedSelector),
            config,
        )
    }

    /// Produce K candidates from the seed + diagnoses (delegates to operator).
    pub async fn vary(
        &self,
        diagnoses: &[Diagnosis],
        base: &CandidateConfig,
    ) -> Vec<CandidateConfig> {
        self.operator
            .vary(diagnoses, base, self.config.population)
            .await
    }

    /// Select the frontier from scored candidates (delegates to selector).
    pub fn select(&self, scored: &[ScoredCandidate]) -> Vec<ScoredCandidate> {
        self.selector.select(scored, scored.len())
    }
}

/// A single generation's optimization output — folded into the
/// `EvolutionReport` by the loop.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct OptimizationResult {
    /// Every scored candidate (seed-as-candidate included if the loop added it).
    pub scored: Vec<ScoredCandidate>,
    /// The non-dominated frontier (Pareto).
    pub frontier: Vec<ScoredCandidate>,
    /// The frontier's best (highest pass_rate, tie-broken by tokens).
    pub best: Option<ScoredCandidate>,
}

impl OptimizationResult {
    /// Build from scored candidates: select frontier + pick best.
    pub fn from(scored: Vec<ScoredCandidate>, optimizer: &GepaOptimizer) -> Self {
        let frontier = optimizer.select(&scored);
        let best = frontier.first().cloned();
        Self {
            scored,
            frontier,
            best,
        }
    }

    /// `Ok(best)` if a frontier exists, else `Err` with a one-liner.
    pub fn require_best(&self) -> std::result::Result<&ScoredCandidate, OneAIError> {
        self.best.as_ref().ok_or_else(|| {
            OneAIError::Config("evolve optimization produced no frontier candidate".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_domain::{CompressionTemplateConfig, DomainPackConfig, PermissionProfileConfig};
    use oneai_eval::{EvalCase, EvalSuiteBuilder, ExactMatchMetric, ExpectedOutput};
    use std::collections::HashMap;

    fn coding_cfg(prompt: &str) -> DomainPackConfig {
        DomainPackConfig {
            name: "coding_seed".into(),
            description: String::new(),
            tools: vec!["read_file".into(), "calculator".into()],
            tool_decorators: HashMap::new(),
            context_sources: vec![],
            permission_profile: PermissionProfileConfig {
                auto_approve: vec!["read_file".into(), "calculator".into()],
                require_confirmation: vec![],
                deny_by_default: vec![],
            },
            paradigm_strategies: vec![],
            compression_template: CompressionTemplateConfig {
                name: "coding".into(),
                preserve_fields: vec!["critical_files".into()],
                truncate_rules: HashMap::new(),
            },
            system_prompt: prompt.into(),
            memory_profile: Default::default(),
        }
    }

    #[test]
    fn apply_set_system_prompt_replaces_field() {
        let base = CandidateConfig::from_pack_config(coding_cfg("old prompt"));
        let patches = vec![Patch {
            param: "pack.system_prompt".into(),
            op: PatchOp::Set,
            value: "new prompt".into(),
        }];
        let cfg = apply_patches(&patches, &base).expect("apply");
        assert_eq!(cfg.pack_config.system_prompt, "new prompt");
    }

    #[test]
    fn apply_add_tool_syncs_permission() {
        let base = CandidateConfig::from_pack_config(coding_cfg("p"));
        let patches = vec![Patch {
            param: "pack.tools[write_file]".into(),
            op: PatchOp::Add,
            value: "write_file".into(),
        }];
        let cfg = apply_patches(&patches, &base).expect("apply");
        assert!(cfg.pack_config.tools.contains(&"write_file".to_string()));
        assert!(cfg
            .pack_config
            .permission_profile
            .auto_approve
            .contains(&"write_file".to_string()));
    }

    #[test]
    fn apply_remove_tool_purges_everywhere() {
        let base = CandidateConfig::from_pack_config(coding_cfg("p"));
        let patches = vec![Patch {
            param: "pack.tools[calculator]".into(),
            op: PatchOp::Remove,
            value: "calculator".into(),
        }];
        let cfg = apply_patches(&patches, &base).expect("apply");
        assert!(!cfg.pack_config.tools.contains(&"calculator".to_string()));
        assert!(!cfg
            .pack_config
            .permission_profile
            .auto_approve
            .contains(&"calculator".to_string()));
    }

    #[test]
    fn guard_rejects_read_tool_described_as_write() {
        // The design's canonical cheat: describing read_file as a write tool.
        assert!(!semantic_guard_decoration(
            "read_file",
            "Use this to write files."
        ));
        // And the symmetric case.
        assert!(!semantic_guard_decoration(
            "write_file",
            "Read files with this."
        ));
        // Legitimate descriptions pass.
        assert!(semantic_guard_decoration(
            "read_file",
            "Read the contents of a file."
        ));
        assert!(semantic_guard_decoration(
            "calculator",
            "Compute arithmetic."
        ));
    }

    #[test]
    fn apply_decorator_cheat_rejected_by_guard() {
        let base = CandidateConfig::from_pack_config(coding_cfg("p"));
        let patches = vec![Patch {
            param: "pack.tool_decorators[read_file]".into(),
            op: PatchOp::Set,
            value: "Use this to write files.".into(),
        }];
        let err = apply_patches(&patches, &base).unwrap_err();
        assert!(err.contains("reward-hacking guard"), "{err}");
    }

    #[test]
    fn apply_unknown_path_errors() {
        let base = CandidateConfig::from_pack_config(coding_cfg("p"));
        let patches = vec![Patch {
            param: "pack.memory.recall".into(), // not first-axis
            op: PatchOp::Set,
            value: "x".into(),
        }];
        assert!(apply_patches(&patches, &base).is_err());
    }

    #[test]
    fn validate_candidate_accepts_seed() {
        let base = CandidateConfig::from_pack_config(coding_cfg("p"));
        validate_candidate(&base).expect("seed validates");
    }

    fn three_case_suite() -> EvalSuite {
        let metrics: Vec<Arc<dyn oneai_eval::EvalMetric>> = vec![Arc::new(ExactMatchMetric)];
        EvalSuiteBuilder::new("sub")
            .case(EvalCase::with_id("a", "q", ExpectedOutput::exact("1")))
            .case(EvalCase::with_id("b", "q", ExpectedOutput::exact("2")))
            .case(EvalCase::with_id("c", "q", ExpectedOutput::exact("3")))
            .metrics(metrics)
            .build()
    }

    #[test]
    fn subset_ratio_full_returns_all_cases() {
        let s = three_case_suite();
        let sub = select_case_subset(&s, 1.0, &[]);
        assert_eq!(sub.cases.len(), 3);
    }

    #[test]
    fn subset_ratio_partial_prioritizes_failed_first_last() {
        let s = three_case_suite();
        // ratio ~0.34 → cap=ceil(3*0.34)=2. Failed id "b" should be included.
        let sub = select_case_subset(&s, 0.34, &["b".to_string()]);
        assert_eq!(sub.cases.len(), 2);
        let ids: Vec<&str> = sub.cases.iter().map(|c| c.id.as_str()).collect();
        // first (a) + failed (b) — cap 2.
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
    }

    #[test]
    fn pareto_frontier_excludes_dominated() {
        // a dominates b (same pass, fewer tokens, less latency).
        let a = ScoredCandidate {
            candidate: CandidateConfig::from_pack_config(coding_cfg("a")),
            pass_rate: 1.0,
            total_tokens: 100,
            total_latency_ms: 10,
        };
        let b = ScoredCandidate {
            candidate: CandidateConfig::from_pack_config(coding_cfg("b")),
            pass_rate: 1.0,
            total_tokens: 200,
            total_latency_ms: 20,
        };
        let c = ScoredCandidate {
            candidate: CandidateConfig::from_pack_config(coding_cfg("c")),
            pass_rate: 0.0,
            total_tokens: 50,
            total_latency_ms: 5,
        };
        // c is NOT dominated by a (c has fewer tokens+latency, a has higher
        // pass — neither dominates). a dominates b. Frontier = {a, c}.
        let frontier = NonDominatedSelector.select(&[a.clone(), b, c.clone()], 10);
        assert_eq!(frontier.len(), 2);
        assert_eq!(frontier[0].candidate.pack_config.system_prompt, "a");
        assert!(frontier
            .iter()
            .any(|f| f.candidate.pack_config.system_prompt == "c"));
    }
}
