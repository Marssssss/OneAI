//! # oneai-evolve — OneAI self-evolution loop
//!
//! Outer driver that runs the closed loop:
//! ① trajectory collection → ② EDD scoring → (E2) ③ minimal-subgraph
//! diagnosis → (E3) ④ GEPA Pareto variation+merge → ⑤ re-run.
//!
//! Sits under [`oneai-app`] alongside `oneai-studio` / `oneai-supervisor`
//! (same discipline: **no `AppBuilder` methods** — it's an outer driver that
//! constructs an [`oneai_app::App`] per candidate and drives [`oneai_eval`]
//! directly). The provider is injected by the caller, so the crate is
//! provider-agnostic exactly like `EvalRunner`.
//!
//! ## Phase status
//!
//! - **E0** (MemoryProfile spec-ification) — landed in `oneai-domain`.
//! - **E1** (this crate): the "plumbing" — `CandidateConfig::build_app`
//!   hot-loads a seed pack; `TrajectoryCollector` drives the loop per case and
//!   captures `(Trajectory, TraceTree)` + `EvalResult`; `EvolutionLoop::run`
//!   runs generation 0 only (no variation/selection) and persists a
//!   report + per-case trajectory files.
//! - **E2** (this crate, current): `FailureExtractor` selects low-score
//!   cases; `SubgraphDiagnostician` (default `HeuristicDiagnostician`, opt-in
//!   `LlmDiagnostician`) attributes each failure to suspect `ParamRef`s by
//!   walking the span tree (minimal-subgraph reverse-BFS + tail-N fallback).
//!   Diagnoses are persisted + rendered in the report. No variation yet.
//! - **E3** (GEPA variation + Pareto) → `gepa.rs`.
//! - **E4** (lesson merge + cross-gen memory) → `lessons.rs`.
//!
//! Design doc: `docs/self-evolution-system-2026-08.md`.
//!
//! ## Supply-chain discipline
//!
//! Zero new external dependencies; all `oneai-*` workspace crates. New public
//! enums carry `#[non_exhaustive]` per the v0.2.0 stability commitment.

pub mod candidate;
pub mod cli;
pub mod failure_extractor;
pub mod loop_runner;
pub mod report;
pub mod subgraph;
pub mod trajectory_collector;

pub use candidate::{AgentLoopOverlay, AppHandle, CandidateConfig};
pub use cli::{run_evolve, EvolveRunArgs};
pub use failure_extractor::{extract_failures, FailedCase, FailureExtractor};
pub use loop_runner::{AppBaseline, EvolutionConfig, EvolutionLoop};
pub use report::{CaseRecord, DiagnosisRecord, EvolutionReport};
pub use subgraph::{
    diagnose_heuristic, Diagnosis, HeuristicDiagnostician, LlmDiagnostician, ParamRef, SpanSummary,
    SubgraphDiagnostician, TraceSlice,
};
pub use trajectory_collector::{CaseRun, TrajectoryCollector};
