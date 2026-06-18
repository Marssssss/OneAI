//! EvalSuite and EvalSuiteBuilder — collection of cases + metrics.
//!
//! An EvalSuite groups related evaluation cases with a set of scoring
//! metrics. It optionally ties to a DomainPack name, so the EvalRunner
//! can configure the agent appropriately before running the cases.
//!
//! The EvalSuiteBuilder provides a fluent API for constructing suites,
//! mirroring the DomainPackBuilder pattern.

use std::sync::Arc;
use std::collections::HashMap;


use crate::eval_case::EvalCase;
use crate::eval_metric::EvalMetric;

// ─── EvalSuite ───────────────────────────────────────────────────────────

/// A collection of evaluation cases + scoring metrics.
///
/// The EvalSuite is the primary unit of evaluation. It contains:
/// - A set of `EvalCase` instances to run
/// - A set of `EvalMetric` instances to score each case
/// - Optional association with a DomainPack name
///
/// Suites can be created programmatically via `EvalSuiteBuilder` or
/// loaded from configuration files (YAML/TOML).
///
/// Usage:
/// ```ignore
/// let suite = EvalSuiteBuilder::new("math_basics")
///     .description("Basic math reasoning evaluation")
///     .domain("coding")
///     .case(EvalCase::new("2+2", ExpectedOutput::Exact { answer: "4" }))
///     .metric(Arc::new(ExactMatchMetric))
///     .build();
/// ```
pub struct EvalSuite {
    /// Suite name (for identification and reporting).
    pub name: String,

    /// Human-readable description.
    pub description: String,

    /// The evaluation cases.
    pub cases: Vec<EvalCase>,

    /// The scoring metrics to apply to each case.
    pub metrics: Vec<Arc<dyn EvalMetric>>,

    /// Optional DomainPack name — the EvalRunner will configure the
    /// agent with this domain pack before running cases.
    pub domain: Option<String>,
}

impl EvalSuite {
    /// Create a new suite with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            cases: Vec::new(),
            metrics: Vec::new(),
            domain: None,
        }
    }

    /// Get the number of cases.
    pub fn case_count(&self) -> usize {
        self.cases.len()
    }

    /// Get the number of metrics.
    pub fn metric_count(&self) -> usize {
        self.metrics.len()
    }

    /// Filter cases by metadata key-value pair.
    pub fn filter_cases(&self, key: &str, value: &str) -> Vec<&EvalCase> {
        self.cases.iter()
            .filter(|c| c.metadata.get(key).map(|v| v == value).unwrap_or(false))
            .collect()
    }

    /// Get metric names.
    pub fn metric_names(&self) -> Vec<&str> {
        self.metrics.iter().map(|m| m.name()).collect()
    }
}

// Manual Debug impl — dyn EvalMetric doesn't implement Debug
impl std::fmt::Debug for EvalSuite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvalSuite")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("cases_count", &self.cases.len())
            .field("metrics_count", &self.metrics.len())
            .field("metric_names", &self.metric_names())
            .field("domain", &self.domain)
            .finish()
    }
}

// ─── EvalSuiteBuilder ────────────────────────────────────────────────────

/// Builder for constructing EvalSuite instances.
///
/// Follows the same fluent builder pattern as DomainPackBuilder.
pub struct EvalSuiteBuilder {
    name: String,
    description: String,
    cases: Vec<EvalCase>,
    metrics: Vec<Arc<dyn EvalMetric>>,
    domain: Option<String>,
}

impl EvalSuiteBuilder {
    /// Start building a suite with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            cases: Vec::new(),
            metrics: Vec::new(),
            domain: None,
        }
    }

    /// Set the description.
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Add an evaluation case.
    pub fn case(mut self, case: EvalCase) -> Self {
        self.cases.push(case);
        self
    }

    /// Add multiple evaluation cases.
    pub fn cases(mut self, cases: Vec<EvalCase>) -> Self {
        self.cases.extend(cases);
        self
    }

    /// Add a scoring metric.
    pub fn metric(mut self, metric: Arc<dyn EvalMetric>) -> Self {
        self.metrics.push(metric);
        self
    }

    /// Add multiple scoring metrics.
    pub fn metrics(mut self, metrics: Vec<Arc<dyn EvalMetric>>) -> Self {
        self.metrics.extend(metrics);
        self
    }

    /// Set the associated DomainPack name.
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Build the EvalSuite.
    pub fn build(self) -> EvalSuite {
        EvalSuite {
            name: self.name,
            description: self.description,
            cases: self.cases,
            metrics: self.metrics,
            domain: self.domain,
        }
    }
}

