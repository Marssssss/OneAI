//! # OneAI Agent
//!
//! Agent paradigms: Plan, ReAct, Reflection, Parallel execution with ScopeState isolation.
//! New: Agentic Loop (dynamic decision-making), SubAgent delegation, streaming, prompts.
//! Phase 1: Lifecycle Hooks, Interrupt/Resume, StructuredOutput + ModelRetry.
//! Phase 7: Team Coordinator (multi-agent coordination patterns), Handoff Protocol (agent handoff-as-tool-call),
//! Swarm Orchestrator (dynamic agent pools with capability-driven routing).
//!
//! E2E testing infrastructure: MockProvider, MockTool for deterministic loop verification.

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

// Several submodules re-export identically-named types, which collides under
// glob re-exports. The ambiguous names remain unresolvable via the glob
// (callers use the fully-qualified path) — silence the lint rather than
// fragment the public re-export surface.
#![allow(ambiguous_glob_reexports)]

pub mod scope_state;
pub mod react_agent;
pub mod plan_agent;
pub mod reflection_agent;
pub mod parallel_executor;
pub mod agent_runner;
pub mod agent_loop;
pub mod sub_agent;
pub mod streaming;
pub mod context_assembler;
pub mod error_recovery;
pub mod prompts;
pub mod async_task_runner;
pub mod worktree_isolation;
pub mod mock_provider;
pub mod mock_tool;
pub mod hooks;
pub mod structured_output;
pub mod team;
pub mod handoff;
pub mod swarm;
pub mod skill_tool;
pub mod plan_state;

pub use scope_state::*;
pub use react_agent::*;
pub use plan_agent::*;
pub use reflection_agent::*;
pub use parallel_executor::*;
pub use agent_runner::*;
pub use agent_loop::*;
pub use sub_agent::*;
pub use streaming::*;
pub use context_assembler::*;
pub use error_recovery::*;
pub use prompts::*;
pub use async_task_runner::*;
pub use worktree_isolation::*;
pub use mock_provider::*;
pub use mock_tool::*;
pub use hooks::*;
pub use structured_output::*;
pub use team::*;
pub use handoff::*;
pub use swarm::*;
pub use skill_tool::*;
pub use plan_state::*;

#[cfg(test)]
mod e2e_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::{GlobalState, Reduction, ContentBlock, MemoryEntry};
    use oneai_core::traits::StateReducer;

    #[test]
    fn test_scope_state_creation() {
        let global = GlobalState::new();
        let scope = ScopeState::from_global(&global);
        assert!(scope.global_memory.is_empty());
        assert!(scope.local_sandbox.is_empty());
        assert!(scope.pending_reductions.is_empty());
    }

    #[test]
    fn test_scope_state_add_reduction() {
        let global = GlobalState::new();
        let mut scope = ScopeState::from_global(&global);

        scope.add_reduction(Reduction::UpdateContext {
            key: "result".to_string(),
            value: "42".to_string(),
        });

        assert_eq!(scope.reductions().len(), 1);
    }

    #[test]
    fn test_scope_state_add_memory() {
        let global = GlobalState::new();
        let mut scope = ScopeState::from_global(&global);

        scope.add_local_memory(MemoryEntry {
            id: "local_1".to_string(),
            content: "sub-agent result".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: std::collections::HashMap::new(),
        });

        assert_eq!(scope.local_sandbox.len(), 1);
    }

    #[test]
    fn test_default_state_reducer() {
        let reducer = DefaultStateReducer;
        let mut global = GlobalState::new();

        let reductions = vec![
            Reduction::UpdateContext {
                key: "answer".to_string(),
                value: "hello world".to_string(),
            },
            Reduction::SetResult {
                step_id: "step_1".to_string(),
                result: ContentBlock::Text { text: "result text".to_string() },
            },
        ];

        reducer.reduce(&mut global, reductions).unwrap();

        assert_eq!(global.context.get("answer"), Some(&"hello world".to_string()));
        assert!(global.step_results.contains_key("step_1"));
    }

    #[test]
    fn test_plan_step_serialization() {
        let step = PlanStep {
            id: "step_1".to_string(),
            description: "Search for information".to_string(),
            coupled: false,
            depends_on: vec![],
            status: PlanStepStatus::Pending,
            active_form: None,
        };

        let json = serde_json::to_string(&step).unwrap();
        let parsed: PlanStep = serde_json::from_str(&json).unwrap();
        assert_eq!(step, parsed);
    }

    #[test]
    fn test_plan_step_coupled() {
        let step = PlanStep {
            id: "step_2".to_string(),
            description: "Process results from step 1".to_string(),
            coupled: true,
            depends_on: vec!["step_1".to_string()],
            status: PlanStepStatus::Pending,
            active_form: None,
        };

        let json = serde_json::to_string(&step).unwrap();
        let parsed: PlanStep = serde_json::from_str(&json).unwrap();
        assert!(parsed.coupled);
        assert_eq!(parsed.depends_on, vec!["step_1"]);
    }

    #[test]
    fn test_parse_plan_steps_valid_json() {
        let raw = "[{\"id\":\"step_1\",\"description\":\"Search\",\"coupled\":false,\"depends_on\":[]}]";
        let steps = plan_agent::parse_plan_steps(raw).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, "step_1");
        assert!(!steps[0].coupled);
    }

    #[test]
    fn test_parse_plan_steps_embedded_json() {
        let raw = "Here is my plan:\n[{\"id\":\"step_1\",\"description\":\"Do A\",\"coupled\":false,\"depends_on\":[]}]\nLet me know if you need more.";
        let steps = plan_agent::parse_plan_steps(raw).unwrap();
        assert_eq!(steps.len(), 1);
    }

    #[test]
    fn test_parse_plan_steps_fallback() {
        // Non-JSON input should create a single-step fallback plan
        let raw = "Just do it directly";
        let steps = plan_agent::parse_plan_steps(raw).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, "step_1");
    }

    #[test]
    fn test_parse_plan_steps_multi_step() {
        let raw = "[{\"id\":\"step_1\",\"description\":\"Search\",\"coupled\":false,\"depends_on\":[]},{\"id\":\"step_2\",\"description\":\"Process\",\"coupled\":true,\"depends_on\":[\"step_1\"]}]";
        let steps = plan_agent::parse_plan_steps(raw).unwrap();
        assert_eq!(steps.len(), 2);
        assert!(!steps[0].coupled);
        assert!(steps[1].coupled);
    }

    #[test]
    fn test_react_config_default() {
        let config = ReActConfig::default();
        assert_eq!(config.max_iterations, 10);
        assert!(!config.use_streaming);
    }

    #[test]
    fn test_plan_config_default() {
        let config = PlanConfig::default();
        assert_eq!(config.temperature, Some(0.0));
    }

    #[test]
    fn test_reflection_config_default() {
        let config = ReflectionConfig::default();
        assert_eq!(config.max_retries, 2);
    }

    #[test]
    fn test_agent_runner_config_default() {
        let config = AgentRunnerConfig::default();
        assert!(config.use_planning);
        assert!(config.use_reflection);
        assert!(config.use_parallel);
    }

    #[test]
    fn test_parallel_step_result() {
        let result = ParallelStepResult {
            step_id: "step_1".to_string(),
            result: "search completed".to_string(),
            success: true,
            reductions: vec![Reduction::SetResult {
                step_id: "step_1".to_string(),
                result: ContentBlock::Text { text: "search completed".to_string() },
            }],
        };
        assert!(result.success);
        assert_eq!(result.reductions.len(), 1);
    }
}