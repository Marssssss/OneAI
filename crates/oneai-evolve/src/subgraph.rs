//! `subgraph.rs` — ③ Minimal Subgraph diagnosis (the E2 core).
//!
//! Given a [`FailedCase`] + the [`CandidateConfig`] that produced it, attribute
//! the failure to a set of *suspect* variation parameters (`ParamRef`s) by
//! walking the span tree — not by gradient. This is Trace(MSP) ("propagation
//! Minimal Subgraph") realized on OneAI's *span tree* (which is a hierarchy,
//! not an explicit compute graph): we substitute span causal-adjacency for
//! the gradient path Trace(MSR) would backprop along.
//!
//! ## Algorithm
//!
//! 1. **Enumerate candidate params** from the config — every mutable field
//!    that's actually set becomes a `ParamRef` (a non-empty `system_prompt`
//!    → `PackSystemPrompt`; each tool → `PackTool`; etc.).
//! 2. **Influence map**: for each candidate param, the set of spans it
//!    plausibly *affects* — e.g. `PackSystemPrompt` → all `SpanKind::LLM`
//!    spans (the prompt shapes every inference); `PackTool(name)` → `TOOL`
//!    spans with `tool.name == name`; `PackRecall`/`PackDecay`/… → `RETRIEVER`
//!    spans; `PackPermission` → `APPROVAL` spans.
//! 3. **Failure span** = the last `LLM` span in pre-order DFS (the inference
//!    that emitted the wrong/failing output); if none, the deepest leaf. The
//!    **failure path** = its ancestry chain root→failure.
//! 4. **Suspect** = candidate params whose affected-span set intersects the
//!    failure path. These are the params that plausibly touched the failing
//!    output.
//! 5. **Fallback** (design §3.3 难点 A): if no param touches the failure path
//!    — e.g. the tree has no `LLM` spans, or every candidate maps to an empty
//!    span set — degrade to Reflexion-style "last N rounds" (tail `LLM`/`TOOL`
//!    spans) and mark *all* candidate params suspect. The diagnostician never
//!    stalls: it always returns a `Diagnosis` (possibly the fallback).
//!
//! The `subtrace` returned is a serializable summary of the failure path (+
//! tail rounds, when the fallback fired) — enough for an LLM-judge to read
//! and for E5's regression gate to stream, without serializing the whole tree.
//!
//! ## Diagnosticians
//!
//! - [`HeuristicDiagnostician`] — the deterministic core (no LLM). The loop's
//!   default (E2 tests assert against it; an LLM judge would make the suite
//!   non-deterministic). Produces `suspect_params` + `subtrace` + a templated
//!   `critique`.
//! - [`LlmDiagnostician`] — wraps the same `suspect_params` + `subtrace` and,
//!   when a judge provider is injected, asks it to rewrite the critique in
//!   natural language (mirroring `LlmJudgeMetric`'s judge call at
//!   `builtin_metrics.rs:455`). Without a judge it degrades to the heuristic
//!   critique. E5 wires a stronger/different-family judge (design §6.3).
//!
//! Design: `docs/self-evolution-system-2026-08.md` §3.2/§3.3 + §4 Phase E2.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use oneai_core::traits::LlmProvider;
use oneai_trace::{Span, SpanKind, TraceTree};

use crate::candidate::CandidateConfig;
use crate::failure_extractor::FailedCase;

// ─── ParamRef ───────────────────────────────────────────────────────────

