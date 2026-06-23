//! SWE-bench external judge.
//!
//! `SwebenchJudge` is an `EvalJudge` that, given the agent's patch (the
//! `actual` argument), runs the external SWE-bench evaluation harness as a
//! Python subprocess and returns a pass/fail `EvalScore` reflecting the
//! harness's `resolved` verdict.
//!
//! The harness is the source of truth for the **能力 axis** (resolved). We do
//! not reimplement its logic in Rust — it evolves with each dataset release and
//! would be a maintenance liability. We only shell out and parse its output.
//!
//! ## Testability
//!
//! `parse_instance_results` is a pure function over the harness's per-instance
//! result JSONL — it is unit-tested with fixture strings and does not touch
//! the filesystem or spawn processes. The subprocess path only runs live.

use async_trait::async_trait;
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

use crate::eval_metric::{EvalJudge, EvalScore};

/// The verdict for a single instance from the SWE-bench harness.
#[derive(Debug, Clone, Serialize)]
pub struct SwebenchVerdict {
    /// Whether the instance was resolved (FAIL_TO_PASS now pass, no PASS_TO_PASS regression).
    pub resolved: bool,
    /// The raw `tests_status` object (per-test pass/fail), kept for reporting.
    pub tests_status: serde_json::Value,
}

/// Parse the SWE-bench harness per-instance results for one instance.
///
/// Accepts either the `instance_results.jsonl` format (one JSON object per
/// line: `{instance_id, resolved, tests_status}`) or a top-level JSON array of
/// those objects. Returns the verdict for `instance_id`, or an error if the
/// instance isn't present (e.g. the harness skipped it).
///
/// Pure function — no I/O, no subprocess. Tested with fixture strings.
pub fn parse_instance_results(results: &str, instance_id: &str) -> Result<SwebenchVerdict, String> {
    let trimmed = results.trim();
    if trimmed.is_empty() {
        return Err(format!("harness result is empty for instance {}", instance_id));
    }

    // Try as a JSON array first, then fall back to JSONL line-by-line.
    let entries: Vec<serde_json::Value> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid harness result array: {}", e))?
    } else {
        trimmed
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .collect()
    };

    for entry in &entries {
        let id = entry.get("instance_id").and_then(|v| v.as_str()).unwrap_or("");
        if id == instance_id {
            let resolved = entry.get("resolved").and_then(|v| v.as_bool()).unwrap_or(false);
            let tests_status = entry.get("tests_status").cloned().unwrap_or(serde_json::Value::Null);
            return Ok(SwebenchVerdict { resolved, tests_status });
        }
    }

    Err(format!("instance '{}' not found in harness results", instance_id))
}

/// Parse a per-instance `report.json` as written by the SWE-bench harness under
/// `--modal true` (Modal/cloud mode).
///
/// That file is an object keyed by instance id, each value carrying the verdict:
/// `{ "<instance_id>": { "resolved": true, "tests_status": {...}, ... } }`.
/// (Non-modal / older harnesses write the `instance_results.jsonl` array form
/// handled by `parse_instance_results` instead.)
pub fn parse_instance_report(report_json: &str, instance_id: &str) -> Result<SwebenchVerdict, String> {
    let value: serde_json::Value = serde_json::from_str(report_json)
        .map_err(|e| format!("invalid harness report.json: {}", e))?;

    let entry = value.get(instance_id).ok_or_else(|| {
        format!("instance '{}' not found in harness report.json", instance_id)
    })?;

    let resolved = entry.get("resolved").and_then(|v| v.as_bool()).unwrap_or(false);
    let tests_status = entry.get("tests_status").cloned().unwrap_or(serde_json::Value::Null);
    Ok(SwebenchVerdict { resolved, tests_status })
}

/// An `EvalJudge` backed by the external SWE-bench harness.
///
/// Holds the per-instance metadata needed to invoke the harness. `judge()`
/// receives the agent's patch as `actual` and:
/// 1. Short-circuits to `unresolved` on an empty patch (the agent made no
///    edits) — no subprocess is spawned.
/// 2. Otherwise writes a one-line prediction JSONL, runs
///    `python -m swebench.harness.run_evaluation`, and parses the per-instance
///    result.
///
/// Subprocess or parse failures never panic — they return a zero score with a
/// descriptive reason so the eval run continues.
pub struct SwebenchJudge {
    /// The instance id this judge scores.
    pub instance_id: String,
    /// Path to the Python interpreter with `swebench` installed
    /// (default `~/.venvs/swebench/bin/python`).
    pub python_path: String,
    /// Run the harness via Modal (`--modal true`) — avoids local docker.
    pub use_modal: bool,
    /// Run id — the harness writes results keyed by this id.
    pub run_id: String,
    /// Dataset name (e.g. `princeton-nlp/SWE-bench_Lite`).
    pub dataset_name: String,
    /// The `model_name_or_path` written into the prediction (also the harness's
    /// per-model results subdir under `--modal true`). The per-instance report
    /// lands at `logs/run_evaluation/<run_id>/<model_name>/<instance_id>/report.json`.
    pub model_name: String,
    /// Directory to write the temp prediction file into.
    pub workspace_dir: PathBuf,
}

