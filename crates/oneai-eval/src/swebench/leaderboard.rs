//! SWE-bench leaderboard serialization + prediction export (Step 4).
//!
//! The swebench.com leaderboard submission schema is an aggregate usage block
//! plus a per-instance block. The comparable quantity is `api_calls`;
//! `resolved` comes from the harness. We export exactly those — internal
//! quantities like step count / tool calls stay in `TraceMetrics` for internal
//! optimization and are NOT emitted, since the leaderboard doesn't standardize
//! them (see [[swe-bench-eval-three-axis]].
//!
//! (USD cost fields were removed — OneAI no longer tracks dollar amounts.)
//!
//! This is a module-level function rather than an `EvalReport` method to keep
//! the SWE-bench schema out of the generic report type.

use std::path::{Path, PathBuf};

use crate::eval_result::EvalReport;

/// Per-instance leaderboard entry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LeaderboardInstance {
    pub instance_id: String,
    pub api_calls: u64,
    pub resolved: bool,
}

/// Aggregate + per-instance leaderboard record, matching the swebench.com
/// submission schema.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SwebenchLeaderboard {
    /// Total API calls across all instances (= leaderboard `instance_calls`).
    pub instance_calls: u64,
    /// Number of instances resolved.
    pub resolved_count: usize,
    /// Total instances evaluated.
    pub total_instances: usize,
    /// Resolution rate (resolved_count / total_instances).
    pub resolution_rate: f64,
    /// Per-instance breakdown.
    pub per_instance: Vec<LeaderboardInstance>,
}

/// The metric name the `SwebenchRunner` records the `resolved` verdict under.
pub const SWEBENCH_RESOLVED_METRIC: &str = "swebench_resolved";

/// Build a leaderboard record from a completed `EvalReport`.
///
/// `resolved` per instance is read from the `swebench_resolved` metric score
/// (falling back to `result.passed()` if absent). `api_calls` come from the
/// usage axis fields populated in Step 1+2.
pub fn render_swebench_leaderboard(report: &EvalReport) -> SwebenchLeaderboard {
    let per_instance: Vec<LeaderboardInstance> = report
        .results
        .iter()
        .map(|r| LeaderboardInstance {
            instance_id: r.case_id.clone(),
            api_calls: r.api_calls,
            resolved: r.metric_passed(SWEBENCH_RESOLVED_METRIC) || r.passed(),
        })
        .collect();

    let instance_calls: u64 = per_instance.iter().map(|i| i.api_calls).sum();
    let resolved_count = per_instance.iter().filter(|i| i.resolved).count();
    let total_instances = per_instance.len();
    let resolution_rate = if total_instances > 0 {
        resolved_count as f64 / total_instances as f64
    } else {
        0.0
    };

    SwebenchLeaderboard {
        instance_calls,
        resolved_count,
        total_instances,
        resolution_rate,
        per_instance,
    }
}

/// Re-emit the agent patches stored in `result.metadata["patch"]` as a standard
/// SWE-bench prediction JSONL (one line per instance). Makes a run
/// reproducible / submittable to the harness independently of the eval report.
pub fn write_prediction_jsonl(report: &EvalReport, path: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    let mut lines = String::new();
    for r in &report.results {
        let patch = r.metadata.get("patch").cloned().unwrap_or_default();
        // Skip instances that produced no patch — an empty model_patch line
        // would just be a no-op for the harness, so don't emit it.
        if patch.trim().is_empty() {
            continue;
        }
        let record = serde_json::json!({
            "instance_id": r.case_id,
            "model_name_or_path": "oneai",
            "model_patch": patch,
        });
        lines.push_str(&serde_json::to_string(&record).map_err(|e| e.to_string())?);
        lines.push('\n');
        count += 1;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create output dir: {}", e))?;
    }
    std::fs::write(path, lines).map_err(|e| format!("cannot write predictions: {}", e))?;
    Ok(count)
}

