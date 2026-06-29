//! Report formatting — JSON and Markdown output for EvalReport.
//!
//! The Markdown format is designed for human review and CI integration.
//! It includes:
//! - Suite summary (pass rate, avg score, tokens, duration)
//! - Per-metric summary (pass rate, avg/min/max score)
//! - Individual case results with scores and reasons

use crate::eval_result::EvalReport;

// ─── Markdown Rendering ─────────────────────────────────────────────────

/// Render an EvalReport as a human-readable Markdown document.
///
/// Format:
/// ```markdown
/// # Eval Report: suite_name
///
/// **Timestamp**: 2026-06-18T12:00:00Z
///
/// ## Summary
///
/// | Metric | Value |
/// |--------|-------|
/// | Total cases | 10 |
/// | Passed | 8 (80%) |
/// | Avg score | 0.85 |
/// | Avg duration | 150ms |
/// | Total tokens | 5000 |
///
/// ## Per-Metric Summary
///
/// | Metric | Pass Rate | Avg Score | Min | Max |
/// |--------|-----------|-----------|-----|-----|
/// | exact_match | 100% | 1.00 | 1.0 | 1.0 |
/// | contains_match | 67% | 0.67 | 0.5 | 1.0 |
///
/// ## Case Results
///
/// ### Case: math_add
/// - Input: "What is 2+2?"
/// - Output: "4"
/// - Status: ✓ PASSED
/// - Scores:
///   - exact_match: 1.0/1.0 (✓) — Exact match
/// ```
pub fn render_markdown(report: &EvalReport) -> String {
    let mut md = String::new();

    // Header
    md.push_str(&format!("# Eval Report: {}\n\n", report.suite_name));
    md.push_str(&format!("**Timestamp**: {}\n\n", report.timestamp.to_rfc3339()));

    // Summary table
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n");
    md.push_str("|--------|-------|\n");
    md.push_str(&format!("| Total cases | {} |\n", report.summary.total_cases));
    md.push_str(&format!("| Passed | {} ({:.1}%) |\n",
        report.summary.passed_cases,
        report.summary.pass_rate * 100.0));
    md.push_str(&format!("| Avg score | {:.2} |\n", report.summary.avg_score));
    md.push_str(&format!("| Avg duration | {}ms |\n", report.summary.avg_duration_ms));
    md.push_str(&format!("| Total tokens | {} |\n", report.summary.total_tokens));
    md.push_str(&format!("| Total API calls | {} |\n\n", report.summary.total_api_calls));

    // Overall status
    if report.all_passed() {
        md.push_str("**Status**: ✓ ALL PASSED\n\n");
    } else {
        md.push_str(&format!("**Status**: ✗ {} FAILED\n\n", report.failed_count()));
    }

    // Per-metric summary
    if !report.summary.metric_summaries.is_empty() {
        md.push_str("## Per-Metric Summary\n\n");
        md.push_str("| Metric | Cases | Pass Rate | Avg Score | Min | Max |\n");
        md.push_str("|--------|-------|-----------|-----------|-----|-----|\n");

        for (_, summary) in &report.summary.metric_summaries {
            md.push_str(&format!(
                "| {} | {} | {:.1}% | {:.2} | {:.2} | {:.2} |\n",
                summary.name,
                summary.case_count,
                summary.pass_rate * 100.0,
                summary.avg_score,
                summary.min_score,
                summary.max_score,
            ));
        }
        md.push_str("\n");
    }

    // Case results
    md.push_str("## Case Results\n\n");
    for result in &report.results {
        md.push_str(&format!("### Case: {}\n", result.case_id));
        md.push_str(&format!("- **Input**: {}\n", result.input));

        if result.has_error() {
            md.push_str(&format!("- **Error**: {}\n", result.error.as_ref().unwrap()));
            md.push_str("- **Status**: ✗ ERROR\n\n");
            continue;
        }

        // Truncate long outputs for readability (char-boundary-safe for CJK)
        let output_preview = if result.actual_output.len() > 200 {
            let end = result.actual_output.char_indices()
                .take_while(|(i, _)| *i < 200)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            format!("{}...", &result.actual_output[..end])
        } else {
            result.actual_output.clone()
        };
        md.push_str(&format!("- **Output**: {}\n", output_preview));
        md.push_str(&format!("- **Duration**: {}ms\n", result.duration_ms));
        if result.api_calls > 0 {
            let est_note = if result.estimated_calls > 0 {
                format!(" ({} calls estimated)", result.estimated_calls)
            } else {
                String::new()
            };
            md.push_str(&format!(
                "- API calls: {}{} | tokens: {}+{}\n",
                result.api_calls, est_note,
                result.prompt_tokens, result.completion_tokens,
            ));
        }
        // Timing breakdown — present when the runner stamped a `timing` JSON
        // into metadata (SWE-bench). Decomposes the run into phase wall-clock
        // and the agent's inference/tool/overhead split.
        if let Some(timing) = render_timing_breakdown(result) {
            md.push_str(&timing);
        }

        let status_icon = if result.passed() { "✓ PASSED" } else { "✗ FAILED" };
        md.push_str(&format!("- **Status**: {}\n", status_icon));

        // Individual metric scores
        if !result.scores.is_empty() {
            md.push_str("- **Scores**:\n");
            for ms in &result.scores {
                let icon = if ms.score.passed { "✓" } else { "✗" };
                md.push_str(&format!(
                    "  - {}: {:.2}/{:.02} ({}) — {}\n",
                    ms.metric_name,
                    ms.score.value,
                    ms.score.max_value,
                    icon,
                    ms.score.reason,
                ));
            }
        }
        md.push_str("\n");
    }

    md
}

