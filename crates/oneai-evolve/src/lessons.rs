//! `lessons.rs` — ④ GEPA lesson merge + cross-generation memory (the E4 core).
//!
//! E3 ended at "vary K → score → Pareto-select the frontier" for *one*
//! generation. E4 closes the loop: carry the frontier forward as the next
//! generation's base, persist a per-generation lesson log, and stop on
//! convergence / budget / stagnation.
//!
//! ## LessonMerger — the GEPA "complementary lessons" seam
//!
//! GEPA's headline move is "stitch complementary lessons from the Pareto
//! frontier" — take the member that wins on pass_rate, graft the member that
//! wins on tokens, … → next-gen base. That stitch requires *patch
//! provenance* (which mutation each frontier member applied). [`ScoredCandidate`]
//! carries the full [`CandidateConfig`](crate::candidate::CandidateConfig),
//! not the patch-list, so a faithful stitcher would have to diff configs — and
//! under the deterministic mock the result isn't checkable (two candidates
//! that differ only on a free-text field are equally "complementary"). The
//! honest first cut: the default [`BestFrontierMerger`] picks the frontier-best
//! (highest pass_rate, tie-broken by tokens — exactly the selector's ordering)
//! as the next-gen base, and records a `lessons_text` describing the
//! frontier's axis trade-offs. The `LessonMerger` trait is the seam for a
//! richer stitcher (E5+); it composes with any selector because the frontier
//! is already ordered.
//!
//! ## LessonsLog — cross-generation memory
//!
//! One JSON line per generation under `<run_dir>/lessons.jsonl`:
//! `(generation, base_pass_rate, frontier_pass_rate, frontier_axes,
//! is_seed, lessons_text)`. Survives crashes (append-only), readable by the
//! report renderer + a future `oneai evolve lesson` subcommand.
//!
//! ## Convergence / early-stop (driven by the loop, read from this log)
//!
//! - `frontier_pass_rate ≥ target` → converged.
//! - `generation == max_generations - 1` → cap.
//! - cumulative tokens ≥ `max_total_tokens` → budget cap.
//! - `gens_without_improvement() ≥ early_stop_patience` → stagnation.
//!
//! The convergence check uses the **subset** frontier-best pass_rate (the
//! score the optimizer already computed — no extra live runs). The held-out
//! full-suite convergence gate is E5 (design §4 Phase E5); E4 keeps E3's
//! single-gen cost identical.
//!
//! Design: `docs/self-evolution-system-2026-08.md` §4 Phase E4.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use oneai_core::error::{OneAIError, Result};

use crate::candidate::CandidateConfig;
use crate::gepa::ScoredCandidate;

// ─── LessonEntry + LessonsLog ─────────────────────────────────────────────

/// One generation's lesson record — persisted as a JSONL line in
/// `lessons.jsonl` + mirrored (summary) in the report's `generations` vec.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LessonEntry {
    /// Generation index (0-based).
    pub generation: usize,
    /// The base config's full-suite pass rate this generation (gen-0 seed's
    /// score; subsequent gens = the previous frontier-best carried forward).
    pub base_pass_rate: f64,
    /// The frontier-best **subset** pass rate this generation (the optimizer's
    /// selection signal — what convergence targets).
    pub frontier_pass_rate: f64,
    /// Frontier-best total tokens (subset).
    pub frontier_total_tokens: u64,
    /// Frontier-best total latency ms (subset).
    pub frontier_total_latency_ms: u64,
    /// True iff the frontier-best *is* the base (no candidate improved on it).
    pub frontier_is_seed: bool,
    /// Natural-language summary of the frontier's axis trade-offs + the merge
    /// decision (deterministic — `BestFrontierMerger` templates it).
    pub lessons_text: String,
}

/// Append-only cross-generation log. The loop holds one in memory for the
/// run, persists to `lessons.jsonl` at the end (and could flush per-gen for
/// crash recovery — E4 flushes once at the end, matching `report.json`).
pub struct LessonsLog {
    /// `<run_dir>/lessons.jsonl`.
    path: PathBuf,
    /// In-order entries (one per generation that ran).
    entries: Vec<LessonEntry>,
}

