//! EvalCase and ExpectedOutput — the fundamental unit of agent evaluation.
//!
//! An EvalCase defines a single test scenario:
//! - `input`: what the user asks the agent
//! - `expected`: what the correct/acceptable output looks like
//! - `metadata`: tags, difficulty, domain for filtering and grouping
//!
//! ExpectedOutput is the key abstraction — it supports multiple evaluation
//! strategies (exact match, contains, regex, LLM-as-judge, trajectory, custom).
//! This allows the same EvalCase infrastructure to serve both deterministic
//! unit tests and fuzzy LLM-based quality assessments.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};


// ─── EvalCase ────────────────────────────────────────────────────────────

/// A single evaluation test case — input + expected output + metadata.
///
/// The core unit of evaluation. Each case defines:
/// - What to ask the agent (`input`)
/// - What counts as a correct/acceptable answer (`expected`)
/// - Optional metadata for filtering, grouping, and difficulty rating
///
/// Cases are collected into `EvalSuite` and run by `EvalRunner`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCase {
    /// Unique case identifier.
    pub id: String,

    /// The user input / task description sent to the agent.
    pub input: String,

    /// The expected output — defines how to judge the agent's response.
    ///
    /// Note: `ExpectedOutput::Custom` uses a trait object and is not
    /// serializable. For serialization, use the other variants.
    #[serde(with = "expected_output_serde")]
    pub expected: ExpectedOutput,

    /// Optional metadata (domain, difficulty, tags, etc.).
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl EvalCase {
    /// Create a new eval case with exact match expected output.
    pub fn new(input: impl Into<String>, expected: ExpectedOutput) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            input: input.into(),
            expected,
            metadata: HashMap::new(),
        }
    }

    /// Create a new eval case with a specific ID.
    pub fn with_id(id: impl Into<String>, input: impl Into<String>, expected: ExpectedOutput) -> Self {
        Self {
            id: id.into(),
            input: input.into(),
            expected,
            metadata: HashMap::new(),
        }
    }

    /// Add metadata to this case.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Set the difficulty level (1-5 scale).
    pub fn difficulty(mut self, level: u8) -> Self {
        self.metadata.insert("difficulty".to_string(), level.to_string());
        self
    }

    /// Set the domain tag.
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.metadata.insert("domain".to_string(), domain.into());
        self
    }
}

// ─── ExpectedOutput ──────────────────────────────────────────────────────

/// How to judge whether the agent's output is correct.
///
/// Supports multiple evaluation strategies:
/// - **Exact**: exact string match (for deterministic outputs like math)
/// - **Contains**: all specified substrings must appear (for factual answers)
/// - **Regex**: regex pattern match (for structured but flexible outputs)
/// - **LlmJudge**: an LLM scores the output on a rubric (for subjective quality)
/// - **Trajectory**: checks that the agent used the right tools (for tool-use eval)
/// - **Custom**: a user-defined `EvalJudge` trait object (for domain-specific logic)
///
/// The `#[non_exhaustive]` annotation ensures new evaluation strategies can be
/// added in future versions without breaking downstream code.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ExpectedOutput {
    /// Exact string match — the output must equal this string exactly.
    Exact {
        /// The expected answer string.
        answer: String,
    },

    /// Contains match — all specified substrings must appear in the output.
    Contains {
        /// Substrings that must all be present in the output.
        substrings: Vec<String>,
        /// Whether matching is case-sensitive.
        case_sensitive: bool,
    },

    /// Regex match — the output must match this regex pattern.
    Regex {
        /// The regex pattern to match against.
        pattern: String,
    },

    /// LLM-as-judge — an LLM scores the output against a rubric.
    ///
    /// Requires an LLM provider to be configured. The judge model evaluates
    /// the output on a 0-10 scale based on the rubric description.
    /// A score >= `min_score` is considered passing.
    LlmJudge {
        /// The evaluation rubric — describes what constitutes a good answer.
        rubric: String,
        /// Minimum score to pass (0.0 to 10.0).
        min_score: f64,
    },

    /// Trajectory evaluation — checks that the agent used specific tools.
    ///
    /// Useful for evaluating whether the agent follows the right execution
    /// path (e.g., used `calculator` for a math problem, used `search` for
    /// a research question).
    Trajectory {
        /// Tool names that should be called during execution.
        expected_tools: Vec<String>,
        /// Maximum number of loop iterations allowed (over = fail).
        max_iterations: usize,
    },

    /// Custom evaluation — uses a user-defined `EvalJudge` implementation.
    ///
    /// This variant is NOT serializable. It's for programmatic use only.
    /// For serializable configs, use one of the other variants.
    Custom {
        /// The custom judge implementation.
        #[cfg(skip_serde)] // This field is handled manually in serde
        judge: Arc<dyn EvalJudge>,
    },
}

// ─── ExpectedOutput convenience constructors ─────────────────────────────