/// Render the per-case timing breakdown (SWE-bench) from metadata, if present.
///
/// Reads the phase wall-clock keys (`dur_clone_ms`/`dur_agent_ms`/`dur_diff_ms`/
/// `dur_judge_ms`) and the `timing` JSON (inference/tool/overhead split) the
/// SwebenchRunner stamps. Returns `None` when no timing metadata exists (e.g.
/// non-SWE-bench suites), so the report stays unchanged for those.
fn render_timing_breakdown(result: &crate::eval_result::EvalResult) -> Option<String> {
    let has_phases = ["dur_clone_ms", "dur_agent_ms", "dur_diff_ms", "dur_judge_ms"]
        .iter()
        .any(|k| result.metadata.contains_key(*k));
    if !has_phases {
        return None;
    }
    let mut out = String::from("- **Timing**: ");
    let mut parts: Vec<String> = Vec::new();
    for (key, label) in [
        ("dur_clone_ms", "clone"),
        ("dur_agent_ms", "agent"),
        ("dur_diff_ms", "diff"),
        ("dur_judge_ms", "judge"),
    ] {
        if let Some(v) = result.metadata.get(key) {
            parts.push(format!("{}={}ms", label, v));
        }
    }
    out.push_str(&parts.join(" "));

    // Agent-internal decomposition from the trace span tree.
    if let Some(timing_json) = result.metadata.get("timing") {
        #[derive(serde::Deserialize)]
        struct T {
            inference_ms: u64,
            inference_calls: usize,
            tool_ms: u64,
            tool_calls: usize,
            overhead_ms: u64,
        }
        if let Ok(t) = serde_json::from_str::<T>(timing_json) {
            out.push_str(&format!(
                " | agent: infer={}ms×{} tool={}ms×{} overhead={}ms",
                t.inference_ms, t.inference_calls, t.tool_ms, t.tool_calls, t.overhead_ms,
            ));
        }
    }
    out.push('\n');
    Some(out)
}

// ─── Compact Summary ─────────────────────────────────────────────────────