/// A pointer into a `CandidateConfig`'s addressable (mutable) field — design
/// §3.0 全图. `E3`'s `VariationOperator` reads a `Diagnosis.suspect_params`
/// and mutates *only* the fields these variants name (首批轴 =
/// `PackSystemPrompt` / `PackToolDecorator` / `PackTool`).
///
/// Memory-axis variants are tags (no inner granularity) until E4 needs
/// per-field resolution; `#[non_exhaustive]` lets us add `DecayField`/… later
/// without breaking the enum's external matches.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ParamRef {
    // ── E3 首选轴 ───────────────────────────────────────────────
    /// `pack_config.system_prompt` — highest-leverage free text.
    PackSystemPrompt,
    /// `pack_config.tool_decorators[name]` — the description override for
    /// `name` (the spec's only channel to retune a tool's description).
    PackToolDecorator(String),
    /// `pack_config.tools` — add/remove the named tool.
    PackTool(String),
    // ── E3 次轴 ───────────────────────────────────────────────
    /// `pack_config.compression_template.preserve_fields[i]`.
    PackCompressionField(usize),
    /// `pack_config.context_sources` — add/remove the named context source.
    PackContextSource(String),
    /// `pack_config.paradigm_strategies[i].trigger`.
    PackParadigmTrigger(usize),
    /// `loop_overlay.thinking_budget`.
    LoopThinkingBudget,
    /// `loop_overlay.hard_max_iterations`.
    LoopHardMaxIterations,
    /// `loop_overlay.token_budget`.
    LoopTokenBudget,
    // ── E0 spec化 + E3后期 / E4 ────────────────────────────────
    /// `memory_profile.extraction_schema` — add/remove a fact-type entry (E4).
    PackExtractionSchema,
    /// `memory_profile.recall` (strategy / top_k / time_decay) — suspect tag
    /// used by the diagnostician; the variation operator addresses the
    /// concrete numeric field via [`PackRecallTopK`].
    PackRecall,
    /// `memory_profile.recall.top_k` — the numeric recall cap (E4 first
    /// concrete MemoryProfile axis; long-horizon suites surface its effect,
    /// short suites don't — design §3.0/E4).
    PackRecallTopK,
    /// `memory_profile.decay` (enabled / thresholds / ttl / half_life).
    PackDecay,
    /// `memory_profile.working_state` (compaction + retention).
    PackWorkingStateCompaction,
    // ── E5 慎 / 低优先 ─────────────────────────────────────────
    /// `permission_profile.*` for the named tool (headless eval has no
    /// `APPROVAL` spans → never suspect in practice; listed for completeness).
    PackPermission(String),
    /// `skill_overrides[i]` — skill text patch.
    SkillText(String),
}

impl ParamRef {
    /// Human-readable path string (e.g. `pack.system_prompt`,
    /// `pack.tool_decorators[calculator]`) — used in the report + critique.
    pub fn path(&self) -> String {
        match self {
            Self::PackSystemPrompt => "pack.system_prompt".into(),
            Self::PackToolDecorator(n) => format!("pack.tool_decorators[{n}]"),
            Self::PackTool(n) => format!("pack.tools[{n}]"),
            Self::PackCompressionField(i) => format!("pack.compression.preserve_fields[{i}]"),
            Self::PackContextSource(n) => format!("pack.context_sources[{n}]"),
            Self::PackParadigmTrigger(i) => format!("pack.paradigm_strategies[{i}].trigger"),
            Self::LoopThinkingBudget => "loop.thinking_budget".into(),
            Self::LoopHardMaxIterations => "loop.hard_max_iterations".into(),
            Self::LoopTokenBudget => "loop.token_budget".into(),
            Self::PackExtractionSchema => "pack.memory.extraction_schema".into(),
            Self::PackRecall => "pack.memory.recall".into(),
            Self::PackRecallTopK => "pack.memory.recall.top_k".into(),
            Self::PackDecay => "pack.memory.decay".into(),
            Self::PackWorkingStateCompaction => "pack.memory.working_state".into(),
            Self::PackPermission(n) => format!("pack.permission[{n}]"),
            Self::SkillText(n) => format!("skill[{n}]"),
        }
    }

    /// Inverse of [`path`](Self::path) for the params the `VariationOperator`
    /// can address: the E3 first-axis (`pack.system_prompt` /
    /// `pack.tool_decorators[<name>]` / `pack.tools[<name>]`) **plus** the E4
    /// MemoryProfile axis (`pack.memory.recall.top_k` /
    /// `pack.memory.extraction_schema`). Returns `None` for any other string —
    /// the `VariationOperator` drops patches it can't address (with a `warn`
    /// log) rather than guessing. This is the parse side of the LLM patch
    /// contract (design §4 E3/E4): the LLM emits `path()` strings, Rust
    /// resolves them back to typed `ParamRef`s.
    pub fn from_path(path: &str) -> Option<Self> {
        if path == "pack.system_prompt" {
            return Some(Self::PackSystemPrompt);
        }
        if let Some(rest) = path.strip_prefix("pack.tool_decorators[") {
            if let Some(name) = rest.strip_suffix(']') {
                return Some(Self::PackToolDecorator(name.to_string()));
            }
        }
        if let Some(rest) = path.strip_prefix("pack.tools[") {
            if let Some(name) = rest.strip_suffix(']') {
                return Some(Self::PackTool(name.to_string()));
            }
        }
        if path == "pack.memory.recall.top_k" {
            return Some(Self::PackRecallTopK);
        }
        if path == "pack.memory.extraction_schema" {
            return Some(Self::PackExtractionSchema);
        }
        None
    }
}