impl ExpectedOutput {
    /// Create an Exact expected output from any string-like value.
    pub fn exact(answer: impl Into<String>) -> Self {
        Self::Exact { answer: answer.into() }
    }

    /// Create a Contains expected output with case-insensitive matching.
    pub fn contains(substrings: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Contains {
            substrings: substrings.into_iter().map(Into::into).collect(),
            case_sensitive: false,
        }
    }

    /// Create a Contains expected output with case-sensitive matching.
    pub fn contains_case_sensitive(substrings: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Contains {
            substrings: substrings.into_iter().map(Into::into).collect(),
            case_sensitive: true,
        }
    }

    /// Create a Regex expected output from any string-like value.
    pub fn regex(pattern: impl Into<String>) -> Self {
        Self::Regex { pattern: pattern.into() }
    }

    /// Create a LlmJudge expected output.
    pub fn llm_judge(rubric: impl Into<String>, min_score: f64) -> Self {
        Self::LlmJudge { rubric: rubric.into(), min_score }
    }

    /// Create a Trajectory expected output.
    pub fn trajectory(expected_tools: impl IntoIterator<Item = impl Into<String>>, max_iterations: usize) -> Self {
        Self::Trajectory {
            expected_tools: expected_tools.into_iter().map(Into::into).collect(),
            max_iterations,
        }
    }
}

// ─── Serde helpers for ExpectedOutput ────────────────────────────────────

/// Custom serde module for ExpectedOutput.
///
/// Handles serialization/deserialization of ExpectedOutput variants,
/// skipping the `Custom` variant (which contains a trait object that
/// cannot be serialized). Custom variants are serialized as a placeholder
/// and deserialized as Exact with an empty answer.
mod expected_output_serde {
    use super::ExpectedOutput;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &ExpectedOutput, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Custom variant can't be serialized — skip it
        match value {
            ExpectedOutput::Exact { answer } => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("ExpectedOutput", 2)?;
                s.serialize_field("type", "exact")?;
                s.serialize_field("answer", answer)?;
                s.end()
            }
            ExpectedOutput::Contains { substrings, case_sensitive } => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("ExpectedOutput", 3)?;
                s.serialize_field("type", "contains")?;
                s.serialize_field("substrings", substrings)?;
                s.serialize_field("case_sensitive", case_sensitive)?;
                s.end()
            }
            ExpectedOutput::Regex { pattern } => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("ExpectedOutput", 2)?;
                s.serialize_field("type", "regex")?;
                s.serialize_field("pattern", pattern)?;
                s.end()
            }
            ExpectedOutput::LlmJudge { rubric, min_score } => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("ExpectedOutput", 3)?;
                s.serialize_field("type", "llm_judge")?;
                s.serialize_field("rubric", rubric)?;
                s.serialize_field("min_score", min_score)?;
                s.end()
            }
            ExpectedOutput::Trajectory { expected_tools, max_iterations } => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("ExpectedOutput", 3)?;
                s.serialize_field("type", "trajectory")?;
                s.serialize_field("expected_tools", expected_tools)?;
                s.serialize_field("max_iterations", max_iterations)?;
                s.end()
            }
            ExpectedOutput::Custom { .. } => {
                // Custom judges can't be serialized — emit a placeholder
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("ExpectedOutput", 2)?;
                s.serialize_field("type", "custom_placeholder")?;
                s.serialize_field("note", "Custom judges cannot be serialized")?;
                s.end()
            }
            // Handle unknown variants from #[non_exhaustive]
            _ => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("ExpectedOutput", 2)?;
                s.serialize_field("type", "unknown")?;
                s.serialize_field("note", "Unknown variant")?;
                s.end()
            }
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ExpectedOutput, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde_json::Value;
        let value = Value::deserialize(deserializer)?;
        let type_str = value.get("type").and_then(|v| v.as_str()).unwrap_or("exact");

        match type_str {
            "exact" => {
                let answer = value.get("answer").and_then(|v| v.as_str()).unwrap_or("").to_string();
                Ok(ExpectedOutput::Exact { answer })
            }
            "contains" => {
                let substrings = value.get("substrings")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                let case_sensitive = value.get("case_sensitive").and_then(|v| v.as_bool()).unwrap_or(false);
                Ok(ExpectedOutput::Contains { substrings, case_sensitive })
            }
            "regex" => {
                let pattern = value.get("pattern").and_then(|v| v.as_str()).unwrap_or("").to_string();
                Ok(ExpectedOutput::Regex { pattern })
            }
            "llm_judge" => {
                let rubric = value.get("rubric").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let min_score = value.get("min_score").and_then(|v| v.as_f64()).unwrap_or(7.0);
                Ok(ExpectedOutput::LlmJudge { rubric, min_score })
            }
            "trajectory" => {
                let expected_tools = value.get("expected_tools")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                let max_iterations = value.get("max_iterations").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                Ok(ExpectedOutput::Trajectory { expected_tools, max_iterations })
            }
            "custom_placeholder" | "custom" => {
                // Deserialized custom placeholder becomes Exact with empty answer
                Ok(ExpectedOutput::Exact { answer: String::new() })
            }
            _ => Err(serde::de::Error::custom(format!("Unknown ExpectedOutput type: {}", type_str))),
        }
    }
}