// ─── SuiteRegistry ───────────────────────────────────────────────────────

/// Registry of named eval suites — for CLI `oneai eval list` and discovery.
///
/// Stores suites by name, allowing lookup and enumeration. The registry
/// is populated with built-in suites by default, and can be extended
/// with user-defined suites.
pub struct SuiteRegistry {
    suites: HashMap<String, EvalSuite>,
}

impl SuiteRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            suites: HashMap::new(),
        }
    }

    /// Create a registry with all built-in suites.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register(crate::builtin_suites::coding_suite());
        reg.register(crate::builtin_suites::tool_use_suite());
        reg.register(crate::builtin_suites::general_suite());
        reg
    }

    /// Register a suite.
    pub fn register(&mut self, suite: EvalSuite) {
        self.suites.insert(suite.name.clone(), suite);
    }

    /// Look up a suite by name.
    pub fn get(&self, name: &str) -> Option<&EvalSuite> {
        self.suites.get(name)
    }

    /// List all registered suite names.
    pub fn list_names(&self) -> Vec<&str> {
        self.suites.keys().map(|s| s.as_str()).collect()
    }

    /// List all registered suites with brief descriptions.
    pub fn list(&self) -> Vec<(&str, &str)> {
        self.suites.values()
            .map(|s| (s.name.as_str(), s.description.as_str()))
            .collect()
    }

    /// Get the number of registered suites.
    pub fn count(&self) -> usize {
        self.suites.len()
    }
}

impl Default for SuiteRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval_case::ExpectedOutput;
    use crate::builtin_metrics::ExactMatchMetric;

    #[test]
    fn test_eval_suite_builder() {
        let suite = EvalSuiteBuilder::new("test_suite")
            .description("A test evaluation suite")
            .case(EvalCase::new("What is 2+2?", ExpectedOutput::exact("4")))
            .case(EvalCase::new("What is 3*5?", ExpectedOutput::exact("15")))
            .metric(Arc::new(ExactMatchMetric))
            .domain("math")
            .build();

        assert_eq!(suite.name, "test_suite");
        assert_eq!(suite.description, "A test evaluation suite");
        assert_eq!(suite.case_count(), 2);
        assert_eq!(suite.metric_count(), 1);
        assert_eq!(suite.domain.as_deref(), Some("math"));
    }

    #[test]
    fn test_eval_suite_filter_cases() {
        let suite = EvalSuiteBuilder::new("test")
            .case(EvalCase::new("a", ExpectedOutput::exact("1")).difficulty(1).domain("math"))
            .case(EvalCase::new("b", ExpectedOutput::exact("2")).difficulty(3).domain("coding"))
            .case(EvalCase::new("c", ExpectedOutput::exact("3")).difficulty(5).domain("math"))
            .build();

        let math_cases = suite.filter_cases("domain", "math");
        assert_eq!(math_cases.len(), 2);

        let hard_cases = suite.filter_cases("difficulty", "5");
        assert_eq!(hard_cases.len(), 1);
    }

    #[test]
    fn test_eval_suite_metric_names() {
        let suite = EvalSuiteBuilder::new("test")
            .metric(Arc::new(ExactMatchMetric))
            .build();

        assert_eq!(suite.metric_names(), vec!["exact_match"]);
    }

    #[test]
    fn test_suite_registry() {
        let registry = SuiteRegistry::with_builtins();
        assert!(registry.count() >= 3); // coding, tool_use, general

        let names = registry.list_names();
        assert!(names.contains(&"coding_basics"));
        assert!(names.contains(&"tool_use"));
        assert!(names.contains(&"general"));
    }

    #[test]
    fn test_suite_registry_custom() {
        let mut registry = SuiteRegistry::new();
        let suite = EvalSuiteBuilder::new("custom")
            .description("Custom suite")
            .build();
        registry.register(suite);

        assert_eq!(registry.count(), 1);
        assert!(registry.get("custom").is_some());
        assert!(registry.get("nonexistent").is_none());
    }
}