/// Persist the full eval report (issue #22): `report.json` (machine-readable)
/// + `report.md` (human-readable) into the workspace.
///
/// Previously the swebench CLI only *printed* the report — a long smoke run
/// scrolled it off the terminal and the run became un-复盘-able. Returns the
/// path to `report.json` (the machine-readable copy). `report.md` sits next to
/// it. A write failure of either file is surfaced as an error rather than
/// silently dropping half the artifact.
pub fn write_report(report: &EvalReport, workspace: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(workspace).map_err(|e| format!("cannot create workspace: {e}"))?;
    let json_path = workspace.join("report.json");
    let md_path = workspace.join("report.md");
    let json = report
        .to_json()
        .map_err(|e| format!("serialize report: {e}"))?;
    std::fs::write(&json_path, &json).map_err(|e| format!("write {}: {e}", json_path.display()))?;
    std::fs::write(&md_path, report.to_markdown())
        .map_err(|e| format!("write {}: {e}", md_path.display()))?;
    Ok(json_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval_metric::EvalScore;
    use crate::eval_result::EvalResult;

    fn result(id: &str, resolved: bool, calls: u64, patch: &str) -> EvalResult {
        let mut r = EvalResult::new(id, "issue", patch);
        r.api_calls = calls;
        r.add_score(
            SWEBENCH_RESOLVED_METRIC,
            EvalScore::from_bool(resolved, "verdict"),
        );
        r.set_metadata("patch", patch);
        r
    }

    #[test]
    fn test_render_leaderboard_aggregates() {
        let report = EvalReport::new(
            "swebench",
            vec![
                result("a", true, 3, "diff a"),
                result("b", false, 7, "diff b"),
            ],
        );
        let lb = render_swebench_leaderboard(&report);

        assert_eq!(lb.total_instances, 2);
        assert_eq!(lb.resolved_count, 1);
        assert!((lb.resolution_rate - 0.5).abs() < 1e-9);
        assert_eq!(lb.instance_calls, 10);
        assert!(lb.per_instance[0].resolved);
        assert!(!lb.per_instance[1].resolved);
        assert_eq!(lb.per_instance[0].instance_id, "a");
    }

    #[test]
    fn test_render_leaderboard_empty() {
        let report = EvalReport::new("swebench", vec![]);
        let lb = render_swebench_leaderboard(&report);
        assert_eq!(lb.total_instances, 0);
        assert_eq!(lb.resolution_rate, 0.0);
    }

    #[test]
    fn test_write_prediction_jsonl_roundtrip() {
        let report = EvalReport::new(
            "swebench",
            vec![
                result("a", true, 1, "diff a"),
                result("b", false, 2, ""), // no patch — skipped
            ],
        );
        let dir = std::env::temp_dir().join(format!(
            "oneai_swebench_pred_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let path = dir.join("predictions.jsonl");
        let n = write_prediction_jsonl(&report, &path).unwrap();
        assert_eq!(n, 1);

        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(text.contains("\"instance_id\":\"a\""));
        assert!(text.contains("\"model_patch\":\"diff a\""));
        assert!(!text.contains("\"instance_id\":\"b\""));
    }

    #[test]
    fn test_write_report_writes_json_and_markdown() {
        // Issue #22: the eval report must land on disk (not just stdout).
        let report = EvalReport::new("swebench", vec![result("a", true, 3, "diff a")]);
        let dir = std::env::temp_dir().join(format!(
            "oneai_swebench_report_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let json_path = write_report(&report, &dir).expect("write_report ok");

        // Both artifacts land in the workspace.
        assert_eq!(json_path, dir.join("report.json"));
        assert!(dir.join("report.md").exists());
        // The JSON is a faithful serialization of the report (pretty-printed,
        // so check the field name + a distinctive value rather than a literal
        // `"suite_name":"swebench"` which the spaces in pretty JSON break).
        let json = std::fs::read_to_string(&json_path).unwrap();
        assert!(json.contains("\"suite_name\""));
        assert!(json.contains("\"case_id\""));
        assert!(json.contains("diff a"));
        // The Markdown is non-empty.
        let md = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(!md.trim().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