/// Render a compact one-line summary for CI integration.
///
/// Format: `suite_name: X/Y passed (Z%) | avg_score: W | duration: Tms`
pub fn render_compact(report: &EvalReport) -> String {
    format!(
        "{}: {}/{} passed ({:.1}%) | avg_score: {:.2} | duration: {}ms",
        report.suite_name,
        report.summary.passed_cases,
        report.summary.total_cases,
        report.summary.pass_rate * 100.0,
        report.summary.avg_score,
        report.summary.avg_duration_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval_metric::EvalScore;

    #[test]
    fn test_markdown_report() {
        let mut r1 = crate::eval_result::EvalResult::new("math_add", "What is 2+2?", "4");
        r1.add_score("exact_match", EvalScore::perfect("Exact match"));
        r1.add_score("contains_match", EvalScore::perfect("Contains '4'"));
        r1.duration_ms = 150;

        let mut r2 = crate::eval_result::EvalResult::new("math_mult", "What is 3*5?", "15");
        r2.add_score("exact_match", EvalScore::perfect("Exact match"));
        r2.add_score("contains_match", EvalScore::new(0.5, 1.0, "Partial match", true));
        r2.duration_ms = 200;

        let report = EvalReport::new("math_test", vec![r1, r2]);
        let md = render_markdown(&report);

        assert!(md.contains("# Eval Report: math_test"));
        assert!(md.contains("## Summary"));
        assert!(md.contains("## Per-Metric Summary"));
        assert!(md.contains("## Case Results"));
        assert!(md.contains("math_add"));
        assert!(md.contains("math_mult"));
        assert!(md.contains("✓ PASSED"));
    }

    #[test]
    fn test_compact_report() {
        let mut r1 = crate::eval_result::EvalResult::new("c1", "test", "4");
        r1.add_score("exact", EvalScore::perfect("OK"));
        r1.duration_ms = 100;

        let report = EvalReport::new("test", vec![r1]);
        let compact = render_compact(&report);

        assert!(compact.contains("test:"));
        assert!(compact.contains("1/1 passed"));
        assert!(compact.contains("100.0%"));
    }

    #[test]
    fn test_markdown_with_error() {
        let mut r1 = crate::eval_result::EvalResult::new("c1", "test", "");
        r1.error = Some("Provider unavailable".to_string());

        let report = EvalReport::new("error_test", vec![r1]);
        let md = render_markdown(&report);

        assert!(md.contains("✗ ERROR"));
        assert!(md.contains("Provider unavailable"));
    }

    #[test]
    fn test_timing_breakdown_rendered_for_swebench() {
        // A result with SWE-bench timing metadata → timing line is rendered.
        let mut r = crate::eval_result::EvalResult::new("astropy__x-1", "fix bug", "diff --git");
        r.add_score("swebench_resolved", EvalScore::perfect("resolved"));
        r.set_metadata("dur_clone_ms", "1200");
        r.set_metadata("dur_agent_ms", "800000");
        r.set_metadata("dur_diff_ms", "30");
        r.set_metadata("dur_judge_ms", "380000");
        r.set_metadata(
            "timing",
            r#"{"inference_ms":420000,"inference_calls":7,"tool_ms":180000,"tool_calls":12,"overhead_ms":200000,"dur_agent_ms":800000}"#,
        );
        let md = render_markdown(&EvalReport::new("swebench", vec![r]));
        assert!(md.contains("- **Timing**:"), "timing line present");
        assert!(md.contains("clone=1200ms"));
        assert!(md.contains("judge=380000ms"));
        assert!(md.contains("infer=420000ms×7"));
        assert!(md.contains("tool=180000ms×12"));
        assert!(md.contains("overhead=200000ms"));
    }

    #[test]
    fn test_no_timing_breakdown_for_plain_suites() {
        // A plain result (no SWE-bench timing metadata) → no timing line.
        let mut r = crate::eval_result::EvalResult::new("c1", "q", "a");
        r.add_score("exact", EvalScore::perfect("OK"));
        r.duration_ms = 100;
        let md = render_markdown(&EvalReport::new("plain", vec![r]));
        assert!(!md.contains("**Timing**"));
    }

    #[test]
    fn test_markdown_all_passed() {
        let mut r1 = crate::eval_result::EvalResult::new("c1", "test", "4");
        r1.add_score("exact", EvalScore::perfect("OK"));

        let report = EvalReport::new("perfect_test", vec![r1]);
        let md = render_markdown(&report);

        assert!(md.contains("✓ ALL PASSED"));
    }

    #[test]
    fn test_markdown_truncates_long_output() {
        let mut r1 = crate::eval_result::EvalResult::new("c1", "test", "a".repeat(300));
        r1.add_score("exact", EvalScore::zero("No match"));

        let report = EvalReport::new("long_test", vec![r1]);
        let md = render_markdown(&report);

        assert!(md.contains("...")); // truncated output
    }
}