// ─── TraceSlice ──────────────────────────────────────────────────────────

/// A serializable minimal slice of the span tree — the failure path (root →
/// failure span) and, when the fallback fired, the tail N rounds. `Span` is
/// already `Serialize`, but it's large and tree-shaped; this summary keeps
/// only what a judge / a human reading the report needs.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TraceSlice {
    /// Root → failure span, in order.
    pub failure_path: Vec<SpanSummary>,
    /// Tail-N-rounds fallback slice (populated only when the heuristic fell
    /// back — i.e. no param touched the failure path).
    pub tail_rounds: Vec<SpanSummary>,
    /// True iff the fallback fired (suspect_params == all candidate params).
    pub used_fallback: bool,
}

/// A one-span summary.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanSummary {
    pub kind: String,
    pub name: String,
    pub status: String,
    pub duration_ms: Option<u64>,
    /// `tool.name` attribute, if any (only set on `TOOL` spans).
    pub tool_name: Option<String>,
}

impl SpanSummary {
    fn from_span(s: &Span) -> Self {
        let tool_name = s
            .attributes
            .get("tool.name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Self {
            kind: format!("{:?}", s.kind),
            name: s.name.clone(),
            status: format!("{:?}", s.status),
            duration_ms: s.duration_ms,
            tool_name,
        }
    }
}

// ─── Diagnosis ───────────────────────────────────────────────────────────

/// The diagnosis of one failed case — E2's output, E3's input.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnosis {
    pub case_id: String,
    /// Params that plausibly influenced the failing output. E3 varies only
    /// these (or all candidate params when `subtrace.used_fallback`).
    pub suspect_params: Vec<ParamRef>,
    /// The minimal causal subtrace (failure path, + tail rounds on fallback).
    pub subtrace: TraceSlice,
    /// Natural-language attribution. Heuristic-templated by default; an
    /// `LlmDiagnostician` with a judge rewrites it.
    pub critique: String,
}

impl Diagnosis {
    /// Construct a `Diagnosis`. External crates (incl. integration tests)
    /// can't use a struct literal — `Diagnosis` is `#[non_exhaustive]`.
    pub fn new(
        case_id: impl Into<String>,
        suspect_params: Vec<ParamRef>,
        subtrace: TraceSlice,
        critique: impl Into<String>,
    ) -> Self {
        Self {
            case_id: case_id.into(),
            suspect_params,
            subtrace,
            critique: critique.into(),
        }
    }
}

// ─── SubgraphDiagnostician trait ─────────────────────────────────────────

/// Attribute a failed case to suspect variation params by walking its span
/// tree. The default [`HeuristicDiagnostician`] is deterministic (no LLM);
/// [`LlmDiagnostician`] enriches the critique via a judge provider.
#[async_trait]
pub trait SubgraphDiagnostician: Send + Sync {
    /// Produce a `Diagnosis` for `fc`. Must never stall — implementations
    /// fall back to tail-N-rounds + all-candidates-suspect when influence
    /// attribution yields nothing.
    async fn diagnose(&self, fc: &FailedCase<'_>) -> Diagnosis;
}

// ─── HeuristicDiagnostician ──────────────────────────────────────────────

/// Deterministic, LLM-free diagnostician — the loop's default. Implements the
/// full §3.3 难点 A algorithm (influence map → reverse-BFS failure path →
/// suspect intersection → tail-N fallback).
pub struct HeuristicDiagnostician;

#[async_trait]
impl SubgraphDiagnostician for HeuristicDiagnostician {
    async fn diagnose(&self, fc: &FailedCase<'_>) -> Diagnosis {
        diagnose_heuristic(fc)
    }
}

// ─── LlmDiagnostician ────────────────────────────────────────────────────

/// Wraps the heuristic core + an optional LLM judge. When a judge is present,
/// the judge rewrites `critique` in natural language (mirroring
/// `LlmJudgeMetric`'s provider call). Without a judge, the heuristic critique
/// is kept verbatim — so this is a strict superset of the heuristic path and
/// is safe as the loop default when a judge hasn't been wired (E5 injects one).
pub struct LlmDiagnostician {
    judge: Option<Arc<dyn LlmProvider>>,
}

impl LlmDiagnostician {
    /// Construct with a judge provider (E5 wires a stronger/different-family
    /// model per design §6.3).
    pub fn new(judge: Arc<dyn LlmProvider>) -> Self {
        Self { judge: Some(judge) }
    }

