//! `TrajectoryCollector` — drives the AgentLoop per eval case and captures the
//! **per-case** `(Trajectory, TraceTree)` + `EvalResult`, which `EvalRunner`
//! deliberately discards (it only keeps scored `EvalResult`s; the span tree
//! is consumed for metric scoring then dropped, and the `--record` CLI path
//! records only the first case with empty `recorded_tool_calls` because the
//! tree isn't surfaced). E1 fills exactly that gap.
//!
//! It mirrors `EvalRunner::run_agent_for_case` (the canonical, tested scoring
//! loop) — same session isolation, same `TraceMetrics::compute_from_tree` +
//! `EfficiencyProfile::from_tree` wiring, same `UsageTracker` accounting — and
//! adds two captures:
//!
//! 1. **Trajectory**: the recorder (a `RecordingProvider` wrapping the real
//!    provider, shared as the App's `Arc<dyn LlmProvider>`) accumulates every
//!    `infer()` response across cases. The collector snapshots the recorder
//!    before/after each case and slices `responses[cursor..]` so each case's
//!    trajectory holds *only* its own responses. Tool names + iteration count
//!    come from the span tree (`SpanKind::TOOL` spans' `tool.name` attribute +
//!    `AgentLoopResult.iterations`).
//! 2. **TraceTree**: `session.trace_context().build_tree()` kept whole, so E2's
//!    `SubgraphDiagnostician` can walk it for minimal-subgraph extraction.
//!
//! All semantic variation candidates (system_prompt / temperature / tools /
//! decorators / recall / compression) require a *live* re-run — the frozen
//! responses a `ReplayProvider` would re-emit belong to the *old* config. So the
//! collector always drives the live loop; replay is reserved for the E5
//! regression gate (design §3.3 难点 C).

use std::sync::Arc;
use std::time::Instant;

use oneai_app::App;
use oneai_eval::{
    EfficiencyProfile, EvalCase, EvalMetric, EvalResult, RecordingProvider, Trajectory,
};
use oneai_trace::{Span, SpanKind, TraceMetrics, TraceTree};

/// One case's full capture from a generation-0 run.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct CaseRun {
    /// The scored result (output + scores + trace_metrics + efficiency + usage).
    pub result: EvalResult,
    /// Recorded provider responses + tool-call digest for this case.
    pub trajectory: Trajectory,
    /// The full span tree (kept for E2 subgraph diagnosis).
    pub trace_tree: TraceTree,
}

/// Drives the loop per case on a shared `App` (built from a `CandidateConfig`)
/// and a shared `RecordingProvider` (wrapping the real provider), capturing
/// `Trajectory` + `TraceTree` + `EvalResult` per case.
pub struct TrajectoryCollector {
    app: Arc<App>,
    recorder: Arc<RecordingProvider>,
}

impl TrajectoryCollector {
    /// Construct from a shared app + the recorder that wraps its provider.
    ///
    /// The recorder must be the *same* `Arc<dyn LlmProvider>` the `App` was
    /// built with (the [`EvolutionLoop`](crate::EvolutionLoop) guarantees this
    /// by wrapping the baseline provider before `build_app`).
    pub fn new(app: Arc<App>, recorder: Arc<RecordingProvider>) -> Self {
        Self { app, recorder }
    }

    /// Run a single case and capture trajectory + tree + scored result.
    pub async fn run_case(&self, case: &EvalCase, metrics: &[Arc<dyn EvalMetric>]) -> CaseRun {
        let start = Instant::now();
        let mut result = EvalResult::new(&case.id, &case.input, "");

        let tree = if self.app.has_provider() {
            self.run_agent_for_case(case, &mut result).await
        } else {
            result.error = Some("No LLM provider configured".to_string());
            None
        };

        result.duration_ms = start.elapsed().as_millis() as u64;

        // Apply metrics (reuse EvalMetric::score_with_trace — the same protocol
        // EvalRunner uses; trace-aware metrics walk the tree, text-only ones
        // ignore it via the default impl).
        for metric in metrics {
            let score = metric
                .score_with_trace(
                    &case.input,
                    &result.actual_output,
                    &case.expected,
                    tree.as_ref(),
                )
                .await;
            result.add_score(metric.name(), score);
        }

        // Build the per-case trajectory from the recorder slice + tree digest.
        let trajectory = self.build_trajectory(case, tree.as_ref(), &result);

        CaseRun {
            result,
            trajectory,
            trace_tree: tree.unwrap_or_else(empty_tree),
        }
    }

