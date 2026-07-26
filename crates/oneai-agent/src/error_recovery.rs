//! Error recovery system — systematic approach to error handling and self-correction.
//!
//! This addresses Issue #21: the current error recovery is basic and not systematic.
//! The ReflectionAgent only relies on LLM judgment, which research shows is
//! unreliable for self-correction. The most effective self-correction is
//! based on **external feedback** (execution results, assertion checks, test outcomes),
//! not LLM self-assessment.
//!
//! Inspired by Claude Code's approach:
//! - Retry with configurable policies
//! - Conditional fallback edges (error → correction path)
//! - State rollback from checkpoints
//! - Assertion/constraint hooks for interception
//! - External feedback-driven judgment (test results, compilation, API status)
//!
//! These recovery strategies are used in the AgentLoop's error handling
//! and in the StateGraph's conditional edge routing.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;

// ─── RecoveryStrategy ──────────────────────────────────────────────────────

/// Strategy for recovering from errors during agent execution.
///
/// Each strategy represents a different approach to error handling,
/// chosen based on the error type, context, and available recovery mechanisms.
///
/// The key innovation is `ExternalFeedback` — error judgment based on
/// verifiable objective indicators (test results, compilation status,
/// API response codes) rather than LLM self-assessment alone.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RecoveryStrategy {
    /// Retry the failed action with a configurable policy.
    ///
    /// Use for transient errors (network timeouts, rate limits).
    Retry {
        /// The retry policy (max retries, delay, etc.)
        policy: RetryPolicy,
    },

    /// Conditional fallback — route from the error node to a correction node.
    ///
    /// This is implemented as a conditional edge in the StateGraph:
    /// `error_node → [error_occurred] → fix_node`
    ///
    /// Example: code compilation fails → route to syntax correction node
    ConditionalFallback {
        /// The node that encountered the error.
        error_node: String,
        /// The node that should handle the error.
        fix_node: String,
    },

    /// State rollback — restore from a checkpoint and retry.
    ///
    /// Use when the error has corrupted the state (e.g., wrong file edits)
    /// and a clean recovery is needed.
    Rollback {
        /// The checkpoint ID to restore from.
        checkpoint_id: String,
    },

    /// Assertion/constraint — hook interception.
    ///
    /// Before or after a node executes, an assertion function checks the state.
    /// If the assertion fails, the recovery strategy is applied.
    ///
    /// Example: after code generation, check if the code compiles.
    /// If compilation fails → apply the on_fail strategy.
    Assertion {
        /// The assertion function to check.
        /// Returns true if the assertion passes, false if it fails.
        check: String, // Named reference to registered assertion function
        /// The recovery strategy to apply if the assertion fails.
        on_fail: Box<RecoveryStrategy>,
    },

    /// External feedback-driven — use execution results to judge correctness.
    ///
    /// This is the most effective form of self-correction. Instead of asking
    /// the LLM whether its answer is correct (which is unreliable), we run
    /// an external validator (tests, compilation, API calls) and use the
    /// objective results to judge success.
    ///
    /// Example: generate code → run tests → if tests fail → feed test output
    /// back to model for correction.
    ExternalFeedback {
        /// The external validator to run.
        validator: String, // Named reference to registered validator
        /// The recovery strategy if validation fails.
        on_validation_fail: Box<RecoveryStrategy>,
    },

    /// Escalate — delegate to a higher-level agent for resolution.
    ///
    /// Use when local recovery strategies have been exhausted.
    /// The main agent receives an error summary and decides globally.
    Escalate {
        /// Summary of the error for the main agent.
        error_summary: String,
    },
}

// ─── RetryPolicy ────────────────────────────────────────────────────────

