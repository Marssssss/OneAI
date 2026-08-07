//! GSM8K adapter — load OpenAI's [GSM8K](https://github.com/openai/grade-school-math)
//! grade-school math JSONL into an [`EvalSuite`].
//!
//! ## Dataset format
//! One JSON object per line:
//! ```jsonc
//! {"question": "Natalia sold 48 clips ... How many in April?",
//!  "answer": "Natalia sold 48/2 = 24 clips in April.\n#### 24"}
//! ```
//! The final numeric answer follows the last `#### ` marker.
//!
//! ## Scoring
//! [`Gsm8kMetric`] extracts the **last** number from the model's output and
//! compares it **numerically** to the expected answer. This is more forgiving
//! than [`ExactMatchMetric`](crate::builtin_metrics::ExactMatchMetric) (which
//! demands a bare, byte-identical number) without false-positiving the way a
//! substring check would (`"10"` is contained in `"100"`):
//! - `"100"` / `"100.0"` / `"100.00"` / `"1,000"` → `100`
//! - `"The answer is 24."` → `24` (last number wins)
//! - `"100" vs expected 100.0` → pass (numerically equal)
//!
//! This keeps the fitness signal clean for the evolve variation loop: a model
//! that reasons correctly but pads its output isn't marked wrong.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use regex::Regex;

use crate::eval_case::{EvalCase, ExpectedOutput};
use crate::eval_metric::{EvalMetric, EvalScore};
use crate::eval_suite::{EvalSuite, EvalSuiteBuilder};

// ─── Gsm8kMetric ──────────────────────────────────────────────────────────

/// Metric for GSM8K: extracts the last number from the model output and
/// compares it numerically (within 1e-6) to the expected `Exact` answer.
///
/// Non-`Exact` expected outputs return "not applicable" (so a multi-metric
/// suite doesn't fail a case this metric just doesn't apply to — see
/// [`EvalScore::not_applicable`]).
pub struct Gsm8kMetric;

#[async_trait::async_trait]
impl EvalMetric for Gsm8kMetric {
    fn name(&self) -> &str {
        "gsm8k_numeric"
    }

    fn description(&self) -> &str {
        "Extracts the last number from the output and compares it numerically to the expected answer (GSM8K)"
    }

    async fn score(&self, _input: &str, actual: &str, expected: &ExpectedOutput) -> EvalScore {
        let ExpectedOutput::Exact { answer } = expected else {
            return EvalScore::not_applicable("Gsm8kMetric only applies to Exact expected output");
        };
        let expected_num = match parse_number(answer) {
            Some(n) => n,
            None => {
                // The expected answer wasn't numeric — the adapter loaded a
                // malformed case. Report not-applicable rather than poisoning
                // the fitness signal with a false failure.
                return EvalScore::not_applicable(format!(
                    "GSM8K expected answer '{answer}' is not numeric"
                ));
            }
        };
        let actual_num = match extract_last_number(actual) {
            Some(n) => n,
            None => {
                return EvalScore::zero(format!(
                    "No number found in output (expected {expected_num})"
                ));
            }
        };
        if (expected_num - actual_num).abs() < 1e-6 {
            EvalScore::perfect(format!("{actual_num} == {expected_num}"))
        } else {
            EvalScore::new(
                0.0,
                1.0,
                format!("Expected {expected_num} but got {actual_num}"),
                false,
            )
        }
    }
}

// ─── number helpers ───────────────────────────────────────────────────────

/// Parse a string as a number, tolerating surrounding whitespace, leading `+`,
/// thousands separators, and a leading currency symbol. Returns `None` if no
/// single number can be recovered.
fn parse_number(s: &str) -> Option<f64> {
    let cleaned: String = s
        .trim()
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    cleaned.parse::<f64>().ok()
}

/// Extract the **last** number token from `s`. A "number token" is an optional
/// sign, digits with optional thousands separators, and an optional decimal
/// fraction — the same shape GSM8K answers take. Returns it parsed as `f64`.
fn extract_last_number(s: &str) -> Option<f64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Optional sign, then a digit, then digits/commas, then optional
        // `.digits`. `find_iter` yields non-overlapping left-to-right matches;
        // we take the last one.
        Regex::new(r"[-+]?\d[\d,]*(?:\.\d+)?").expect("static GSM8K number regex")
    });
    re.find_iter(s).last().and_then(|m| {
        let cleaned: String = m.as_str().chars().filter(|c| *c != ',').collect();
        cleaned.parse::<f64>().ok()
    })
}