impl SwebenchJudge {
    /// Build a judge for one instance from shared run config.
    pub fn new(
        instance_id: impl Into<String>,
        python_path: impl Into<String>,
        use_modal: bool,
        run_id: impl Into<String>,
        dataset_name: impl Into<String>,
        workspace_dir: PathBuf,
    ) -> Self {
        Self::with_model_name(
            instance_id, python_path, use_modal, run_id, dataset_name,
            "oneai".to_string(), workspace_dir,
        )
    }

    /// Like `new` but with an explicit `model_name` (controls the harness's
    /// per-model results subdir and the `model_name_or_path` in the prediction).
    pub fn with_model_name(
        instance_id: impl Into<String>,
        python_path: impl Into<String>,
        use_modal: bool,
        run_id: impl Into<String>,
        dataset_name: impl Into<String>,
        model_name: String,
        workspace_dir: PathBuf,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            python_path: python_path.into(),
            use_modal,
            run_id: run_id.into(),
            dataset_name: dataset_name.into(),
            model_name,
            workspace_dir,
        }
    }

    /// Per-instance `report.json` written by the harness under `--modal true`:
    /// `logs/run_evaluation/<run_id>/<model_name>/<instance_id>/report.json`.
    /// (Relative to the harness's CWD — the CLI runs from the repo root.)
    fn modal_report_path(&self) -> PathBuf {
        PathBuf::from("logs/run_evaluation")
            .join(&self.run_id)
            .join(&self.model_name)
            .join(&self.instance_id)
            .join("report.json")
    }

    /// Per-run `instance_results.jsonl` written by the harness in non-modal mode:
    /// `evaluation_results/<run_id>/instance_results.jsonl`.
    fn instance_results_path(&self) -> PathBuf {
        PathBuf::from(format!("evaluation_results/{}", self.run_id))
            .join("instance_results.jsonl")
    }

    /// Write a one-line prediction JSONL and return its path.
    fn write_prediction(&self, patch: &str) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.workspace_dir)
            .map_err(|e| format!("cannot create workspace dir: {}", e))?;
        let record = serde_json::json!({
            "instance_id": self.instance_id,
            "model_name_or_path": "oneai",
            "model_patch": patch,
        });
        let line = serde_json::to_string(&record).map_err(|e| e.to_string())?;
        let path = self.workspace_dir.join(format!("{}.prediction.jsonl", self.instance_id));
        std::fs::write(&path, format!("{}\n", line))
            .map_err(|e| format!("cannot write prediction file: {}", e))?;
        Ok(path)
    }
}