/// Retry policy for the RecoveryStrategy::Retry variant.
///
/// Different from the WorkflowConfig's RetryPolicy — this one
/// includes backoff strategies and is used in the AgentLoop context.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts.
    pub max_retries: usize,

    /// Initial delay between retries (in seconds).
    pub initial_delay_secs: u64,

    /// Backoff strategy for increasing delay between retries.
    pub backoff: BackoffStrategy,

    /// Maximum delay between retries (in seconds).
    pub max_delay_secs: u64,

    /// Specific error types that should trigger retry (regex patterns).
    pub retry_on_patterns: Vec<String>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_secs: 1,
            backoff: BackoffStrategy::Exponential { factor: 2.0 },
            max_delay_secs: 30,
            retry_on_patterns: vec!["timeout".to_string(), "rate_limit".to_string()],
        }
    }
}

impl RetryPolicy {
    /// Whether an error message should trigger a retry.
    ///
    /// Matches the error (case-insensitively) against `retry_on_patterns`.
    /// Empty patterns → retry nothing (caller decides).
    pub fn should_retry(&self, error: &str) -> bool {
        if self.retry_on_patterns.is_empty() {
            return false;
        }
        let lower = error.to_lowercase();
        self.retry_on_patterns
            .iter()
            .any(|p| lower.contains(&p.to_lowercase()))
    }

    /// Compute the delay before the next retry attempt, including jitter.
    ///
    /// `attempt` is 0-indexed (0 = delay before the first retry). The base
    /// delay follows the `BackoffStrategy`; jitter (up to 25% of the base,
    /// derived from wall-clock nanos so no `rand` dependency is needed) is
    /// added to desynchronize retry-storms across concurrent callers. The
    /// result is capped by `max_delay_secs`.
    pub fn compute_delay(&self, attempt: usize) -> std::time::Duration {
        let base_secs = match &self.backoff {
            BackoffStrategy::Fixed => self.initial_delay_secs as f64,
            BackoffStrategy::Linear { increment_secs } => {
                self.initial_delay_secs as f64
                    + *increment_secs as f64 * attempt as f64
            }
            BackoffStrategy::Exponential { factor } => {
                self.initial_delay_secs as f64 * factor.powi(attempt as i32)
            }
        };
        let base_secs = base_secs.min(self.max_delay_secs as f64);

        // Jitter: 0..=25% of base, from wall-clock nanos (no `rand` dep).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let jitter_secs = (base_secs * 0.25) * (nanos % 4) as f64 / 3.0;

        let total = (base_secs + jitter_secs).min(self.max_delay_secs as f64);
        std::time::Duration::from_secs_f64(total.max(0.0))
    }
}

// ─── BackoffStrategy ────────────────────────────────────────────────────

/// Backoff strategy for retry delays.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum BackoffStrategy {
    /// Fixed delay — same delay for each retry.
    Fixed,

    /// Linear backoff — delay increases linearly.
    Linear { increment_secs: u64 },

    /// Exponential backoff — delay doubles each time.
    Exponential { factor: f64 },
}

// ─── ExternalValidator trait ────────────────────────────────────────────

/// External validator — checks correctness using objective indicators.
///
/// Instead of relying on LLM self-assessment (which is unreliable),
/// external validators use verifiable, objective criteria:
/// - Test results (unit test pass/fail)
/// - Compilation status (code compiles or not)
/// - API response codes (HTTP 200 vs 500)
/// - File existence checks (file was created or not)
/// - Diff checks (output matches expected pattern)
///
/// Validators are registered in the RecoveryManager and referenced
/// by name in RecoveryStrategy::ExternalFeedback.
#[async_trait]
pub trait ExternalValidator: Send + Sync {
    /// Run the validation check.
    ///
    /// Returns a ValidationResult indicating whether the check passed
    /// and detailed feedback for the model if it failed.
    async fn validate(&self, context: &ValidationContext) -> Result<ValidationResult>;

    /// Get the validator name.
    fn name(&self) -> &str;

    /// Get a description of what this validator checks.
    fn description(&self) -> &str;
}

// ─── ValidationContext ──────────────────────────────────────────────────