/// Extract the final answer from a raw GSM8K `answer` field — the token after
/// the last `#### ` marker. GSM8K puts a single number there, but we take the
/// first whitespace-delimited token defensively.
fn extract_gsm8k_answer(answer: &str) -> Option<String> {
    let idx = answer.rfind("####")?;
    let rest = &answer[idx + 4..];
    let token = rest.split_whitespace().next()?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

// ─── suite loader ─────────────────────────────────────────────────────────

/// Errors loading a GSM8K JSONL file into a suite.
#[derive(Debug, thiserror::Error)]
pub enum Gsm8kLoadError {
    #[error("failed to read GSM8K file '{path}': {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse JSON on line {line} of '{path}': {source}")]
    Json {
        path: String,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("line {line} of '{path}': missing 'question' field")]
    MissingQuestion { path: String, line: usize },
    #[error("line {line} of '{path}': missing 'answer' field or no '#### <num>' marker")]
    MissingAnswer { path: String, line: usize },
}

/// Load a GSM8K JSONL file into an [`EvalSuite`].
///
/// Each non-empty line is one case (`gsm8k-<line>`). The final answer is
/// extracted from the `#### ` marker; the suite is scored by [`Gsm8kMetric`].
///
/// `sample`: if `Some(n)` and `n < case_count`, take a deterministic random
/// sample of `n` cases (fixed-seed Fisher–Yates — reproducible across runs,
/// no `rand` dependency). `None` or `n >= case_count` keeps all cases.
pub fn load_gsm8k_suite(path: &Path, sample: Option<usize>) -> Result<EvalSuite, Gsm8kLoadError> {
    let path_str = path.display().to_string();
    let contents = std::fs::read_to_string(path).map_err(|source| Gsm8kLoadError::Io {
        path: path_str.clone(),
        source,
    })?;

    let mut cases = Vec::new();
    for (i, raw) in (1..).zip(contents.lines()) {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|source| Gsm8kLoadError::Json {
                path: path_str.clone(),
                line: i,
                source,
            })?;
        let question = v.get("question").and_then(|x| x.as_str()).ok_or_else(|| {
            Gsm8kLoadError::MissingQuestion {
                path: path_str.clone(),
                line: i,
            }
        })?;
        let answer = v.get("answer").and_then(|x| x.as_str()).ok_or_else(|| {
            Gsm8kLoadError::MissingAnswer {
                path: path_str.clone(),
                line: i,
            }
        })?;
        let final_num =
            extract_gsm8k_answer(answer).ok_or_else(|| Gsm8kLoadError::MissingAnswer {
                path: path_str.clone(),
                line: i,
            })?;
        cases.push(
            EvalCase::with_id(
                format!("gsm8k-{i}"),
                question,
                ExpectedOutput::exact(final_num),
            )
            .difficulty(2)
            .domain("math"),
        );
    }

    if let Some(n) = sample {
        cases = sample_cases(cases, n);
    }

    Ok(EvalSuiteBuilder::new("gsm8k")
        .description("OpenAI GSM8K grade-school math (numeric answer match)")
        .domain("math")
        .cases(cases)
        .metric(Arc::new(Gsm8kMetric))
        .build())
}