    /// Construct without a judge — degrades to the heuristic critique.
    pub fn without_provider() -> Self {
        Self { judge: None }
    }
}

#[async_trait]
impl SubgraphDiagnostician for LlmDiagnostician {
    async fn diagnose(&self, fc: &FailedCase<'_>) -> Diagnosis {
        let mut d = diagnose_heuristic(fc);
        if let Some(judge) = &self.judge {
            if let Ok(text) = judge_critique(judge, fc, &d).await {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    d.critique = trimmed.to_string();
                }
            }
        }
        d
    }
}

// ─── core heuristic ──────────────────────────────────────────────────────

/// The shared core: candidate enumeration → influence map → failure path →
/// suspect intersection, with the tail-N fallback. Used by both
/// [`HeuristicDiagnostician`] and [`LlmDiagnostician`] so the LLM only
/// rewrites prose, never the structural attribution.
pub fn diagnose_heuristic(fc: &FailedCase<'_>) -> Diagnosis {
    let case_id = fc.case.id.clone();
    let candidates = candidate_params(fc.candidate);
    let tree = fc.trace_tree;

    // Failure span: last LLM span in pre-order DFS; else deepest leaf.
    let failure_span = failure_span(tree.root()).unwrap_or_else(|| tree.root());
    let failure_path = ancestry_of(tree.root(), &failure_span.span_id);

    // Suspect = candidate params whose affected spans hit the failure path.
    let path_ids: HashSet<String> = failure_path.iter().map(|s| s.span_id.clone()).collect();
    let mut suspect: Vec<ParamRef> = candidates
        .iter()
        .filter(|p| {
            affected_spans(p, tree)
                .iter()
                .any(|s| path_ids.contains(&s.span_id))
        })
        .cloned()
        .collect();

    // Fallback (§3.3 难点 A): no param touches the failure path → tail N
    // rounds + all candidates suspect. Guarantees a non-empty Diagnosis.
    let mut subtrace = TraceSlice {
        failure_path: failure_path
            .iter()
            .map(|&s| SpanSummary::from_span(s))
            .collect(),
        tail_rounds: Vec::new(),
        used_fallback: false,
    };
    if suspect.is_empty() {
        let tail = tail_n_rounds(tree.root(), TAIL_N_ROUNDS);
        subtrace.tail_rounds = tail.iter().map(|&s| SpanSummary::from_span(s)).collect();
        subtrace.used_fallback = true;
        suspect = candidates;
    }

    let critique = heuristic_critique(fc, &suspect, subtrace.used_fallback);

    Diagnosis {
        case_id,
        suspect_params: suspect,
        subtrace,
        critique,
    }
}

/// Tail-N-rounds window for the fallback slice.
const TAIL_N_ROUNDS: usize = 3;

