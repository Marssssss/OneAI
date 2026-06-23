//! SWE-bench evaluation adapter (Steps 3–5 of the three-axis eval).
//!
//! This module wires OneAI into the SWE-bench Lite/Verified evaluation loop:
//!
//! - [`instance`] — load instances from a local JSONL (the standard SWE-bench
//!   distribution format; HF fetching stays in the phase-1 Python script).
//! - [`judge`] — an `EvalJudge` backed by the external SWE-bench harness
//!   (Python subprocess). Source of truth for the **能力 (resolved)** axis.
//! - [`runner`] — `SwebenchRunner`: clone → checkout → drive agent → collect
//!   `git diff` → judge. Cost + trace (成本 / 效率) flow through the standard
//!   `EvalResult` fields.
//! - [`leaderboard`] — export the swebench.com submission schema + re-emit the
//!   prediction JSONL.
//!
//! See [[swe-bench-eval-three-axis]] for the three-axis design rationale.

pub mod instance;
pub mod judge;
pub mod leaderboard;
pub mod runner;

pub use instance::{load_instances, load_instances_filtered, SwebenchInstance};
pub use judge::{parse_instance_results, SwebenchJudge, SwebenchVerdict};
pub use leaderboard::{
    render_swebench_leaderboard, write_prediction_jsonl, LeaderboardInstance,
    SwebenchLeaderboard, SWEBENCH_RESOLVED_METRIC,
};
pub use runner::{SwebenchRunner, SwebenchRunnerConfig};
