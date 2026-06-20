//! Built-in eval suites — predefined test collections.
//!
//! Provides ready-to-use eval suites for common agent evaluation scenarios:
//! - **CodingSuite** (`coding_basics`): Basic coding/math reasoning
//! - **ToolUseSuite** (`tool_use`): Tool selection and execution
//! - **GeneralSuite** (`general`): General QA and reasoning

use std::sync::Arc;

use crate::eval_case::{EvalCase, ExpectedOutput};
use crate::eval_suite::{EvalSuite, EvalSuiteBuilder};
use crate::builtin_metrics::{ExactMatchMetric, ContainsMatchMetric, RegexMatchMetric, TrajectoryMetric};

// ─── Coding Suite ────────────────────────────────────────────────────────

/// Create the basic coding evaluation suite.
///
/// Tests basic math and string reasoning that a coding agent should handle:
/// - Simple arithmetic (2+2, 3*5, 10-3)
/// - String operations (reverse, length)
/// - Logic puzzles
///
/// Uses ExactMatch for deterministic outputs and ContainsMatch
/// for explanations where partial credit is useful.
pub fn coding_suite() -> EvalSuite {
    EvalSuiteBuilder::new("coding_basics")
        .description("Basic coding/math reasoning evaluation")
        .domain("coding")
        // Simple math — exact answers expected
        .case(EvalCase::with_id("math_add", "What is 2+2?", ExpectedOutput::exact("4"))
            .difficulty(1).domain("math"))
        .case(EvalCase::with_id("math_multiply", "What is 3*5?", ExpectedOutput::exact("15"))
            .difficulty(1).domain("math"))
        .case(EvalCase::with_id("math_subtract", "What is 10-3?", ExpectedOutput::exact("7"))
            .difficulty(1).domain("math"))
        .case(EvalCase::with_id("math_divide", "What is 100/4?", ExpectedOutput::exact("25"))
            .difficulty(1).domain("math"))
        // Math explanations — contains key concepts
        .case(EvalCase::with_id("math_explain", "Explain why 0! = 1",
            ExpectedOutput::contains(["empty product", "1"]))
            .difficulty(3).domain("math"))
        // String operations — regex for flexible format
        .case(EvalCase::with_id("str_reverse", "Reverse the string 'hello'",
            ExpectedOutput::regex("olleh"))
            .difficulty(2).domain("string"))
        .case(EvalCase::with_id("str_length", "What is the length of 'hello world'?",
            ExpectedOutput::contains(["11"]))
            .difficulty(1).domain("string"))
        // Logic
        .case(EvalCase::with_id("logic_fizzbuzz", "What does FizzBuzz return for 15?",
            ExpectedOutput::contains_case_sensitive(["FizzBuzz"]))
            .difficulty(2).domain("logic"))
        // Metrics
        .metric(Arc::new(ExactMatchMetric))
        .metric(Arc::new(ContainsMatchMetric))
        .metric(Arc::new(RegexMatchMetric))
        .build()
}

// ─── Tool Use Suite ──────────────────────────────────────────────────────

/// Create the tool use evaluation suite.
///
/// Tests whether the agent correctly selects and uses tools:
/// - Calculator for math
/// - Shell for system operations
/// - File tools for reading/writing
///
/// Uses TrajectoryMetric to check tool selection and ExactMatch
/// for verifying the final answer.
pub fn tool_use_suite() -> EvalSuite {
    EvalSuiteBuilder::new("tool_use")
        .description("Tool selection and execution evaluation")
        .domain("coding")
        // Tool selection — trajectory checks
        .case(EvalCase::with_id("calc_tool", "Calculate 2+2 using the calculator",
            ExpectedOutput::trajectory(["calculator"], 3))
            .difficulty(1).domain("tool"))
        .case(EvalCase::with_id("calc_result", "Use calculator to compute 7*8",
            ExpectedOutput::exact("56"))
            .difficulty(1).domain("tool"))
        .case(EvalCase::with_id("multi_tool", "Calculate 2+2 and then multiply the result by 3",
            ExpectedOutput::trajectory(["calculator"], 5))
            .difficulty(2).domain("tool"))
        // Tool result verification
        .case(EvalCase::with_id("calc_large", "What is 123 * 456?",
            ExpectedOutput::contains(["56088"]))
            .difficulty(2).domain("tool"))
        // Metrics
        .metric(Arc::new(ExactMatchMetric))
        .metric(Arc::new(ContainsMatchMetric))
        .metric(Arc::new(TrajectoryMetric))
        .build()
}