impl LessonsLog {
    /// Construct an empty log rooted at `path` (the caller passes
    /// `<run_dir>/lessons.jsonl`).
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            entries: Vec::new(),
        }
    }

    /// Append a generation's lesson. The loop calls this once per generation
    /// after the optimization + merge step.
    pub fn record(&mut self, entry: LessonEntry) {
        self.entries.push(entry);
    }

    /// The entries accumulated so far (the report reads them to render the
    /// per-generation table).
    pub fn entries(&self) -> &[LessonEntry] {
        &self.entries
    }

    /// The most recent frontier-best pass rate (the convergence/early-stop
    /// signal). `None` before the first generation completes.
    pub fn last_frontier_pass_rate(&self) -> Option<f64> {
        self.entries.last().map(|e| e.frontier_pass_rate)
    }

    /// Trailing run of generations with no strict improvement in the
    /// frontier-best pass rate (≤ the previous). 0 before gen 1. Drives the
    /// `early_stop_patience` gate.
    pub fn gens_without_improvement(&self) -> usize {
        let mut run = 0usize;
        for w in self.entries.windows(2).rev() {
            // windows(2): [prev, curr]. No improvement iff curr <= prev.
            let prev = w[0].frontier_pass_rate;
            let curr = w[1].frontier_pass_rate;
            if curr <= prev {
                run += 1;
            } else {
                break;
            }
        }
        run
    }

    /// Number of generations recorded.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether any generation recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The path this log persists to (`<run_dir>/lessons.jsonl`).
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Persist the entries as JSONL (one JSON object per line). Overwrites —
    /// the log is the authoritative record for this run. Returns the path.
    pub fn persist(&self) -> Result<PathBuf> {
        if self.entries.is_empty() {
            return Ok(self.path.clone());
        }
        let body = self
            .entries
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| OneAIError::Config(format!("serialize lesson entry: {e}")))?
            .join("\n");
        std::fs::write(&self.path, format!("{body}\n"))
            .map_err(|e| OneAIError::Config(format!("write {}: {}", self.path.display(), e)))?;
        Ok(self.path.clone())
    }
}

// ─── LessonMerger trait + BestFrontierMerger ──────────────────────────────

/// Produces the next generation's base from the current Pareto frontier.
/// GEPA's "complementary lessons from the Pareto frontier" (design §3.2
/// `LessonMerger`). The default [`BestFrontierMerger`] is the honest first
/// cut — see the module docs for why a faithful stitcher isn't
/// deterministically testable under the mock.
#[async_trait]
pub trait LessonMerger: Send + Sync {
    /// Merge the frontier into the next-gen base. Returns
    /// `(next_base, lessons_text)`:
    /// - `next_base` — the config to vary + score in generation N+1.
    /// - `lessons_text` — a deterministic natural-language note for the log
    ///   (frontier composition + the merge decision).
    ///
    /// `current_base` is the gen-N base (carried forward when the frontier is
    /// empty — e.g. `no_optimize` runs or all candidates dropped).
    async fn merge(
        &self,
        frontier: &[ScoredCandidate],
        current_base: &CandidateConfig,
    ) -> (CandidateConfig, String);
}

/// Default merger — the frontier-best (selector's ordering: pass_rate desc,
/// tokens asc) becomes the next-gen base. When the frontier is empty, the
/// current base is carried forward unchanged. `lessons_text` describes the
/// frontier's axis trade-offs deterministically.
pub struct BestFrontierMerger;

impl BestFrontierMerger {
    /// Construct.
    pub fn new() -> Self {
        Self
    }
}