/// Enumerate the candidate `ParamRef`s actually set in `cfg` — the universe
/// the influence map + suspect intersection consider. A field that's empty /
/// `None` is not a candidate (nothing to vary).
pub fn candidate_params(cfg: &CandidateConfig) -> Vec<ParamRef> {
    let mut v = Vec::new();
    let pc = &cfg.pack_config;

    if !pc.system_prompt.is_empty() {
        v.push(ParamRef::PackSystemPrompt);
    }
    for t in &pc.tools {
        v.push(ParamRef::PackTool(t.clone()));
    }
    for name in pc.tool_decorators.keys() {
        v.push(ParamRef::PackToolDecorator(name.clone()));
    }
    for cs in &pc.context_sources {
        v.push(ParamRef::PackContextSource(cs.clone()));
    }
    for i in 0..pc.paradigm_strategies.len() {
        v.push(ParamRef::PackParadigmTrigger(i));
    }
    for i in 0..pc.compression_template.preserve_fields.len() {
        v.push(ParamRef::PackCompressionField(i));
    }

    // Loop overlay — only the knobs actually set.
    let g = &cfg.loop_overlay;
    if g.thinking_budget.is_some() {
        v.push(ParamRef::LoopThinkingBudget);
    }
    if g.hard_max_iterations.is_some() {
        v.push(ParamRef::LoopHardMaxIterations);
    }
    if g.token_budget.is_some() {
        v.push(ParamRef::LoopTokenBudget);
    }

    // Memory axis (E0 spec-ified). Non-default-ish memory → candidate. Only
    // consider these when memory is actually armed (enable_memory_tools or a
    // non-zero core budget); otherwise they'd never have RETRIEVER spans to
    // affect and would just be fallback-suspect noise.
    let mp = &pc.memory_profile;
    let memory_armed = mp.enable_memory_tools || mp.core_budget_tokens > 0;
    if memory_armed {
        if !mp.extraction_schema.is_empty() {
            v.push(ParamRef::PackExtractionSchema);
        }
        v.push(ParamRef::PackRecall);
        v.push(ParamRef::PackRecallTopK);
        v.push(ParamRef::PackDecay);
        v.push(ParamRef::PackWorkingStateCompaction);
    }

    // Permission axis — low priority, never suspect in headless eval (no
    // APPROVAL spans). Listed for completeness; E5 may gate these out.
    for p in &pc.permission_profile.auto_approve {
        v.push(ParamRef::PackPermission(p.clone()));
    }

    // Skill text patches.
    for s in &cfg.skill_overrides {
        v.push(ParamRef::SkillText(s.clone()));
    }

    v
}

/// Influence map: the spans a param plausibly affects. Matches design §3.3
/// 难点 A step 1 — `PackSystemPrompt`→all `LLM`; `PackTool(name)`→`TOOL`
/// with matching `tool.name`; memory→`RETRIEVER`; permission→`APPROVAL`; etc.
fn affected_spans<'a>(param: &ParamRef, tree: &'a TraceTree) -> Vec<&'a Span> {
    let root = tree.root();
    match param {
        ParamRef::PackSystemPrompt
        | ParamRef::LoopThinkingBudget
        | ParamRef::LoopTokenBudget
        | ParamRef::PackContextSource(_) => root.spans_by_kind(SpanKind::LLM),

        ParamRef::PackTool(name) | ParamRef::PackToolDecorator(name) => root
            .spans_by_kind(SpanKind::TOOL)
            .into_iter()
            .filter(|s| tool_name_attr(s).as_deref() == Some(name.as_str()))
            .collect(),

        ParamRef::PackParadigmTrigger(_) | ParamRef::LoopHardMaxIterations => {
            root.spans_by_kind(SpanKind::AGENT)
        }
        ParamRef::PackCompressionField(_) => root.spans_by_kind(SpanKind::PARSER),

        ParamRef::PackExtractionSchema
        | ParamRef::PackRecall
        | ParamRef::PackRecallTopK
        | ParamRef::PackDecay
        | ParamRef::PackWorkingStateCompaction => root.spans_by_kind(SpanKind::RETRIEVER),

        ParamRef::PackPermission(_) => root.spans_by_kind(SpanKind::APPROVAL),

        // Skill text has no dedicated span kind; it influences the LLM spans
        // it's injected into. Map to LLM so a skill patch can be a suspect.
        ParamRef::SkillText(_) => root.spans_by_kind(SpanKind::LLM),
    }
}

fn tool_name_attr(s: &Span) -> Option<String> {
    s.attributes
        .get("tool.name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// The failure span — the last `LLM` span in pre-order DFS (chronological:
/// spans are appended as work happens). If no `LLM` span ran (e.g. the
/// loop errored before any inference), fall back to the deepest leaf.
fn failure_span(root: &Span) -> Option<&Span> {
    let llm_spans: Vec<&Span> = root.spans_by_kind(SpanKind::LLM);
    if let Some(last) = llm_spans.last() {
        return Some(last);
    }
    deepest_leaf(root)
}

/// Deepest leaf in DFS pre-order (the last span chronologically when no LLM
/// span exists).
fn deepest_leaf(root: &Span) -> Option<&Span> {
    fn walk<'a>(s: &'a Span, acc: &mut Option<&'a Span>) {
        acc.get_or_insert_with(|| s);
        if let Some(last_child) = s.children.last() {
            walk(last_child, acc);
        } else {
            *acc = Some(s);
        }
    }
    let mut acc = None;
    walk(root, &mut acc);
    acc
}

/// Ancestry chain root → `target_id` (inclusive of both ends). Returns the
/// root alone if `target_id` isn't found (defensive — the failure span came
/// from the same tree, so this only fires on a logic bug, not in practice).
fn ancestry_of<'a>(root: &'a Span, target_id: &str) -> Vec<&'a Span> {
    fn walk<'a>(s: &'a Span, target: &str, path: &mut Vec<&'a Span>) -> bool {
        path.push(s);
        if s.span_id == target {
            return true;
        }
        for child in &s.children {
            if walk(child, target, path) {
                return true;
            }
        }
        path.pop();
        false
    }
    let mut path = Vec::new();
    walk(root, target_id, &mut path);
    path
}