/// Deterministic fixed-seed Fisher–Yates sample: shuffle `cases` with an
/// xorshift64 PRNG seeded from a constant, then truncate to `n`. Reproducible
/// across runs and platforms (no `rand` / no `Math.random`).
fn sample_cases(mut cases: Vec<EvalCase>, n: usize) -> Vec<EvalCase> {
    if n >= cases.len() {
        return cases;
    }
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15; // fixed golden-ratio seed
    for i in (1..cases.len()).rev() {
        // xorshift64*
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        cases.swap(i, j);
    }
    cases.truncate(n);
    cases
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_number() {
        assert_eq!(parse_number("24"), Some(24.0));
        assert_eq!(parse_number("  100.0  "), Some(100.0));
        assert_eq!(parse_number("$1,000"), Some(1000.0));
        assert_eq!(parse_number("-3.5"), Some(-3.5));
        assert_eq!(parse_number("not a number"), None);
    }

    #[test]
    fn test_extract_last_number() {
        assert_eq!(extract_last_number("24"), Some(24.0));
        assert_eq!(extract_last_number("The answer is 24."), Some(24.0));
        assert_eq!(extract_last_number("100.0 vs 100"), Some(100.0));
        assert_eq!(extract_last_number("1,000,000"), Some(1_000_000.0));
        assert_eq!(extract_last_number("no digits here"), None);
        // last number wins when multiple are present
        assert_eq!(extract_last_number("48/2 = 24"), Some(24.0));
    }

    #[test]
    fn test_extract_gsm8k_answer() {
        assert_eq!(
            extract_gsm8k_answer("Natalia sold 48/2 = 24 clips.\n#### 24"),
            Some("24".to_string())
        );
        assert_eq!(extract_gsm8k_answer("#### 100"), Some("100".to_string()));
        assert_eq!(extract_gsm8k_answer("no marker here"), None);
        assert_eq!(extract_gsm8k_answer("####  "), None);
    }

    #[tokio::test]
    async fn test_gsm8k_metric_passes_numeric_variants() {
        let m = Gsm8kMetric;
        let expected = ExpectedOutput::exact("100");

        // Bare, exact.
        assert!(m.score("", "100", &expected).await.passed);
        // Trailing zeros / decimal — numerically equal.
        assert!(m.score("", "100.0", &expected).await.passed);
        assert!(m.score("", "100.00", &expected).await.passed);
        // Embedded in prose — last number wins.
        assert!(m.score("", "The answer is 100.", &expected).await.passed);
        // Thousands separators.
        assert!(
            m.score("", "1,000", &ExpectedOutput::exact("1000"))
                .await
                .passed
        );
    }

    #[tokio::test]
    async fn test_gsm8k_metric_fails_wrong_number() {
        let m = Gsm8kMetric;
        let expected = ExpectedOutput::exact("24");
        assert!(!m.score("", "48", &expected).await.passed);
        assert!(!m.score("", "no digits here", &expected).await.passed);
    }

    #[tokio::test]
    async fn test_gsm8k_metric_not_applicable_for_non_exact() {
        let m = Gsm8kMetric;
        let contains = ExpectedOutput::contains(["foo"]);
        let score = m.score("", "foo", &contains).await;
        assert!(!score.applicable);
    }

    fn write_tmp_jsonl(content: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oneai-gsm8k-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gsm8k.jsonl");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_load_gsm8k_suite_full() {
        let jsonl = "\
{\"question\": \"What is 2+2?\", \"answer\": \"2+2 = 4.\\n#### 4\"}
{\"question\": \"3 times 5?\", \"answer\": \"3*5 = 15.\\n#### 15\"}
{\"question\": \"10 minus 3?\", \"answer\": \"#### 7\"}
";
        let path = write_tmp_jsonl(jsonl);
        let suite = load_gsm8k_suite(&path, None).unwrap();
        assert_eq!(suite.name, "gsm8k");
        assert_eq!(suite.case_count(), 3);
        assert_eq!(suite.metric_names(), vec!["gsm8k_numeric"]);
        // ids carry the source line number.
        let ids: Vec<&str> = suite.cases.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["gsm8k-1", "gsm8k-2", "gsm8k-3"]);
        // expected answer extracted from the #### marker.
        assert!(matches!(
            &suite.cases[0].expected,
            ExpectedOutput::Exact { answer } if answer == "4"
        ));
    }

    #[test]
    fn test_load_gsm8k_suite_sample_is_deterministic_and_subset() {
        let mut jsonl = String::new();
        for i in 1..=50 {
            jsonl.push_str(&format!(
                "{{\"question\": \"q{i}?\", \"answer\": \"#### {i}\"}}\n"
            ));
        }
        let path = write_tmp_jsonl(&jsonl);
        // Two independent loads with the same sample size must yield identical
        // (deterministic) selections.
        let s1 = load_gsm8k_suite(&path, Some(10)).unwrap();
        let s2 = load_gsm8k_suite(&path, Some(10)).unwrap();
        assert_eq!(s1.case_count(), 10);
        let ids1: Vec<&str> = s1.cases.iter().map(|c| c.id.as_str()).collect();
        let ids2: Vec<&str> = s2.cases.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids1, ids2);
        // sample larger than population keeps everything.
        let s3 = load_gsm8k_suite(&path, Some(1_000)).unwrap();
        assert_eq!(s3.case_count(), 50);
    }

    #[test]
    fn test_load_gsm8k_suite_skips_blank_lines() {
        let jsonl = "\n{\"question\": \"q\", \"answer\": \"#### 1\"}\n\n";
        let path = write_tmp_jsonl(jsonl);
        let suite = load_gsm8k_suite(&path, None).unwrap();
        assert_eq!(suite.case_count(), 1);
    }

    #[test]
    fn test_load_gsm8k_suite_errors_on_missing_marker() {
        let jsonl = "{\"question\": \"q\", \"answer\": \"no marker here\"}\n";
        let path = write_tmp_jsonl(jsonl);
        let err = load_gsm8k_suite(&path, None).unwrap_err();
        assert!(matches!(err, Gsm8kLoadError::MissingAnswer { line: 1, .. }));
    }

    #[test]
    fn test_load_gsm8k_suite_errors_on_bad_json() {
        let jsonl = "{not json\n";
        let path = write_tmp_jsonl(jsonl);
        assert!(matches!(
            load_gsm8k_suite(&path, None).unwrap_err(),
            Gsm8kLoadError::Json { line: 1, .. }
        ));
    }
}