impl Default for BestFrontierMerger {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LessonMerger for BestFrontierMerger {
    async fn merge(
        &self,
        frontier: &[ScoredCandidate],
        current_base: &CandidateConfig,
    ) -> (CandidateConfig, String) {
        match frontier.first() {
            Some(best) => {
                let text = if frontier.len() == 1 {
                    format!(
                        "frontier-best: pass={:.2} tokens={} latency={}ms (sole frontier member; carried forward as next-gen base)",
                        best.pass_rate, best.total_tokens, best.total_latency_ms,
                    )
                } else {
                    // Describe the axis spread — which member wins each axis.
                    let pass_leader = frontier
                        .iter()
                        .max_by(|a, b| {
                            a.pass_rate
                                .partial_cmp(&b.pass_rate)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|c| (c.pass_rate, c.total_tokens, c.total_latency_ms));
                    let tok_leader = frontier
                        .iter()
                        .min_by(|a, b| a.total_tokens.cmp(&b.total_tokens))
                        .map(|c| (c.pass_rate, c.total_tokens, c.total_latency_ms));
                    let lat_leader = frontier
                        .iter()
                        .min_by(|a, b| a.total_latency_ms.cmp(&b.total_latency_ms))
                        .map(|c| (c.pass_rate, c.total_tokens, c.total_latency_ms));
                    let fmt = |t: Option<(f64, u64, u64)>| {
                        t.map_or("(none)".into(), |(p, tk, l)| {
                            format!("pass={p:.2} tok={tk} lat={l}ms")
                        })
                    };
                    format!(
                        "frontier size {} — pass-leader {{{}}} / tok-leader {{{}}} / lat-leader {{{}}}; \
                         frontier-best (pass={:.2} tok={} lat={}ms) carried forward as next-gen base",
                        frontier.len(),
                        fmt(pass_leader),
                        fmt(tok_leader),
                        fmt(lat_leader),
                        best.pass_rate,
                        best.total_tokens,
                        best.total_latency_ms,
                    )
                };
                (best.candidate.clone(), text)
            }
            None => (
                current_base.clone(),
                "empty frontier — current base carried forward unchanged".into(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_domain::{CompressionTemplateConfig, DomainPackConfig, PermissionProfileConfig};
    use std::collections::HashMap;

    fn pack(name: &str) -> DomainPackConfig {
        DomainPackConfig {
            name: name.into(),
            description: String::new(),
            tools: vec!["read_file".into()],
            tool_decorators: HashMap::new(),
            context_sources: vec![],
            permission_profile: PermissionProfileConfig {
                auto_approve: vec!["read_file".into()],
                require_confirmation: vec![],
                deny_by_default: vec![],
            },
            paradigm_strategies: vec![],
            compression_template: CompressionTemplateConfig {
                name: "c".into(),
                preserve_fields: vec![],
                truncate_rules: HashMap::new(),
            },
            system_prompt: name.into(),
            memory_profile: Default::default(),
        }
    }

    fn scored(name: &str, pass: f64, tok: u64, lat: u64) -> ScoredCandidate {
        ScoredCandidate {
            candidate: CandidateConfig::from_pack_config(pack(name)),
            pass_rate: pass,
            total_tokens: tok,
            total_latency_ms: lat,
        }
    }

    #[tokio::test]
    async fn merger_picks_frontier_best() {
        let frontier = vec![scored("best", 1.0, 100, 10), scored("cheap", 0.5, 50, 5)];
        let base = CandidateConfig::from_pack_config(pack("base"));
        let (next, text) = BestFrontierMerger::new().merge(&frontier, &base).await;
        assert_eq!(next.pack_config.system_prompt, "best");
        assert!(text.contains("frontier"));
    }

    #[tokio::test]
    async fn merger_empty_frontier_carries_base() {
        let base = CandidateConfig::from_pack_config(pack("base"));
        let (next, text) = BestFrontierMerger::new().merge(&[], &base).await;
        assert_eq!(next.pack_config.system_prompt, "base");
        assert!(text.contains("empty frontier"));
    }

    #[test]
    fn lessons_log_persists_and_counts_stagnation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = LessonsLog::new(tmp.path().join("lessons.jsonl"));
        // gen0 frontier 0.5, gen1 0.75 (improve), gen2 0.75 (flat), gen3 0.75 (flat)
        log.record(LessonEntry {
            generation: 0,
            base_pass_rate: 0.0,
            frontier_pass_rate: 0.5,
            frontier_total_tokens: 1,
            frontier_total_latency_ms: 1,
            frontier_is_seed: false,
            lessons_text: "g0".into(),
        });
        assert_eq!(log.gens_without_improvement(), 0);
        log.record(LessonEntry {
            generation: 1,
            base_pass_rate: 0.5,
            frontier_pass_rate: 0.75,
            frontier_total_tokens: 1,
            frontier_total_latency_ms: 1,
            frontier_is_seed: false,
            lessons_text: "g1".into(),
        });
        assert_eq!(log.gens_without_improvement(), 0); // 0.75 > 0.5
        log.record(LessonEntry {
            generation: 2,
            base_pass_rate: 0.75,
            frontier_pass_rate: 0.75,
            frontier_total_tokens: 1,
            frontier_total_latency_ms: 1,
            frontier_is_seed: false,
            lessons_text: "g2".into(),
        });
        assert_eq!(log.gens_without_improvement(), 1); // 0.75 <= 0.75
        log.record(LessonEntry {
            generation: 3,
            base_pass_rate: 0.75,
            frontier_pass_rate: 0.75,
            frontier_total_tokens: 1,
            frontier_total_latency_ms: 1,
            frontier_is_seed: false,
            lessons_text: "g3".into(),
        });
        assert_eq!(log.gens_without_improvement(), 2);
        assert_eq!(log.last_frontier_pass_rate(), Some(0.75));
        let path = log.persist().unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 4);
    }

    #[test]
    fn lessons_log_empty_persist_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let log = LessonsLog::new(tmp.path().join("lessons.jsonl"));
        assert!(log.is_empty());
        // Empty persist is a no-op (doesn't write a file).
        log.persist().unwrap();
        assert!(!tmp.path().join("lessons.jsonl").exists());
    }
}