/// Tail-N-rounds fallback slice: the last N `LLM` spans + their sibling `TOOL`
/// spans, pre-order. Used when influence attribution yields no suspect.
fn tail_n_rounds(root: &Span, n: usize) -> Vec<&Span> {
    // Flatten pre-order, keep LLM + TOOL spans, take the last n of each round.
    let mut flat: Vec<&Span> = Vec::new();
    flatten_llm_tool(root, &mut flat);
    let take = flat.len().saturating_sub(0).min(n * 4); // ~4 spans/round cap
    let start = flat.len().saturating_sub(take);
    flat[start..].to_vec()
}

fn flatten_llm_tool<'a>(s: &'a Span, out: &mut Vec<&'a Span>) {
    if matches!(s.kind, SpanKind::LLM | SpanKind::TOOL) {
        out.push(s);
    }
    for child in &s.children {
        flatten_llm_tool(child, out);
    }
}

/// Templated natural-language critique. Deterministic; an `LlmDiagnostician`
/// with a judge rewrites it.
fn heuristic_critique(fc: &FailedCase<'_>, suspect: &[ParamRef], fallback: bool) -> String {
    let expected = expected_brief(&fc.case.expected);
    let suspect_paths: Vec<String> = suspect.iter().map(|p| p.path()).collect();
    if fallback {
        format!(
            "Case '{}' failed: output did not match the expected {}. No single \
             variation parameter's influence span intersected the failing inference; \
             suspect set is the full candidate universe ({}). Review the last {} \
             rounds in the subtrace — this is the Reflexion-style fallback attribution.",
            fc.case.id,
            expected,
            suspect_paths.join(", "),
            TAIL_N_ROUNDS,
        )
    } else {
        format!(
            "Case '{}' failed: output did not match the expected {}. The failing \
             inference's ancestry intersects the influence spans of: {}. These are the \
             variation parameters most likely to have shaped the wrong output.",
            fc.case.id,
            expected,
            suspect_paths.join(", "),
        )
    }
}

// ─── LLM judge critique ──────────────────────────────────────────────────

/// Ask the judge provider for a 1–2 sentence natural-language critique of why
/// the case failed, given the suspect params + subtrace. Mirrors
/// `LlmJudgeMetric`'s call shape (`builtin_metrics.rs:455`). On any error or
/// empty response, the caller keeps the heuristic critique.
async fn judge_critique(
    judge: &Arc<dyn LlmProvider>,
    fc: &FailedCase<'_>,
    d: &Diagnosis,
) -> Result<String, String> {
    let suspect_paths: Vec<String> = d.suspect_params.iter().map(|p| p.path()).collect();
    let subtrace_json = serde_json::to_string_pretty(&d.subtrace)
        .unwrap_or_else(|_| "<unserializable subtrace>".to_string());
    let prompt = format!(
        "You are diagnosing why an AI agent failed an eval case. Attribute the \
         failure to the most likely configuration parameters.\n\n\
         Case ID: {}\nInput: {}\nExpected: {}\nActual output: {}\n\n\
         Suspect variation parameters (influence intersected the failing inference):\n{}\n\n\
         Minimal causal subtrace (span summaries, root → failure):\n{}\n\n\
         Write 1–2 sentences naming which parameter(s) most plausibly caused the \
         failure and why. Do not restate the input; attribute.",
        fc.case.id,
        fc.case.input,
        expected_brief(&fc.case.expected),
        fc.result.actual_output,
        suspect_paths.join(", "),
        subtrace_json,
    );

    let mut conversation = oneai_core::Conversation::new();
    conversation.add_message(oneai_core::Message::user(prompt));
    let request = oneai_core::InferenceRequest {
        conversation,
        tools: vec![],
        max_tokens: Some(256),
        temperature: Some(0.0),
        top_p: None,
        stop_sequences: vec![],
        constrained_output: None,
        thinking_budget: None,
        metadata: std::collections::HashMap::new(),
    };
    let response = judge
        .infer(request)
        .await
        .map_err(|e| format!("judge infer: {e}"))?;
    Ok(response.message.text_content())
}