impl serde::Serialize for ExpectedOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        expected_output_serde::serialize(self, serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ExpectedOutput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        expected_output_serde::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eval_case_exact() {
        let case = EvalCase::new("What is 2+2?", ExpectedOutput::exact("4"));
        assert_eq!(case.input, "What is 2+2?");
        assert!(matches!(case.expected, ExpectedOutput::Exact { answer: _ }));
    }

    #[test]
    fn test_eval_case_with_metadata() {
        let case = EvalCase::with_id("case_001", "test", ExpectedOutput::exact("ok"))
            .difficulty(3)
            .domain("math");
        assert_eq!(case.id, "case_001");
        assert_eq!(case.metadata.get("difficulty").unwrap(), "3");
        assert_eq!(case.metadata.get("domain").unwrap(), "math");
    }

    #[test]
    fn test_eval_case_contains() {
        let case = EvalCase::new(
            "Explain Rust",
            ExpectedOutput::contains(["memory", "safe"]),
        );
        assert!(matches!(case.expected, ExpectedOutput::Contains { .. }));
    }

    #[test]
    fn test_eval_case_regex() {
        let case = EvalCase::new(
            "What is the date?",
            ExpectedOutput::regex("\\d{4}-\\d{2}-\\d{2}"),
        );
        assert!(matches!(case.expected, ExpectedOutput::Regex { .. }));
    }

    #[test]
    fn test_eval_case_trajectory() {
        let case = EvalCase::new(
            "Calculate 2+2",
            ExpectedOutput::trajectory(["calculator"], 5),
        );
        assert!(matches!(case.expected, ExpectedOutput::Trajectory { .. }));
    }

    #[test]
    fn test_expected_output_convenience_constructors() {
        let exact = ExpectedOutput::exact("42");
        assert!(matches!(exact, ExpectedOutput::Exact { answer: _ }));

        let contains = ExpectedOutput::contains(["a", "b"]);
        if let ExpectedOutput::Contains { substrings, case_sensitive } = contains {
            assert_eq!(substrings, vec!["a", "b"]);
            assert!(!case_sensitive);
        } else {
            panic!("Expected Contains");
        }

        let contains_cs = ExpectedOutput::contains_case_sensitive(["A", "B"]);
        if let ExpectedOutput::Contains { substrings, case_sensitive } = contains_cs {
            assert!(case_sensitive);
        } else {
            panic!("Expected Contains");
        }

        let regex = ExpectedOutput::regex("pattern");
        assert!(matches!(regex, ExpectedOutput::Regex { pattern: _ }));

        let llm = ExpectedOutput::llm_judge("Good answer", 7.0);
        assert!(matches!(llm, ExpectedOutput::LlmJudge { .. }));

        let traj = ExpectedOutput::trajectory(["tool_a", "tool_b"], 10);
        assert!(matches!(traj, ExpectedOutput::Trajectory { .. }));
    }

    #[test]
    fn test_expected_output_serde_roundtrip() {
        let expected = ExpectedOutput::exact("42");
        let json = serde_json::to_string(&expected).unwrap();
        let deserialized: ExpectedOutput = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, ExpectedOutput::Exact { answer: _ }));
    }

    #[test]
    fn test_expected_output_contains_serde() {
        let expected = ExpectedOutput::contains_case_sensitive(["hello", "world"]);
        let json = serde_json::to_string(&expected).unwrap();
        let deserialized: ExpectedOutput = serde_json::from_str(&json).unwrap();
        if let ExpectedOutput::Contains { substrings, case_sensitive } = deserialized {
            assert_eq!(substrings, vec!["hello", "world"]);
            assert!(case_sensitive);
        } else {
            panic!("Expected Contains variant");
        }
    }

    #[test]
    fn test_expected_output_trajectory_serde() {
        let expected = ExpectedOutput::trajectory(["calculator", "read_file"], 10);
        let json = serde_json::to_string(&expected).unwrap();
        let deserialized: ExpectedOutput = serde_json::from_str(&json).unwrap();
        if let ExpectedOutput::Trajectory { expected_tools, max_iterations } = deserialized {
            assert_eq!(expected_tools, vec!["calculator", "read_file"]);
            assert_eq!(max_iterations, 10);
        } else {
            panic!("Expected Trajectory variant");
        }
    }

    #[test]
    fn test_eval_case_json_roundtrip() {
        let case = EvalCase::with_id("test_1", "What is 2+2?", ExpectedOutput::exact("4"))
            .difficulty(1);
        let json = serde_json::to_string(&case).unwrap();
        let deserialized: EvalCase = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "test_1");
        assert_eq!(deserialized.input, "What is 2+2?");
    }
}