#[async_trait]
impl EvalJudge for SwebenchJudge {
    async fn judge(&self, _input: &str, actual: &str) -> EvalScore {
        // Empty patch → the agent made no changes. Don't waste a harness run.
        if actual.trim().is_empty() {
            return EvalScore::zero("empty patch — agent made no changes");
        }

        let prediction_path = match self.write_prediction(actual) {
            Ok(p) => p,
            Err(e) => return EvalScore::zero(format!("failed to write prediction: {}", e)),
        };

        // Invoke the external harness.
        let mut args: Vec<String> = vec![
            "-m".into(), "swebench.harness.run_evaluation".into(),
            "--dataset_name".into(), self.dataset_name.clone(),
            "--predictions_path".into(), prediction_path.to_string_lossy().into(),
            "--max_workers".into(), "1".into(),
            "--run_id".into(), self.run_id.clone(),
        ];
        if self.use_modal {
            args.push("--modal".into());
            args.push("true".into());
        }
        let output = match Command::new(&self.python_path).args(&args).output() {
            Ok(o) => o,
            Err(e) => {
                return EvalScore::zero(format!(
                    "failed to run swebench harness (python={}): {} — is the venv installed?",
                    self.python_path, e
                ));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return EvalScore::zero(format!(
                "swebench harness exited {} : {}",
                output.status,
                stderr.trim()
            ));
        }

        // Read the per-instance verdict the harness produced. Under `--modal
        // true` it writes a keyed report.json per instance; in non-modal mode
        // it writes a per-run instance_results.jsonl. Try the modal path
        // first (it's the default), then fall back.
        let verdict = if let Ok(report_text) = std::fs::read_to_string(self.modal_report_path()) {
            parse_instance_report(&report_text, &self.instance_id)
        } else {
            let results_path = self.instance_results_path();
            std::fs::read_to_string(&results_path)
                .map_err(|e| {
                    format!(
                        "harness produced no results (tried {} and {}): {}",
                        self.modal_report_path().display(),
                        results_path.display(),
                        e
                    )
                })
                .and_then(|text| parse_instance_results(&text, &self.instance_id))
        };

        match verdict {
            Ok(verdict) => {
                let reason = if verdict.resolved {
                    format!("resolved: {}", summary_tests_status(&verdict.tests_status))
                } else {
                    format!("unresolved: {}", summary_tests_status(&verdict.tests_status))
                };
                EvalScore::from_bool(verdict.resolved, reason)
            }
            Err(e) => EvalScore::zero(format!("could not parse harness result: {}", e)),
        }
    }
}

/// Render a one-line summary of a `tests_status` object for the score reason.
fn summary_tests_status(status: &serde_json::Value) -> String {
    // tests_status is typically {"PASS_TO_PASS": {"success":[...], "failure":[...]},
    //                             "FAIL_TO_PASS": {"success":[...], "failure":[...]}}
    let mut parts = Vec::new();
    for key in ["FAIL_TO_PASS", "PASS_TO_PASS"] {
        if let Some(group) = status.get(key) {
            let success = group.get("success").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            let failure = group.get("failure").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            parts.push(format!("{}={}/{} pass", key, success, success + failure));
        }
    }
    if parts.is_empty() {
        "no test breakdown".to_string()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_instance_results_resolved() {
        let jsonl = "{\"instance_id\":\"astropy__astropy-12907\",\"resolved\":true,\"tests_status\":{\"FAIL_TO_PASS\":{\"success\":[\"t1\"],\"failure\":[]},\"PASS_TO_PASS\":{\"success\":[\"t2\"],\"failure\":[]}}}\n";
        let v = parse_instance_results(jsonl, "astropy__astropy-12907").unwrap();
        assert!(v.resolved);
        assert_eq!(
            summary_tests_status(&v.tests_status),
            "FAIL_TO_PASS=1/1 pass, PASS_TO_PASS=1/1 pass"
        );
    }

    #[test]
    fn test_parse_instance_results_unresolved() {
        let jsonl = "{\"instance_id\":\"x__y-1\",\"resolved\":false,\"tests_status\":{\"FAIL_TO_PASS\":{\"success\":[],\"failure\":[\"t1\"]}}}\n";
        let v = parse_instance_results(jsonl, "x__y-1").unwrap();
        assert!(!v.resolved);
    }

    #[test]
    fn test_parse_instance_results_array_form() {
        let arr = "[{\"instance_id\":\"a\",\"resolved\":true,\"tests_status\":{}},{\"instance_id\":\"b\",\"resolved\":false,\"tests_status\":{}}]";
        let v = parse_instance_results(arr, "b").unwrap();
        assert!(!v.resolved);
    }

    #[test]
    fn test_parse_instance_results_missing_instance() {
        let jsonl = "{\"instance_id\":\"a\",\"resolved\":true,\"tests_status\":{}}\n";
        let err = parse_instance_results(jsonl, "b").unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn test_parse_instance_results_empty() {
        let err = parse_instance_results("", "a").unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn test_parse_instance_report_resolved() {
        // The --modal form: object keyed by instance_id.
        let report = r#"{
            "astropy__astropy-12907": {
                "patch_is_None": false,
                "resolved": true,
                "tests_status": {
                    "FAIL_TO_PASS": {"success": ["t1", "t2"], "failure": []},
                    "PASS_TO_PASS": {"success": ["t3"], "failure": []}
                }
            }
        }"#;
        let v = parse_instance_report(report, "astropy__astropy-12907").unwrap();
        assert!(v.resolved);
        assert_eq!(
            summary_tests_status(&v.tests_status),
            "FAIL_TO_PASS=2/2 pass, PASS_TO_PASS=1/1 pass"
        );
    }

    #[test]
    fn test_parse_instance_report_unresolved() {
        let report = r#"{"x__y-1": {"resolved": false, "tests_status": {"FAIL_TO_PASS": {"success": [], "failure": ["t1"]}}}}"#;
        let v = parse_instance_report(report, "x__y-1").unwrap();
        assert!(!v.resolved);
    }

    #[test]
    fn test_parse_instance_report_missing_instance() {
        let report = r#"{"a": {"resolved": true, "tests_status": {}}}"#;
        let err = parse_instance_report(report, "b").unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn test_judge_empty_patch_short_circuits() {
        // No python, no venv — must NOT spawn anything, just return unresolved.
        let judge = SwebenchJudge::new(
            "x__y-1", "/nonexistent/python", true, "run-x",
            "princeton-nlp/SWE-bench_Lite", std::env::temp_dir(),
        );
        let score = judge.judge("issue text", "   ").await;
        assert!(!score.passed);
        assert!(score.reason.contains("empty patch"));
    }
}