fn expected_brief(expected: &oneai_eval::ExpectedOutput) -> String {
    match expected {
        oneai_eval::ExpectedOutput::Exact { answer } => format!("exact {:?}", answer),
        oneai_eval::ExpectedOutput::Contains { substrings, .. } => {
            format!("contains {:?}", substrings)
        }
        oneai_eval::ExpectedOutput::Regex { pattern } => format!("regex {:?}", pattern),
        oneai_eval::ExpectedOutput::LlmJudge { rubric, .. } => format!("[judge rubric] {}", rubric),
        oneai_eval::ExpectedOutput::Trajectory { expected_tools, .. } => {
            format!("trajectory {:?}", expected_tools)
        }
        _ => "<custom>".into(),
    }
}

// ─── TraceTree accessor ─────────────────────────────────────────────────
// `TraceTree.root_span` is a public field in the real `trace` feature; this
// tiny shim future-proofs call sites against a test double that may not expose
// `root_span` directly.
trait TraceTreeExt {
    fn root(&self) -> &Span;
}

impl TraceTreeExt for TraceTree {
    fn root(&self) -> &Span {
        &self.root_span
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::AgentLoopOverlay;
    use oneai_domain::{
        CompressionTemplateConfig, DomainPackConfig, MemoryProfileConfig, PermissionProfileConfig,
    };
    use oneai_eval::{EvalCase, EvalResult, ExpectedOutput, Trajectory};
    use oneai_trace::{Span, SpanKind, SpanStatus, TraceTree};

    fn coding_cfg(prompt: &str) -> DomainPackConfig {
        DomainPackConfig {
            name: "coding_seed".into(),
            description: String::new(),
            tools: vec!["read_file".into(), "calculator".into()],
            tool_decorators: std::collections::HashMap::new(),
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
                truncate_rules: std::collections::HashMap::new(),
            },
            system_prompt: prompt.into(),
            memory_profile: MemoryProfileConfig::default(),
        }
    }

    fn empty_tree() -> TraceTree {
        TraceTree {
            trace_id: String::new(),
            metadata: Default::default(),
            root_span: Span::new(SpanKind::SESSION, "session", None),
            metrics: Default::default(),
        }
    }

    /// Build a tree shaped like a ReAct loop that ended on a wrong final
    /// answer: SESSION → [AGENT → [LLM(“thought”, wrong answer)]].
    fn tree_with_one_llm() -> TraceTree {
        let mut t = empty_tree();
        let mut agent = Span::new(SpanKind::AGENT, "react", Some(&t.root_span.span_id));
        let mut llm = Span::new(SpanKind::LLM, "infer", Some(&agent.span_id));
        llm.end(SpanStatus::Ok);
        agent.children.push(llm);
        agent.end(SpanStatus::Ok);
        t.root_span.children.push(agent);
        t
    }

