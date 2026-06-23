//! # OneAI Eval — Structured evaluation framework for agent quality assessment
//!
//! The oneai-eval crate provides a comprehensive evaluation system for measuring
//! agent quality across multiple dimensions:
//!
//! - **EvalCase**: Individual test case (input + expected output)
//! - **EvalMetric**: Scoring strategy trait (exact match, contains, regex, LLM judge, trajectory)
//! - **EvalSuite**: Collection of cases + metrics, optionally tied to a DomainPack
//! - **EvalRunner**: Execution engine — runs cases against an App, collects traces, scores results
//! - **EvalReport**: Aggregated results with summary statistics + JSON/Markdown output
//!
//! ## Usage
//!
//! ```ignore
//! // Create a simple eval suite
//! let suite = EvalSuiteBuilder::new("math_basics")
//!     .description("Basic math reasoning")
//!     .case(EvalCase::new("2+2", ExpectedOutput::Exact { answer: "4" }))
//!     .case(EvalCase::new("3*5", ExpectedOutput::Contains { substrings: vec!["15"], case_sensitive: false }))
//!     .metric(Arc::new(ExactMatchMetric))
//!     .metric(Arc::new(ContainsMatchMetric))
//!     .build();
//!
//! // Run against an App
//! let app = AppBuilder::new().provider(provider).auto_approval_gate().build().await?;
//! let runner = EvalRunner::new(app);
//! let report = runner.run(&suite).await?;
//!
//! // Print report
//! println!("{}", report.to_markdown());
//! ```

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.


pub mod eval_case;
pub mod eval_metric;
pub mod eval_suite;
pub mod eval_result;
pub mod eval_runner;
pub mod builtin_metrics;
pub mod builtin_suites;
pub mod report_format;
pub mod swebench;

pub use eval_case::*;
pub use eval_metric::*;
pub use eval_suite::*;
pub use eval_result::*;
pub use eval_runner::*;
pub use builtin_metrics::*;
pub use builtin_suites::*;
pub use report_format::*;
