//! `FailureExtractor` — the thinnest possible filter between ② EDD scoring
//! and ③ minimal-subgraph diagnosis. It selects the cases whose `EvalResult`
//! did not pass (either a metric failed or the run errored) and pairs each
//! with everything [`SubgraphDiagnostician`](crate::subgraph::SubgraphDiagnostician)
//! needs to attribute the failure: the scored result, the recorded trajectory,
//! the full span tree (for reverse-BFS subgraph extraction), and a borrow of
//! the [`CandidateConfig`](crate::candidate::CandidateConfig) that produced it
//! (so `ParamRef`s resolve against the real field values).
//!
//! This is deliberately a borrow-passing adapter, not a cloner: E1 already
//! persists the trajectory + report; E2 only *adds* a diagnosis pass over the
//! same in-memory `CaseRun`s the collector just produced. No case is re-run.

use oneai_eval::{EvalCase, EvalResult, Trajectory};
use oneai_trace::TraceTree;

use crate::candidate::CandidateConfig;
use crate::trajectory_collector::CaseRun;

/// One failed case + the borrows the diagnostician reads.
///
/// `'a` ties the case/result/trajectory/tree to the owning `CaseRun` (kept
/// alive in the loop's `Vec<CaseRun>` for the duration of the diagnosis pass),
/// and `candidate` to the loop's `&CandidateConfig` seed.
#[non_exhaustive]
pub struct FailedCase<'a> {
    /// The eval case (input + expected output) — the ground truth the run
    /// missed. The expected output drives the heuristic critique's "what the
    /// agent should have produced" framing.
    pub case: &'a EvalCase,
    /// The scored result (actual output + per-metric scores + usage). A
    /// `passed == false` (or non-empty `error`) is the entry condition.
    pub result: &'a EvalResult,
    /// The recorded provider responses + tool-call digest for this case.
    pub trajectory: &'a Trajectory,
    /// The full span tree — the raw material for reverse-BFS subgraph
    /// extraction. Kept whole by `TrajectoryCollector` exactly for this.
    pub trace_tree: &'a TraceTree,
    /// The candidate config that produced this run — `ParamRef`s resolve
    /// against its `pack_config` / `loop_overlay` / `skill_overrides`.
    pub candidate: &'a CandidateConfig,
}

impl<'a> FailedCase<'a> {
    /// Construct from the borrows the loop already holds.
    pub fn new(case: &'a EvalCase, run: &'a CaseRun, candidate: &'a CandidateConfig) -> Self {
        Self {
            case,
            result: &run.result,
            trajectory: &run.trajectory,
            trace_tree: &run.trace_tree,
            candidate,
        }
    }
}

/// Select failed `CaseRun`s and pair them with their `EvalCase` + the
/// candidate config that produced them.
///
/// `runs` must be in the same order as `cases` (the loop iterates the suite
/// in order and pushes each `CaseRun` to a parallel `Vec`, so a `zip` is
/// exact). A run is "failed" when [`EvalResult::passed`] is false — that
/// covers both metric failures and execution errors (an error embeds in
/// `actual_output` and still gets scored; `result.error` is reserved for the
/// "no provider" path, which also yields `passed == false`).
pub fn extract_failures<'a>(
    cases: &'a [EvalCase],
    runs: &'a [CaseRun],
    candidate: &'a CandidateConfig,
) -> Vec<FailedCase<'a>> {
    cases
        .iter()
        .zip(runs.iter())
        .filter(|(_, run)| !run.result.passed())
        .map(|(case, run)| FailedCase::new(case, run, candidate))
        .collect()
}

/// The thin filter façade — design §3 names it "FailureExtractor (极薄)". A
/// unit struct so callers read `FailureExtractor::extract(...)` at the call
/// site; the work is the free function above.
pub struct FailureExtractor;

impl FailureExtractor {
    /// Select failures (delegates to [`extract_failures`]).
    pub fn extract<'a>(
        cases: &'a [EvalCase],
        runs: &'a [CaseRun],
        candidate: &'a CandidateConfig,
    ) -> Vec<FailedCase<'a>> {
        extract_failures(cases, runs, candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_domain::{DomainPackConfig, PermissionProfileConfig};
    use oneai_eval::{EvalCase, ExpectedOutput};

    fn fake_run(passed: bool) -> CaseRun {
        // Build a CaseRun with the minimal fields the extractor reads. The
        // extractor only touches `run.result.passed()` + the borrows, so we
        // construct a tiny passed/failed result against an empty tree.
        let mut result = EvalResult::new("c1", "in", "out");
        let score = if passed {
            oneai_eval::EvalScore::perfect("ok")
        } else {
            oneai_eval::EvalScore::zero("mismatch")
        };
        result.add_score("exact", score);
        let trajectory = Trajectory {
            input: "in".to_string(),
            responses: vec![],
            recorded_tool_calls: vec![],
            recorded_iterations: 1,
        };
        let trace_tree = TraceTree {
            trace_id: String::new(),
            metadata: Default::default(),
            root_span: oneai_trace::Span::new(oneai_trace::SpanKind::SESSION, "s", None),
            metrics: Default::default(),
        };
        CaseRun {
            result,
            trajectory,
            trace_tree,
        }
    }

    fn empty_config() -> DomainPackConfig {
        DomainPackConfig {
            name: "t".into(),
            description: String::new(),
            tools: vec![],
            tool_decorators: std::collections::HashMap::new(),
            context_sources: vec![],
            permission_profile: PermissionProfileConfig {
                auto_approve: vec![],
                require_confirmation: vec![],
                deny_by_default: vec![],
            },
            paradigm_strategies: vec![],
            compression_template: Default::default(),
            system_prompt: String::new(),
            memory_profile: Default::default(),
        }
    }

    #[test]
    fn extracts_only_failures_in_order() {
        let cases = vec![
            EvalCase::with_id("c0", "q0", ExpectedOutput::exact("a0")),
            EvalCase::with_id("c1", "q1", ExpectedOutput::exact("a1")),
            EvalCase::with_id("c2", "q2", ExpectedOutput::exact("a2")),
        ];
        let runs = vec![fake_run(true), fake_run(false), fake_run(false)];
        let cfg = CandidateConfig::from_pack_config(empty_config());
        let fails = extract_failures(&cases, &runs, &cfg);
        assert_eq!(fails.len(), 2);
        assert_eq!(fails[0].case.id, "c1");
        assert_eq!(fails[1].case.id, "c2");
        assert!(!fails[0].result.passed());
    }
}