    fn failed_case<'a>(
        cfg: &'a CandidateConfig,
        tree: &'a TraceTree,
        case: &'a EvalCase,
        result: &'a EvalResult,
        trajectory: &'a Trajectory,
    ) -> FailedCase<'a> {
        FailedCase {
            case,
            result,
            trajectory,
            trace_tree: tree,
            candidate: cfg,
        }
    }

    fn make_case() -> EvalCase {
        EvalCase::with_id("math_add", "What is 2+2?", ExpectedOutput::exact("4"))
    }

    fn make_result() -> EvalResult {
        let mut r = EvalResult::new("math_add", "What is 2+2?", "5");
        r.add_score("exact", oneai_eval::EvalScore::zero("got 5, want 4"));
        r
    }

    fn make_trajectory() -> Trajectory {
        Trajectory {
            input: "What is 2+2?".into(),
            responses: vec![],
            recorded_tool_calls: vec![],
            recorded_iterations: 1,
        }
    }

    #[test]
    fn candidate_params_covers_prompt_tools_and_memory_for_coding_seed() {
        let cfg = CandidateConfig::from_pack_config(coding_cfg("You are a coding agent."));
        let params = candidate_params(&cfg);
        assert!(params.contains(&ParamRef::PackSystemPrompt));
        assert!(params.contains(&ParamRef::PackTool("calculator".into())));
        assert!(params.contains(&ParamRef::PackTool("read_file".into())));
        // memory armed (default enable_memory_tools) → recall/decay/compaction
        // candidates; extraction_schema empty → not a candidate.
        assert!(params.contains(&ParamRef::PackRecall));
        assert!(!params.contains(&ParamRef::PackExtractionSchema));
    }

    #[test]
    fn diagnose_attributes_llm_failure_to_system_prompt() {
        let cfg = CandidateConfig::from_pack_config(coding_cfg("Answer directly."));
        let tree = tree_with_one_llm();
        let case = make_case();
        let result = make_result();
        let trajectory = make_trajectory();
        let fc = failed_case(&cfg, &tree, &case, &result, &trajectory);

        let d = diagnose_heuristic(&fc);
        assert_eq!(d.case_id, "math_add");
        // PackSystemPrompt affects all LLM spans; the failing LLM span is on
        // the failure path → suspect. Tools have no TOOL spans → not suspect.
        assert!(
            d.suspect_params.contains(&ParamRef::PackSystemPrompt),
            "PackSystemPrompt should be suspect, got {:?}",
            d.suspect_params
        );
        assert!(
            !d.suspect_params
                .contains(&ParamRef::PackTool("calculator".into())),
            "calculator has no TOOL span → should not be suspect"
        );
        assert!(!d.subtrace.used_fallback);
        assert!(!d.subtrace.failure_path.is_empty());
        assert!(d.critique.contains("math_add"));
    }

    #[test]
    fn diagnose_falls_back_when_no_param_touches_failure_path() {
        // Empty tree → no LLM/TOOL/RETRIEVER spans → every candidate's
        // affected set is empty → suspect intersection is empty → fallback
        // fires: all candidates suspect.
        let cfg = CandidateConfig::from_pack_config(coding_cfg("Answer directly."));
        let tree = empty_tree();
        let case = make_case();
        let result = make_result();
        let trajectory = make_trajectory();
        let fc = failed_case(&cfg, &tree, &case, &result, &trajectory);

        let d = diagnose_heuristic(&fc);
        assert!(d.subtrace.used_fallback);
        assert!(
            !d.suspect_params.is_empty(),
            "fallback marks all candidates"
        );
        assert!(d.critique.contains("Reflexion-style fallback"));
    }

    #[test]
    fn overlay_knobs_become_candidates_when_set() {
        let cfg =
            CandidateConfig::from_pack_config(coding_cfg("p")).with_overlay(AgentLoopOverlay {
                thinking_budget: Some(1024),
                token_budget: Some(8000),
                ..Default::default()
            });
        let params = candidate_params(&cfg);
        assert!(params.contains(&ParamRef::LoopThinkingBudget));
        assert!(params.contains(&ParamRef::LoopTokenBudget));
        // hard_max_iterations unset → not a candidate
        assert!(!params.contains(&ParamRef::LoopHardMaxIterations));
    }

    #[test]
    fn paramref_paths_are_stable() {
        assert_eq!(ParamRef::PackSystemPrompt.path(), "pack.system_prompt");
        assert_eq!(
            ParamRef::PackTool("calculator".into()).path(),
            "pack.tools[calculator]"
        );
        assert_eq!(ParamRef::LoopTokenBudget.path(), "loop.token_budget");
    }

    #[test]
    fn paramref_from_path_roundtrips_first_axis() {
        // E3 first-axis paths round-trip.
        assert_eq!(
            ParamRef::from_path("pack.system_prompt"),
            Some(ParamRef::PackSystemPrompt)
        );
        assert_eq!(
            ParamRef::from_path("pack.tool_decorators[calculator]"),
            Some(ParamRef::PackToolDecorator("calculator".into()))
        );
        assert_eq!(
            ParamRef::from_path("pack.tools[read_file]"),
            Some(ParamRef::PackTool("read_file".into()))
        );
        // Non-first-axis + garbage → None (operator drops the patch).
        assert_eq!(ParamRef::from_path("pack.memory.recall"), None);
        assert_eq!(ParamRef::from_path("pack.tools["), None);
        assert_eq!(ParamRef::from_path("nonsense"), None);
    }
}