    /// Run the agent loop for one case — mirrors `EvalRunner::run_agent_for_case`
    /// verbatim (session isolation, trace-metrics + efficiency wiring, usage
    /// accounting) but keeps the `TraceTree` for the caller instead of dropping
    /// it after scoring.
    async fn run_agent_for_case(
        &self,
        case: &EvalCase,
        result: &mut EvalResult,
    ) -> Option<TraceTree> {
        let mut session = self.app.create_session();
        let session_id = session.session_id().to_string();
        let mut tree_out: Option<TraceTree> = None;

        // Isolate this case's usage accounting.
        if let Some(ct) = &self.app.usage_tracker {
            let _ = ct.clear_session(&session_id).await;
        }

        // Snapshot the recorder cursor *before* the agent runs so we can slice
        // this case's responses out of the shared accumulator afterward.
        let cursor = self.recorder.recorded_responses().await.len();

        let agent_start = Instant::now();
        let agent_result = session.run_agent_silent(&case.input).await;
        let agent_dur_ms = agent_start.elapsed().as_millis() as u64;

        let iterations = match agent_result {
            Ok(loop_result) => {
                result.actual_output = loop_result.final_answer.clone();

                if let Some(ctx) = session.trace_context() {
                    let tree = ctx.build_tree();
                    result.trace_metrics = TraceMetrics::compute_from_tree(&tree.root_span);
                    result.efficiency = Some(EfficiencyProfile::from_tree(
                        &tree.root_span,
                        agent_dur_ms,
                        0, // total_tokens filled from usage below
                        0,
                        0,
                        result.trace_metrics.avg_iterations.round() as usize,
                    ));
                    tree_out = Some(tree);
                }
                loop_result.iterations
            }
            Err(e) => {
                // Preserve EvalRunner's behavior: embed the error in the output
                // so metrics can still score it (result.error stays None — that
                // path is reserved for "no provider").
                result.actual_output = format!("ERROR: {}", e);
                0
            }
        };

        // Collect the usage axis: api_calls + token breakdown.
        if let Some(ct) = &self.app.usage_tracker {
            if let Ok(summary) = ct.session_usage(&session_id).await {
                result.api_calls = summary.call_count;
                result.estimated_calls = summary.estimated_call_count;
                result.prompt_tokens = summary.prompt_tokens;
                result.completion_tokens = summary.completion_tokens;
                if let Some(p) = result.efficiency.as_mut() {
                    p.total_tokens = summary.prompt_tokens + summary.completion_tokens;
                }
            }
        }

        // Slice this case's responses out of the shared recorder.
        let all_responses = self.recorder.recorded_responses().await;
        let this_case_responses = if cursor <= all_responses.len() {
            all_responses[cursor..].to_vec()
        } else {
            Vec::new()
        };

        // Stash the case-scoped slice + iteration count in the result's
        // metadata so build_trajectory (which only gets `&result`) can read
        // them without re-snapshotting. Avoids a second lock acquisition.
        result.set_metadata("__evolve_iter", iterations.to_string());
        result.set_metadata(
            "__evolve_responses",
            serde_json::to_string(&this_case_responses).unwrap_or_else(|_| "[]".to_string()),
        );

        tree_out
    }

    /// Assemble the per-case [`Trajectory`] from the recorder slice (stashed
    /// in result metadata) + tool-call names (walked from the tree) +
    /// iteration count.
    fn build_trajectory(
        &self,
        case: &EvalCase,
        tree: Option<&TraceTree>,
        result: &EvalResult,
    ) -> Trajectory {
        let responses: Vec<_> = result
            .metadata
            .get("__evolve_responses")
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        let iterations: usize = result
            .metadata
            .get("__evolve_iter")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let tool_calls = tree
            .map(|t| extract_tool_names(&t.root_span))
            .unwrap_or_default();

        Trajectory {
            input: case.input.clone(),
            responses,
            recorded_tool_calls: tool_calls,
            recorded_iterations: iterations,
        }
    }
}

/// Walk the span tree in pre-order DFS, collecting `tool.name` attributes from
/// `SpanKind::TOOL` spans — the chronological tool-call sequence for the
/// trajectory's determinism digest.
fn extract_tool_names(root: &Span) -> Vec<String> {
    root.spans_by_kind(SpanKind::TOOL)
        .iter()
        .filter_map(|s| s.attributes.get("tool.name"))
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

/// Construct an empty `TraceTree` (no provider ran, or tracing off). `TraceTree`
/// has no `Default` in the real `trace` feature module, so build one by hand.
fn empty_tree() -> TraceTree {
    TraceTree {
        trace_id: String::new(),
        metadata: Default::default(),
        root_span: Span::new(SpanKind::SESSION, "empty", None),
        metrics: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_constructs() {
        let t = empty_tree();
        assert_eq!(t.root_span.kind, SpanKind::SESSION);
        assert!(t.root_span.children.is_empty());
    }
}