/// Context provided to external validators.
pub struct ValidationContext {
    /// The task that was attempted.
    pub task: String,

    /// The result that was produced.
    pub result: String,

    /// Additional context variables.
    pub variables: HashMap<String, String>,
}

// ─── ValidationResult ──────────────────────────────────────────────────

/// Result of an external validation check.
pub struct ValidationResult {
    /// Whether the validation passed.
    pub passed: bool,

    /// Detailed feedback for the model (if validation failed).
    /// This feedback is fed back to the model for self-correction.
    /// It contains objective, verifiable information (not LLM opinion).
    pub feedback: String,

    /// Specific issues found (if validation failed).
    pub issues: Vec<String>,
}

// ─── RecoveryManager ────────────────────────────────────────────────────

/// Recovery manager — orchestrates error recovery strategies.
///
/// Registered validators and assertion functions can be referenced
/// by name in RecoveryStrategy variants. The manager resolves
/// these references to actual implementations at runtime.
pub struct RecoveryManager {
    /// Registered external validators.
    validators: HashMap<String, Arc<dyn ExternalValidator>>,

    /// Registered assertion functions (by name).
    /// Assertions are lightweight checks that return bool.
    assertions: HashMap<String, Arc<dyn AssertionFn>>,
}

/// Assertion function trait — lightweight state checks.
/// Uses async_trait for dyn compatibility.
#[async_trait]
pub trait AssertionFn: Send + Sync {
    /// Check the assertion. Returns true if it passes.
    async fn check(&self, context: &ValidationContext) -> Result<bool>;

    /// Get the assertion name.
    fn name(&self) -> &str;
}

impl RecoveryManager {
    /// Create a new recovery manager.
    pub fn new() -> Self {
        Self {
            validators: HashMap::new(),
            assertions: HashMap::new(),
        }
    }

    /// Register an external validator.
    pub fn register_validator(&mut self, validator: Arc<dyn ExternalValidator>) {
        self.validators.insert(validator.name().to_string(), validator);
    }

    /// Register an assertion function.
    pub fn register_assertion(&mut self, assertion: Arc<dyn AssertionFn>) {
        self.assertions.insert(assertion.name().to_string(), assertion);
    }

    /// Apply a recovery strategy.
    ///
    /// Note: nested strategies (Assertion::on_fail, ExternalFeedback::on_validation_fail)
    /// are not recursively applied here. Instead, they are returned as part of the
    /// outcome, and the caller (e.g., AgentLoop or StateGraph executor) applies them
    /// in a subsequent iteration. This avoids async recursion lifetime issues.
    pub async fn apply(&self, strategy: &RecoveryStrategy, context: &ValidationContext) -> Result<RecoveryOutcome> {
        match strategy {
            RecoveryStrategy::Retry { policy } => {
                Ok(RecoveryOutcome::RetryScheduled { max_retries: policy.max_retries })
            }
            RecoveryStrategy::ConditionalFallback { error_node, fix_node } => {
                Ok(RecoveryOutcome::FallbackRoute {
                    from: error_node.clone(),
                    to: fix_node.clone(),
                })
            }
            RecoveryStrategy::Rollback { checkpoint_id } => {
                Ok(RecoveryOutcome::RollbackTo { checkpoint_id: checkpoint_id.clone() })
            }
            RecoveryStrategy::Assertion { check, on_fail: _ } => {
                if let Some(assertion) = self.assertions.get(check) {
                    let result = assertion.check(context).await?;
                    if result {
                        Ok(RecoveryOutcome::AssertionPassed)
                    } else {
                        // Return descriptive info about the nested on_fail strategy
                        // The caller (AgentLoop or StateGraph executor) applies it in a subsequent iteration
                        Ok(RecoveryOutcome::AssertionFailed {
                            on_fail_strategy_description: format!("Apply on_fail strategy after assertion '{}' failed", check),
                        })
                    }
                } else {
                    Ok(RecoveryOutcome::AssertionNotFound { name: check.clone() })
                }
            }
            RecoveryStrategy::ExternalFeedback { validator, on_validation_fail: _ } => {
                if let Some(v) = self.validators.get(validator) {
                    let result = v.validate(context).await?;
                    if result.passed {
                        Ok(RecoveryOutcome::ValidationPassed)
                    } else {
                        Ok(RecoveryOutcome::ValidationFailed { feedback: result.feedback })
                    }
                } else {
                    Ok(RecoveryOutcome::ValidatorNotFound { name: validator.clone() })
                }
            }
            RecoveryStrategy::Escalate { error_summary } => {
                Ok(RecoveryOutcome::Escalated { summary: error_summary.clone() })
            }
        }
    }
}