// ─── General Suite ───────────────────────────────────────────────────────

/// Create the general QA evaluation suite.
///
/// Tests general reasoning and knowledge:
/// - Factual questions
/// - Explanation quality
/// - Logical reasoning
///
/// Uses ContainsMatch for factual accuracy and RegexMatch
/// for structured outputs.
pub fn general_suite() -> EvalSuite {
    EvalSuiteBuilder::new("general")
        .description("General QA and reasoning evaluation")
        // Factual questions — must contain key facts
        .case(EvalCase::with_id("rust_safe", "What makes Rust unique?",
            ExpectedOutput::contains(["memory", "safe"]))
            .difficulty(2).domain("knowledge"))
        .case(EvalCase::with_id("rust_zero_cost", "What is zero-cost abstraction in Rust?",
            ExpectedOutput::contains(["abstraction", "runtime"]))
            .difficulty(3).domain("knowledge"))
        // Structured output — regex for format
        .case(EvalCase::with_id("date_format", "What is today's date in ISO format?",
            ExpectedOutput::regex("\\d{4}-\\d{2}-\\d{2}"))
            .difficulty(1).domain("knowledge"))
        .case(EvalCase::with_id("email_format", "Format this as an email: John Doe, example.com",
            ExpectedOutput::regex("john\\.?doe@example\\.com"))
            .difficulty(2).domain("formatting"))
        // Logic puzzles
        .case(EvalCase::with_id("binary_convert", "Convert 10 to binary",
            ExpectedOutput::contains(["1010"]))
            .difficulty(2).domain("logic"))
        // Metrics
        .metric(Arc::new(ContainsMatchMetric))
        .metric(Arc::new(RegexMatchMetric))
        .build()
}

// ─── Suite index ─────────────────────────────────────────────────────────

/// Get all built-in suite names.
pub fn builtin_suite_names() -> Vec<&'static str> {
    vec!["coding_basics", "tool_use", "general"]
}

/// Get a built-in suite by name.
///
/// Returns None if the name doesn't match any built-in suite.
pub fn get_builtin_suite(name: &str) -> Option<EvalSuite> {
    match name {
        "coding_basics" => Some(coding_suite()),
        "tool_use" => Some(tool_use_suite()),
        "general" => Some(general_suite()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coding_suite_structure() {
        let suite = coding_suite();
        assert_eq!(suite.name, "coding_basics");
        assert_eq!(suite.case_count(), 8);
        assert_eq!(suite.metric_count(), 3); // exact + contains + regex
        assert_eq!(suite.domain.as_deref(), Some("coding"));
    }

    #[test]
    fn test_tool_use_suite_structure() {
        let suite = tool_use_suite();
        assert_eq!(suite.name, "tool_use");
        assert_eq!(suite.case_count(), 4);
        assert_eq!(suite.metric_count(), 3); // exact + contains + trajectory
        assert_eq!(suite.metric_names(), vec!["exact_match", "contains_match", "trajectory"]);
    }

    #[test]
    fn test_general_suite_structure() {
        let suite = general_suite();
        assert_eq!(suite.name, "general");
        assert_eq!(suite.case_count(), 5);
        assert_eq!(suite.metric_count(), 2); // contains + regex
    }

    #[test]
    fn test_builtin_suite_names() {
        let names = builtin_suite_names();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"coding_basics"));
        assert!(names.contains(&"tool_use"));
        assert!(names.contains(&"general"));
    }

    #[test]
    fn test_get_builtin_suite() {
        assert!(get_builtin_suite("coding_basics").is_some());
        assert!(get_builtin_suite("tool_use").is_some());
        assert!(get_builtin_suite("general").is_some());
        assert!(get_builtin_suite("nonexistent").is_none());
    }

    #[test]
    fn test_coding_suite_case_ids() {
        let suite = coding_suite();
        let ids: Vec<&str> = suite.cases.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"math_add"));
        assert!(ids.contains(&"math_multiply"));
        assert!(ids.contains(&"math_explain"));
    }

    #[test]
    fn test_suite_domain_filtering() {
        let suite = coding_suite();
        let math_cases = suite.filter_cases("domain", "math");
        assert_eq!(math_cases.len(), 5); // 4 exact math + 1 math_explain
    }
}