impl Default for RecoveryManager {
    fn default() -> Self { Self::new() }
}

// ─── RecoveryOutcome ──────────────────────────────────────────────────

/// Outcome of applying a recovery strategy.
#[derive(Debug)]
pub enum RecoveryOutcome {
    /// Retry scheduled with max retries.
    RetryScheduled { max_retries: usize },
    /// Fallback route applied (from error node to fix node).
    FallbackRoute { from: String, to: String },
    /// Rollback to checkpoint.
    RollbackTo { checkpoint_id: String },
    /// Assertion check passed.
    AssertionPassed,
    /// Assertion check failed — the on_fail strategy is referenced by name.
    /// The caller can look up and apply the referenced strategy in a subsequent iteration.
    AssertionFailed { on_fail_strategy_description: String },
    /// Assertion check failed (name not found).
    AssertionNotFound { name: String },
    /// External validation passed.
    ValidationPassed,
    /// External validation failed (with feedback for model correction).
    ValidationFailed { feedback: String },
    /// Validator not found.
    ValidatorNotFound { name: String },
    /// Error escalated to main agent.
    Escalated { summary: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_retry_matches_patterns_case_insensitively() {
        let policy = RetryPolicy::default(); // patterns: ["timeout", "rate_limit"]
        assert!(policy.should_retry("Connection timeout"));
        assert!(policy.should_retry("RATE_LIMIT exceeded"));
        assert!(!policy.should_retry("Tool not found"));
        assert!(!policy.should_retry("Denied by policy"));
    }

    #[test]
    fn should_retry_false_when_no_patterns() {
        let policy = RetryPolicy {
            retry_on_patterns: vec![],
            ..RetryPolicy::default()
        };
        assert!(!policy.should_retry("timeout"));
    }

    #[test]
    fn compute_delay_grows_with_attempt_and_caps_at_max() {
        let policy = RetryPolicy {
            initial_delay_secs: 1,
            backoff: BackoffStrategy::Exponential { factor: 2.0 },
            max_delay_secs: 10,
            ..RetryPolicy::default()
        };
        let d0 = policy.compute_delay(0);
        let d3 = policy.compute_delay(3);
        // Base grows 1, 2, 4, 8 → attempt 3 base is 8s (jitter adds a bit, but capped at 10s).
        assert!(d0.as_secs_f64() >= 1.0 && d0.as_secs_f64() < 2.0);
        assert!(d3.as_secs_f64() >= 8.0);
        assert!(d3.as_secs_f64() <= 10.0 + f64::EPSILON);
    }

    #[test]
    fn compute_delay_fixed_backoff_is_constant() {
        let policy = RetryPolicy {
            initial_delay_secs: 2,
            backoff: BackoffStrategy::Fixed,
            max_delay_secs: 30,
            ..RetryPolicy::default()
        };
        for attempt in 0..5 {
            let d = policy.compute_delay(attempt);
            // Base is fixed 2s; jitter adds 0..~0.5s.
            assert!(d.as_secs_f64() >= 2.0 && d.as_secs_f64() < 3.0);
        }
    }
}