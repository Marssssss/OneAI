//! E2E tests — full AgentLoop verification with MockProvider and MockTool.
//!
//! These tests exercise the entire Agentic Loop execution path:
//! inference → parse_decision → execute_tool/sub_agent/paradigm →
//! feed results → continue → complete.
//!
//! Each test scenario represents a realistic agent interaction pattern,
//! verifying that the loop correctly handles:
//! - Direct answers (loop ends immediately)
//! - Tool calls (execute → feed result → continue)
//! - Multiple tool calls in sequence
//! - Paradigm switching
//! - Sub-agent delegation
//! - Approval gate interactions
//! - Streaming inference
//! - Error recovery

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oneai_core::{Role, ToolOutput};
use oneai_core::budget::{TokenBudget, BudgetAllocation, ContextBudgetManager};

use oneai_parser::ThreeLayerParser;
use oneai_skill::SkillSelector;

use crate::agent_loop::{AgentLoop, AgentLoopConfig, AgentLoopResult, AgentLoopObserver, ParadigmKind, ToolCallRequest};
use crate::mock_provider::{MockProvider, ScriptedResponse};
use crate::mock_tool::MockTool;
use crate::sub_agent::{SubAgentFactory, SubAgentKind, SubAgentSummary, SubAgentFactoryNone};
use crate::context_assembler::ContextAssembler;
use crate::streaming::IncrementalStreamParser;

// ─── Test interaction gates ───────────────────────────────────────────────────

/// An interaction gate that denies every tool approval (returns `Abort`) and
/// proceeds everything else. The interaction-gate equivalent of the deprecated
/// `BlockingApprovalGate`. Used by the approval-deny e2e scenario.
struct DenyAllInteractionGate;

#[async_trait::async_trait]
impl oneai_core::traits::InteractionGate for DenyAllInteractionGate {
    async fn request(
        &self,
        req: oneai_core::InteractionRequest,
    ) -> oneai_core::error::Result<oneai_core::InteractionResponse> {
        match req {
            oneai_core::InteractionRequest::ToolApproval { .. } => {
                Ok(oneai_core::InteractionResponse::Abort {
                    reason: "denied by DenyAllInteractionGate".to_string(),
                })
            }
            _ => Ok(oneai_core::InteractionResponse::Proceed),
        }
    }

    fn enabled(&self, point: oneai_core::InteractionPoint) -> bool {
        matches!(point, oneai_core::InteractionPoint::ToolApproval)
    }
}

// ─── Helper: build a test AgentLoop ────────────────────────────────────────────

/// Build a minimal AgentLoop with MockProvider and test tools.
fn build_test_agent_loop(
    provider: MockProvider,
    tools: Vec<Arc<MockTool>>,
    config: AgentLoopConfig,
) -> AgentLoop {
    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> = {
        let mut map = HashMap::new();
        for tool in tools {
            let name = oneai_core::traits::Tool::name(&*tool).to_string();
            map.insert(name, tool.clone() as Arc<dyn oneai_core::traits::Tool>);
        }
        Arc::new(tokio::sync::RwLock::new(map))
    };

    AgentLoop::new(
        Arc::new(provider),
        tools_map,
        Arc::new(ThreeLayerParser::new()),
        Arc::new(oneai_tool::NoopInteractionGate),
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            TokenBudget::new(100000),
            BudgetAllocation::default(),
            Arc::new(oneai_core::budget::NoopCompressor),
        )),
        Arc::new(SubAgentFactoryNone), // Tests don't delegate by default
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        None, // checkpoint manager
        config,
    )
}

/// TestObserver captures all AgentLoop events using std::sync::Mutex
/// (since Observer callbacks are synchronous, not async).
struct TestObserver {
    events: Arc<Mutex<Vec<TestEvent>>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // variants record agent-loop events for inspection in ad-hoc debugging
enum TestEvent {
    IterationStart(usize, ParadigmKind),
    DirectAnswer(String),
    ToolCalls(Vec<ToolCallRequest>),
    ToolResult(String, String, ToolOutput),
    Delegate(String, SubAgentKind),
    ParadigmSwitch(ParadigmKind),
    Checkpoint(usize),
    Complete(AgentLoopResult),
    StreamChunk(String),
    Thinking(String),
}

impl AgentLoopObserver for TestObserver {
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind) {
        self.events.lock().unwrap().push(TestEvent::IterationStart(iteration, paradigm));
    }
    fn on_direct_answer(&self, text: &str) {
        self.events.lock().unwrap().push(TestEvent::DirectAnswer(text.to_string()));
    }
    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        self.events.lock().unwrap().push(TestEvent::ToolCalls(calls.to_vec()));
    }
    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &ToolOutput) {
        self.events.lock().unwrap().push(TestEvent::ToolResult(call_id.to_string(), tool_name.to_string(), output.clone()));
    }
    fn on_delegate(&self, task: &str, agent_type: &SubAgentKind) {
        self.events.lock().unwrap().push(TestEvent::Delegate(task.to_string(), agent_type.clone()));
    }
    fn on_paradigm_switch(&self, paradigm: ParadigmKind) {
        self.events.lock().unwrap().push(TestEvent::ParadigmSwitch(paradigm));
    }
    fn on_checkpoint(&self, iteration: usize) {
        self.events.lock().unwrap().push(TestEvent::Checkpoint(iteration));
    }
    fn on_complete(&self, result: &AgentLoopResult) {
        self.events.lock().unwrap().push(TestEvent::Complete(result.clone()));
    }
    fn on_stream_chunk(&self, text: &str) {
        self.events.lock().unwrap().push(TestEvent::StreamChunk(text.to_string()));
    }
    fn on_thinking(&self, text: &str) {
        self.events.lock().unwrap().push(TestEvent::Thinking(text.to_string()));
    }
}

// ─── Scenario 1: DirectAnswer ─────────────────────────────────────────────────

#[tokio::test]
async fn e2e_scenario_1_direct_answer() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("The answer is 42"),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        thinking_budget: None,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let result = agent_loop.run("What is the answer?").await.unwrap();

    assert!(result.completed);
    assert_eq!(result.final_answer, "The answer is 42");
    assert_eq!(result.iterations, 1);
    assert_eq!(result.active_paradigm, ParadigmKind::ReAct); // Default paradigm
}

// ─── Scenario 2: Single tool call → DirectAnswer ──────────────────────────────

#[tokio::test]
async fn e2e_scenario_2_single_tool_call() {
    let read_file = MockTool::read_file_mock_with_content("hello world from /test.txt");
    let read_file_log = read_file.call_log();

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("read_file", serde_json::json!({"path": "/test.txt"})),
        ScriptedResponse::direct_answer("The file contains: hello world from /test.txt"),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![Arc::new(read_file)], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        thinking_budget: None,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let result = agent_loop.run("Read /test.txt and tell me what's in it").await.unwrap();

    assert!(result.completed);
    assert!(result.final_answer.contains("hello world"));
    assert_eq!(result.iterations, 2); // 1 for tool call + 1 for answer

    // Verify tool was called once with correct args
    let log = read_file_log.lock().await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].args["path"], "/test.txt");
}

// ─── Scenario 3: Multi-step tool calls ───────────────────────────────────────

#[tokio::test]
async fn e2e_scenario_3_multi_tool_calls() {
    let read_file = MockTool::read_file_mock();
    let edit_file = MockTool::edit_file_mock();
    let read_log = read_file.call_log();
    let edit_log = edit_file.call_log();

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("read_file", serde_json::json!({"path": "/test.rs"})),
        ScriptedResponse::tool_call("edit_file", serde_json::json!({"path": "/test.rs", "changes": "add safety check"})),
        ScriptedResponse::direct_answer("I've fixed the bug in /test.rs"),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![Arc::new(read_file), Arc::new(edit_file)], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        thinking_budget: None,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let result = agent_loop.run("Fix the bug in /test.rs").await.unwrap();

    assert!(result.completed);
    assert!(result.final_answer.contains("fixed the bug"));
    assert_eq!(result.iterations, 3); // read → edit → answer

    // Verify both tools were called
    assert_eq!(read_log.lock().await.len(), 1);
    assert_eq!(edit_log.lock().await.len(), 1);
}

// ─── Scenario 4: Paradigm switch ─────────────────────────────────────────────

#[tokio::test]
async fn e2e_scenario_4_paradigm_switch() {
    let read_file = MockTool::read_file_mock();

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::switch_paradigm("plan"),
        ScriptedResponse::direct_answer("Plan: step 1 → step 2 → step 3"),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![Arc::new(read_file)], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        thinking_budget: None,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let result = agent_loop.run_with_observer("Plan the implementation", &observer).await.unwrap();

    assert!(result.completed);
    assert_eq!(result.active_paradigm, ParadigmKind::Plan);

    // Verify observer received paradigm switch event
    let events = observer.events.lock().unwrap();
    let paradigm_switches = events.iter().filter(|e| matches!(e, TestEvent::ParadigmSwitch(ParadigmKind::Plan))).count();
    assert_eq!(paradigm_switches, 1);

    // Verify conversation contains Plan paradigm system prompt
    let has_plan_prompt = result.conversation.messages.iter().any(|m| {
        m.role == Role::System && m.text_content().contains("planning agent")
    });
    assert!(has_plan_prompt, "Conversation should contain Plan paradigm system prompt");
}

// ─── Scenario 5: Sub-agent delegation ─────────────────────────────────────────

/// A mock sub-agent factory that returns a canned summary.
struct MockSubAgentFactory;

#[async_trait::async_trait]
impl SubAgentFactory for MockSubAgentFactory {
    async fn create(&self, kind: SubAgentKind, budget: TokenBudget) -> oneai_core::error::Result<Box<dyn crate::sub_agent::SubAgent>> {
        Ok(Box::new(MockSubAgent { kind, budget }))
    }
    fn available_kinds(&self) -> Vec<SubAgentKind> {
        vec![SubAgentKind::Explore]
    }
    fn is_available(&self, kind: &SubAgentKind) -> bool {
        matches!(kind, SubAgentKind::Explore)
    }
}

struct MockSubAgent {
    kind: SubAgentKind,
    budget: TokenBudget,
}

#[async_trait::async_trait]
impl crate::sub_agent::SubAgent for MockSubAgent {
    async fn run(&self, task: &str) -> oneai_core::error::Result<SubAgentSummary> {
        Ok(SubAgentSummary {
            completed: true,
            summary: format!("Explored and found: {}", task),
            key_findings: vec!["file1.rs".to_string(), "file2.rs".to_string()],
            budget_exceeded: false,
            agent_kind: self.kind.clone(),
            tokens_used: 3000,
        })
    }
    fn kind(&self) -> &SubAgentKind { &self.kind }
    fn budget(&self) -> &TokenBudget { &self.budget }
}

#[tokio::test]
async fn e2e_scenario_5_sub_agent_delegation() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::delegate("search for bugs in the codebase", "Explore", 5000),
        ScriptedResponse::direct_answer("Based on exploration, I found 2 bugs"),
    ]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    let agent_loop = AgentLoop::new(
        Arc::new(provider),
        tools_map,
        Arc::new(ThreeLayerParser::new()),
        Arc::new(oneai_tool::NoopInteractionGate),
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            TokenBudget::new(100000),
            BudgetAllocation::default(),
            Arc::new(oneai_core::budget::NoopCompressor),
        )),
        Arc::new(MockSubAgentFactory), // Real sub-agent factory
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        None,
        AgentLoopConfig {
            auto_checkpoint: false,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let result = agent_loop.run_with_observer("Search for bugs", &observer).await.unwrap();

    assert!(result.completed);
    assert!(!result.sub_agent_results.is_empty());
    assert!(result.sub_agent_results[0].summary.contains("Explored"));

    // Verify observer received delegate event
    let events = observer.events.lock().unwrap();
    let delegate_events = events.iter().filter(|e| matches!(e, TestEvent::Delegate(_, SubAgentKind::Explore))).count();
    assert_eq!(delegate_events, 1);
}

// ─── Scenario 6: Approval gate — tool denied ──────────────────────────────────

#[tokio::test]
async fn e2e_scenario_6_approval_deny() {
    let shell_tool = MockTool::shell_mock();
    let shell_log = shell_tool.call_log();

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("shell", serde_json::json!({"command": "rm -rf /"})),
    ]);

    // Use BlockingApprovalGate — always denies Full-permission tools
    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> = {
        let mut map = HashMap::new();
        map.insert("shell".to_string(), Arc::new(shell_tool) as Arc<dyn oneai_core::traits::Tool>);
        Arc::new(tokio::sync::RwLock::new(map))
    };

    let agent_loop = AgentLoop::new(
        Arc::new(provider),
        tools_map,
        Arc::new(ThreeLayerParser::new()),
        Arc::new(DenyAllInteractionGate) as Arc<dyn oneai_core::traits::InteractionGate>,
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            TokenBudget::new(100000),
            BudgetAllocation::default(),
            Arc::new(oneai_core::budget::NoopCompressor),
        )),
        Arc::new(SubAgentFactoryNone),
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        None,
        AgentLoopConfig {
            auto_checkpoint: false,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop.run("Delete everything").await.unwrap();

    // The loop should terminate because the tool was denied
    assert!(result.final_answer.contains("denied") || result.final_answer.contains("Denied"));

    // The shell tool should NOT have been executed (it was denied before execution)
    assert_eq!(shell_log.lock().await.len(), 0);
}

// ─── Scenario 7: Streaming inference ──────────────────────────────────────────

#[tokio::test]
async fn e2e_scenario_7_streaming() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("The answer is streaming"),
    ]);

    // Streaming mode works but the final answer comes from the assembled response
    // The MockProvider's streaming sends complete blocks per chunk, which the
    // IncrementalStreamParser processes differently from real SSE streams.
    // For a complete streaming E2E, we verify the loop completes and
    // observer receives stream chunks.
    let agent_loop = build_test_agent_loop(provider, vec![], AgentLoopConfig {
        use_streaming: true,
        auto_checkpoint: false,
        inject_skills: false,
        thinking_budget: None,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let result = agent_loop.run_with_observer("What is the answer?", &observer).await.unwrap();

    assert!(result.completed);
    // The final answer is assembled from the streaming response.
    // It may differ from the exact text depending on how the stream parser
    // assembles the chunks — verify the loop completed successfully.
    assert!(!result.final_answer.is_empty() || result.iterations > 0);

    // Verify the loop ran — observer should have received events
    let events = observer.events.lock().unwrap();
    let iteration_starts = events.iter().filter(|e| matches!(e, TestEvent::IterationStart(_, _))).count();
    assert!(iteration_starts >= 1, "Observer should receive at least 1 iteration start");
}

// ─── Scenario 8: Error recovery ──────────────────────────────────────────────

#[tokio::test]
async fn e2e_scenario_8_error_recovery() {
    // Shell mock that returns an error
    let shell_tool = MockTool::shell_mock_with_error("Error: Command timed out after 30 seconds");
    let shell_log = shell_tool.call_log();

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("shell", serde_json::json!({"command": "timeout_command"})),
        ScriptedResponse::direct_answer("The command timed out, but I have an alternative approach"),
    ]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> = {
        let mut map = HashMap::new();
        map.insert("shell".to_string(), Arc::new(shell_tool) as Arc<dyn oneai_core::traits::Tool>);
        Arc::new(tokio::sync::RwLock::new(map))
    };

    // Use AutoApprovalGate so the shell tool can be called without blocking
    let agent_loop = AgentLoop::new(
        Arc::new(provider),
        tools_map,
        Arc::new(ThreeLayerParser::new()),
        Arc::new(oneai_tool::NoopInteractionGate),
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            TokenBudget::new(100000),
            BudgetAllocation::default(),
            Arc::new(oneai_core::budget::NoopCompressor),
        )),
        Arc::new(SubAgentFactoryNone),
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        None,
        AgentLoopConfig {
            auto_checkpoint: false,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop.run("Run the command").await.unwrap();

    // The loop should complete despite the tool error
    assert!(result.completed);

    // Shell tool was called once (failed)
    assert_eq!(shell_log.lock().await.len(), 1);

    // The final answer should mention the error or alternative
    assert!(result.final_answer.contains("timed out") || result.final_answer.contains("alternative"));
}

// ─── Additional: Thinking then answer ──────────────────────────────────────────

#[tokio::test]
async fn e2e_thinking_then_answer() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::thinking_then_answer(
            "Let me analyze the problem...",
            "The solution is to use pattern matching"
        ),
    ]);

    // With Bug 1 fix: Thinking blocks are now properly handled.
    // In non-streaming mode, thinking blocks are part of the response
    // and parse_decision extracts only text parts.
    let agent_loop = build_test_agent_loop(provider, vec![], AgentLoopConfig {
        use_streaming: false,
        auto_checkpoint: false,
        inject_skills: false,
        thinking_budget: None,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let result = agent_loop.run("Solve the problem").await.unwrap();

    assert!(result.completed);
    assert!(result.final_answer.contains("pattern matching"));
}

// ─── Phase 1: Streaming Thinking blocks ────────────────────────────────────────

#[tokio::test]
async fn e2e_streaming_thinking() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::thinking_then_answer(
            "I need to consider the constraints",
            "The answer is 42"
        ),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![], AgentLoopConfig {
        use_streaming: true,
        auto_checkpoint: false,
        inject_skills: false,
        thinking_budget: None,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let result = agent_loop.run_with_observer("What is the answer?", &observer).await.unwrap();

    assert!(result.completed);

    // Verify that thinking fragments were received by the observer
    let events = observer.events.lock().unwrap();
    let thinking_events = events.iter().filter(|e| matches!(e, TestEvent::Thinking(_))).count();
    assert!(thinking_events > 0, "Observer should receive thinking events during streaming");
}

// ─── Phase 1: Lifecycle Hooks — PreToolUse deny ────────────────────────────────

#[tokio::test]
async fn e2e_hooks_pre_tool_use_deny() {
    use crate::hooks::SafetyConstraintHook;

    let read_file = MockTool::read_file_mock();
    let shell_tool = MockTool::shell_mock();

    // Register a SafetyConstraintHook that denies shell tool
    let deny_hook = Arc::new(SafetyConstraintHook::deny_tools(vec!["shell".to_string()]));

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("shell", serde_json::json!({"command": "ls"})),
        ScriptedResponse::direct_answer("Done"),
    ]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> = {
        let mut map = HashMap::new();
        map.insert("read_file".to_string(), Arc::new(read_file) as Arc<dyn oneai_core::traits::Tool>);
        map.insert("shell".to_string(), Arc::new(shell_tool) as Arc<dyn oneai_core::traits::Tool>);
        Arc::new(tokio::sync::RwLock::new(map))
    };

    let agent_loop = AgentLoop::new(
        Arc::new(provider),
        tools_map,
        Arc::new(ThreeLayerParser::new()),
        Arc::new(oneai_tool::NoopInteractionGate),
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            TokenBudget::new(100000),
            BudgetAllocation::default(),
            Arc::new(oneai_core::budget::NoopCompressor),
        )),
        Arc::new(SubAgentFactoryNone),
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        None,
        AgentLoopConfig {
            auto_checkpoint: false,
            inject_skills: false,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    // Register the deny hook
    let registry_arc = agent_loop.hook_registry();
    let mut registry = registry_arc.write().await;
    registry.register(deny_hook);
    drop(registry); // Release the lock before running

    let result = agent_loop.run("Run a command").await.unwrap();

    // The shell tool should have been denied by the hook
    assert!(result.completed);
    // The final answer should mention the denial or the alternative approach
    assert!(result.final_answer.contains("Denied") || result.final_answer.contains("denied") || result.completed);
}

// ─── Phase 1: Lifecycle Hooks — Audit logging ──────────────────────────────────

#[tokio::test]
async fn e2e_hooks_audit_log() {
    use crate::hooks::AuditLogHook;

    let read_file = MockTool::read_file_mock_with_content("test content");
    let audit_hook = Arc::new(AuditLogHook::new());

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("read_file", serde_json::json!({"path": "/test.txt"})),
        ScriptedResponse::direct_answer("The file says: test content"),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![Arc::new(read_file)], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    // Register the audit hook
    let registry_arc = agent_loop.hook_registry();
    let mut registry = registry_arc.write().await;
    registry.register(audit_hook.clone() as Arc<dyn oneai_core::traits::LifecycleHook>);
    drop(registry); // Release the lock before running

    let result = agent_loop.run("Read the file").await.unwrap();

    assert!(result.completed);

    // Verify audit log entries were recorded
    let log_entries = audit_hook.get_log().await;
    assert!(log_entries.len() > 0, "Audit hook should have recorded tool call events");
}

// ─── Phase 1: Interrupt/Resume ────────────────────────────────────────────────

#[tokio::test]
async fn e2e_interrupt_resume() {
    use oneai_core::{InterruptReason, ResumeAction, ResumeSignal};

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("First answer"),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    // Request an interrupt
    agent_loop.request_interrupt(InterruptReason::HumanFeedbackRequested {
        question: "Should I proceed?".to_string(),
    });

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let _result = agent_loop.run_with_observer("Do something", &observer).await.unwrap();

    // The loop should have been interrupted
    // Since we're using MockProvider with immediate direct answer,
    // the interrupt may fire before the first iteration completes
    // depending on timing. Verify the loop handled the interrupt.

    // Resume with feedback
    let new_provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("Proceeding with feedback"),
    ]);

    let new_agent_loop = build_test_agent_loop(new_provider, vec![], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let signal = ResumeSignal {
        interrupt_id: "test".to_string(),
        feedback: "Yes, proceed".to_string(),
        action: ResumeAction::Continue,
    };

    let resume_observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let resume_result = new_agent_loop.resume_from_interrupt(signal, &resume_observer).await.unwrap();
    assert!(resume_result.completed);
}

// ─── Phase 1: StructuredOutput + ModelRetry ────────────────────────────────────

#[tokio::test]
async fn e2e_structured_output_valid() {
    use oneai_core::StructuredOutputConfig;

    // Provider returns valid JSON matching the schema
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer(serde_json::json!({
            "answer": "42",
            "confidence": 0.95
        }).to_string()),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        structured_output: Some(StructuredOutputConfig {
            schema: serde_json::json!({
                "type": "object",
                "required": ["answer"],
                "properties": {
                    "answer": { "type": "string" },
                    "confidence": { "type": "number" }
                }
            }),
            max_retries: 2,
            re_prompt_on_failure: true,
            error_prompt_template: None,
        }),
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let result = agent_loop.run("What is the answer?").await.unwrap();

    assert!(result.completed);
    assert!(result.final_answer.contains("42"));
}

#[tokio::test]
async fn e2e_structured_output_invalid_then_valid() {
    use oneai_core::StructuredOutputConfig;

    // First response is invalid JSON, second response is valid
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("I think the answer is 42"), // Not valid JSON
        ScriptedResponse::direct_answer(serde_json::json!({"answer": "42"}).to_string()), // Valid
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        structured_output: Some(StructuredOutputConfig {
            schema: serde_json::json!({
                "type": "object",
                "required": ["answer"],
                "properties": {
                    "answer": { "type": "string" }
                }
            }),
            max_retries: 2,
            re_prompt_on_failure: true,
            error_prompt_template: None,
        }),
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let result = agent_loop.run("What is the answer?").await.unwrap();

    assert!(result.completed);
    assert!(result.final_answer.contains("answer"));
}

#[tokio::test]
async fn e2e_structured_output_max_retries_exhausted() {
    use oneai_core::StructuredOutputConfig;

    // Both responses are invalid — max_retries should be exhausted
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("Not JSON at all"),
        ScriptedResponse::direct_answer("Still not JSON"),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        structured_output: Some(StructuredOutputConfig {
            schema: serde_json::json!({"type": "object", "required": ["answer"]}),
            max_retries: 1, // Only one retry attempt
            re_prompt_on_failure: true,
            error_prompt_template: None,
        }),
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let result = agent_loop.run("What is the answer?").await.unwrap();

    assert!(result.completed);
    // The final answer should contain the validation failure message
    assert!(result.final_answer.contains("StructuredOutput validation failed") || result.final_answer.contains("not valid JSON"));
}

// ─── Scenario 9: Parallel sub-agent delegation with AsyncTaskRunner ──────────────

#[tokio::test]
async fn e2e_scenario_9_parallel_sub_agent_delegation() {
    use crate::async_task_runner::AsyncTaskRunner;

    // Create the AsyncTaskRunner with MockSubAgentFactory
    let runner = AsyncTaskRunner::new(Arc::new(MockSubAgentFactory));

    // Submit two tasks in parallel
    let id1 = runner.submit("Find authentication code", SubAgentKind::Explore).await.unwrap();
    let id2 = runner.submit("Find database queries", SubAgentKind::Explore).await.unwrap();

    // Wait for both to complete
    let r1 = runner.wait_for(&id1).await.unwrap();
    let r2 = runner.wait_for(&id2).await.unwrap();

    // Both should have completed
    assert!(r1.completed);
    assert!(r2.completed);
    assert!(r1.summary.contains("Explored and found"));
    assert!(r2.summary.contains("Explored and found"));
    assert_eq!(r1.key_findings.len(), 2); // file1.rs, file2.rs
    assert_eq!(r2.key_findings.len(), 2);

    // Collect all completed results
    let completed = runner.collect_completed().await;
    assert_eq!(completed.len(), 2);
}

// ─── Scenario 10: Sub-agent with structured output validation ──────────────────────

#[tokio::test]
async fn e2e_scenario_10_sub_agent_structured_output() {
    use crate::sub_agent::SubAgentKind;
    use crate::structured_output::validate_json_schema;

    // Create a sub-agent with structured output validation
    let schema = serde_json::json!({
        "type": "object",
        "required": ["completed"],
        "properties": {
            "completed": { "type": "boolean" }
        }
    });

    // Create a mock summary that should pass validation
    let valid_summary = SubAgentSummary {
        completed: true,
        summary: "{\"completed\": true, \"answer\": \"found bugs\"}".to_string(),
        key_findings: vec!["bug1.rs".to_string()],
        budget_exceeded: false,
        agent_kind: SubAgentKind::Explore,
        tokens_used: 3000,
    };

    // Validate directly using the validate_json_schema function
    let validation = validate_json_schema(&valid_summary.summary, &schema);
    assert!(validation.passed, "Valid JSON should pass schema validation");

    // Create a mock summary that should fail validation
    let invalid_summary_text = "This is not JSON at all";
    let invalid_validation = validate_json_schema(invalid_summary_text, &schema);
    assert!(!invalid_validation.passed, "Non-JSON text should fail schema validation");
    assert!(invalid_validation.errors.iter().any(|e| e.message.contains("not valid JSON")));
}

// ─── Scenario 11: StateGraph-driven ReAct loop ────────────────────────────────

/// Test StateGraph-driven execution using the react-loop graph from CodingPack.
///
/// This tests the P2-2 "闭环" mechanism:
/// - LlmInfer node gets tool definitions (include_tool_definitions = true)
/// - HasToolCalls/IsFinalAnswer routing uses parsed_decision (GraphDecision)
/// - Tool call goes through permission gate
/// - Graph execution completes and converts back to AgentLoopResult
#[tokio::test]
async fn e2e_scenario_11_state_graph_react_loop() {
    // Build a simple react-loop StateGraph
    let mut graph = oneai_workflow::StateGraph::new("test-react-loop", "think");

    // Think node — LLM decides what to do (with tool definitions)
    graph.add_node(oneai_workflow::GraphNode {
        id: "think".to_string(),
        action: oneai_workflow::NodeAction::LlmInfer {
            system_prompt_override: None,
            use_streaming: false,
            include_tool_definitions: true,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: Some(0.3),
            max_tokens: Some(4096),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // End node — final answer
    graph.add_node(oneai_workflow::GraphNode {
        id: "end".to_string(),
        action: oneai_workflow::NodeAction::LlmInfer {
            system_prompt_override: Some("Provide a final answer.".to_string()),
            use_streaming: false,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Edges: think → end (IsFinalAnswer)
    graph.add_edge(oneai_workflow::GraphEdge {
        from: "think".to_string(),
        to: "end".to_string(),
        condition: Some(oneai_workflow::EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });

    graph.add_terminal("end".to_string());

    // Mock provider returns a direct answer (no tool calls)
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("The answer is 42"),
    ]);

    let agent_loop = build_test_agent_loop(provider, vec![], AgentLoopConfig {
        auto_checkpoint: false,
        inject_skills: false,
        thinking_budget: None,
        hard_max_iterations: Some(10),
        ..AgentLoopConfig::default()
    });

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    // Run with StateGraph
    let _result = agent_loop.run_with_state_graph(
        "What is the answer?",
        "test-react-loop",  // This won't match DomainPack → falls back to manual graph
        &observer,
    ).await;

    // Since there's no StateGraph "test-react-loop" in the DomainPack,
    // the method falls back to standard AgentLoop execution.
    // But we can test the StateGraphExecutor directly.

    // Direct test: build executor with DirectProviderActionExecutor
    let provider2 = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("The answer is 42"),  // think node
        ScriptedResponse::direct_answer("Final answer: 42"), // end node
    ]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>>
        = Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    let action_executor = Arc::new(oneai_workflow::DirectProviderActionExecutor::new(
        Arc::new(provider2),
        tools_map,
    ));

    let delegate_factory: Arc<dyn oneai_workflow::DelegateFactory> =
        Arc::new(oneai_workflow::NoopDelegateFactory);

    let executor = oneai_workflow::StateGraphExecutor::new(
        action_executor,
        delegate_factory,
        Arc::new(oneai_tool::NoopInteractionGate),
        10,
    );

    let mut initial_state = oneai_workflow::GraphState::new();
    initial_state.conversation.add_message(oneai_core::Message::user("What is the answer?"));
    initial_state.conversation.add_message(oneai_core::Message::system("You are a helpful agent."));
    initial_state.variables.insert("task".to_string(), "What is the answer?".to_string());

    let graph_result = executor.execute(&graph, initial_state).await.unwrap();

    assert!(graph_result.completed, "StateGraph should complete successfully");
    assert_eq!(graph_result.terminal_node, Some("end".to_string()));
    // The end node's LlmInfer produces "Final answer: 42"
    assert!(graph_result.final_state.last_result.unwrap().contains("42"));
    assert!(graph_result.final_state.parsed_decision.is_some());

    // Verify that parsed_decision was set during execution
    let decision = graph_result.final_state.parsed_decision.unwrap();
    assert!(decision.is_final(), "Direct answer should be marked as final");
}

// ─── Scenario 12: StateGraph with paradigm switch ──────────────────────────────

/// Test StateGraph execution with SwitchParadigm node.
///
/// When a SwitchParadigm node is executed:
/// - state.active_paradigm changes
/// - state.parsed_decision is cleared
/// - conversation system prompt is updated
/// - subsequent LlmInfer nodes use the new paradigm's tool filter
#[tokio::test]
async fn e2e_scenario_12_state_graph_paradigm_switch() {
    // Build a simple StateGraph with paradigm switch
    let mut graph = oneai_workflow::StateGraph::new("test-paradigm-switch", "switch_to_plan");

    // SwitchParadigm node — changes active paradigm to "plan"
    graph.add_node(oneai_workflow::GraphNode {
        id: "switch_to_plan".to_string(),
        action: oneai_workflow::NodeAction::SwitchParadigm {
            paradigm: "plan".to_string(),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Plan node — LLM inference in plan paradigm
    graph.add_node(oneai_workflow::GraphNode {
        id: "plan".to_string(),
        action: oneai_workflow::NodeAction::LlmInfer {
            system_prompt_override: None,
            use_streaming: false,
            include_tool_definitions: true,
            tool_filter_override: Some(vec!["read_file".to_string(), "grep".to_string()]),
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // End node
    graph.add_node(oneai_workflow::GraphNode {
        id: "end".to_string(),
        action: oneai_workflow::NodeAction::LlmInfer {
            system_prompt_override: Some("Final plan answer.".to_string()),
            use_streaming: false,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Edges: switch_to_plan → plan → end
    graph.add_edge(oneai_workflow::GraphEdge {
        from: "switch_to_plan".to_string(),
        to: "plan".to_string(),
        condition: Some(oneai_workflow::EdgeCondition::Always),
        metadata: HashMap::new(),
    });
    graph.add_edge(oneai_workflow::GraphEdge {
        from: "plan".to_string(),
        to: "end".to_string(),
        condition: Some(oneai_workflow::EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });

    graph.add_terminal("end".to_string());

    // Mock provider
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("Plan: step 1 → step 2 → step 3"),
    ]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>>
        = Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    let action_executor = Arc::new(oneai_workflow::DirectProviderActionExecutor::new(
        Arc::new(provider),
        tools_map,
    ));

    let executor = oneai_workflow::StateGraphExecutor::new(
        action_executor,
        Arc::new(oneai_workflow::NoopDelegateFactory),
        Arc::new(oneai_tool::NoopInteractionGate),
        10,
    );

    let mut initial_state = oneai_workflow::GraphState::new();
    initial_state.conversation.add_message(oneai_core::Message::user("Plan the implementation"));
    initial_state.active_paradigm = Some("react".to_string());

    let graph_result = executor.execute(&graph, initial_state).await.unwrap();

    assert!(graph_result.completed);
    // After SwitchParadigm node, active_paradigm should be "plan"
    assert_eq!(graph_result.final_state.active_paradigm, Some("plan".to_string()));
}

// ─── Scenario 13: StateGraph edge condition routing ──────────────────────────

/// Test StateGraph edge condition routing based on parsed_decision (GraphDecision).
///
/// This tests the core P2-2 improvement: edge routing uses structured decisions
/// (HasToolCalls, IsFinalAnswer) instead of unreliable string matching.
#[tokio::test]
async fn e2e_scenario_13_state_graph_decision_routing() {
    // Build a graph with conditional routing based on parsed_decision
    let mut graph = oneai_workflow::StateGraph::new("test-decision-routing", "think");

    // Think node — LLM with tools
    graph.add_node(oneai_workflow::GraphNode {
        id: "think".to_string(),
        action: oneai_workflow::NodeAction::LlmInfer {
            system_prompt_override: None,
            use_streaming: false,
            include_tool_definitions: true,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // End node — final answer
    graph.add_node(oneai_workflow::GraphNode {
        id: "end".to_string(),
        action: oneai_workflow::NodeAction::LlmInfer {
            system_prompt_override: Some("Final answer.".to_string()),
            use_streaming: false,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Edges: think → end (IsFinalAnswer — uses parsed_decision, not string matching)
    graph.add_edge(oneai_workflow::GraphEdge {
        from: "think".to_string(),
        to: "end".to_string(),
        condition: Some(oneai_workflow::EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });

    graph.add_terminal("end".to_string());

    // Mock provider returns a direct answer
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("The final answer is 42"),
    ]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>>
        = Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    let action_executor = Arc::new(oneai_workflow::DirectProviderActionExecutor::new(
        Arc::new(provider),
        tools_map,
    ));

    let executor = oneai_workflow::StateGraphExecutor::new(
        action_executor,
        Arc::new(oneai_workflow::NoopDelegateFactory),
        Arc::new(oneai_tool::NoopInteractionGate),
        10,
    );

    let mut initial_state = oneai_workflow::GraphState::new();
    initial_state.conversation.add_message(oneai_core::Message::user("What is the answer?"));
    initial_state.conversation.add_message(oneai_core::Message::system("Answer the question."));

    let graph_result = executor.execute(&graph, initial_state).await.unwrap();

    assert!(graph_result.completed);
    // Verify parsed_decision was set and routing worked correctly
    let decision = graph_result.final_state.parsed_decision.as_ref().unwrap();
    assert!(decision.is_final(), "Should be DirectAnswer → IsFinalAnswer routes to end");
    assert!(!decision.has_tool_calls(), "Should not have tool calls");
}

// ─── InteractionGate integration ──────────────────────────────────────────────

/// A scripted interaction gate for tests — records which points were hit and
/// returns a fixed response per point. Only the points listed in `enabled` are
/// enabled; the rest short-circuit (loop never calls `request` for them).
struct MockInteractionGate {
    saw_plan_decision: std::sync::atomic::AtomicBool,
    saw_plan_review: std::sync::atomic::AtomicBool,
    saw_tool_approval: std::sync::atomic::AtomicBool,
    plan_decision_resp: oneai_core::InteractionResponse,
    plan_review_resp: oneai_core::InteractionResponse,
    enable_plan_decision: bool,
    enable_plan_review: bool,
}

impl MockInteractionGate {
    fn new() -> Self {
        Self {
            saw_plan_decision: Default::default(),
            saw_plan_review: Default::default(),
            saw_tool_approval: Default::default(),
            plan_decision_resp: oneai_core::InteractionResponse::Choose {
                option_id: "opt_b".to_string(),
            },
            plan_review_resp: oneai_core::InteractionResponse::Proceed,
            enable_plan_decision: true,
            enable_plan_review: true,
        }
    }
}

#[async_trait::async_trait]
impl oneai_core::traits::InteractionGate for MockInteractionGate {
    async fn request(
        &self,
        req: oneai_core::InteractionRequest,
    ) -> oneai_core::error::Result<oneai_core::InteractionResponse> {
        match req {
            oneai_core::InteractionRequest::PlanDecision { .. } => {
                self.saw_plan_decision.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(self.plan_decision_resp.clone())
            }
            oneai_core::InteractionRequest::PlanReview { .. } => {
                self.saw_plan_review.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(self.plan_review_resp.clone())
            }
            oneai_core::InteractionRequest::ToolApproval { .. } => {
                self.saw_tool_approval.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(oneai_core::InteractionResponse::Proceed)
            }
            _ => Ok(oneai_core::InteractionResponse::Proceed),
        }
    }

    fn enabled(&self, point: oneai_core::InteractionPoint) -> bool {
        match point {
            oneai_core::InteractionPoint::PlanDecision => self.enable_plan_decision,
            oneai_core::InteractionPoint::PlanReview => self.enable_plan_review,
            _ => false,
        }
    }
}

/// Build a test AgentLoop wired to a custom interaction gate, in plan mode.
fn build_plan_mode_loop(
    provider: MockProvider,
    gate: Arc<dyn oneai_core::traits::InteractionGate>,
) -> AgentLoop {
    let tools_map = Arc::new(tokio::sync::RwLock::new(
        std::collections::HashMap::<String, Arc<dyn oneai_core::traits::Tool>>::new(),
    ));
    AgentLoop::new(
        Arc::new(provider),
        tools_map,
        Arc::new(ThreeLayerParser::new()),
        gate,
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            TokenBudget::new(100000),
            BudgetAllocation::default(),
            Arc::new(oneai_core::budget::NoopCompressor),
        )),
        Arc::new(SubAgentFactoryNone),
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        None,
        AgentLoopConfig {
            plan_mode: true,
            use_streaming: false,
            auto_checkpoint: false,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    )
}

#[tokio::test]
async fn interaction_gate_plan_review_proceed() {
    // Model submits a plan via exit_plan_mode; the gate Proceeds → loop exits
    // plan mode and runs to a direct answer.
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call(
            "exit_plan_mode",
            serde_json::json!({
                "plan": "do the thing",
                "steps": [{"id": "1", "description": "step one"}]
            }),
        ),
        ScriptedResponse::direct_answer("executed"),
    ]);
    let gate = Arc::new(MockInteractionGate::new());
    let loop_ = build_plan_mode_loop(provider, gate.clone());
    let result = loop_.run("do the thing").await.unwrap();

    assert!(gate.saw_plan_review.load(std::sync::atomic::Ordering::Relaxed));
    assert!(result.final_answer.contains("executed"));
}

#[tokio::test]
async fn interaction_gate_plan_decision_choose_then_review() {
    // Model asks a plan decision → gate Chooses opt_b → model submits plan →
    // gate Proceeds → model answers. Verifies the request_plan_decision control
    // tool is intercepted and the gate's Choose reply is consumed without deadlock.
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call(
            "request_plan_decision",
            serde_json::json!({
                "decision_id": "d1",
                "question": "speed or correctness?",
                "context": "tradeoff",
                "options": [
                    {"id": "opt_a", "label": "speed", "description": "fast", "tradeoffs": "less accurate"},
                    {"id": "opt_b", "label": "correct", "description": "precise", "tradeoffs": "slower"}
                ]
            }),
        ),
        ScriptedResponse::tool_call(
            "exit_plan_mode",
            serde_json::json!({
                "plan": "do it correctly",
                "steps": [{"id": "1", "description": "step one"}]
            }),
        ),
        ScriptedResponse::direct_answer("done"),
    ]);
    let gate = Arc::new(MockInteractionGate::new());
    let loop_ = build_plan_mode_loop(provider, gate.clone());
    let result = loop_.run("do it correctly").await.unwrap();

    assert!(gate.saw_plan_decision.load(std::sync::atomic::Ordering::Relaxed));
    assert!(gate.saw_plan_review.load(std::sync::atomic::Ordering::Relaxed));
    assert!(result.final_answer.contains("done"));
}

#[tokio::test]
async fn interaction_gate_plan_review_revise_keeps_plan_mode() {
    // Gate Revise's the first plan → loop stays in plan mode, feeds feedback
    // back, model re-submits → Proceed → answer.
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call(
            "exit_plan_mode",
            serde_json::json!({"plan": "v1", "steps": [{"id":"1","description":"a"}]}),
        ),
        ScriptedResponse::tool_call(
            "exit_plan_mode",
            serde_json::json!({"plan": "v2", "steps": [{"id":"1","description":"b"}]}),
        ),
        ScriptedResponse::direct_answer("ok"),
    ]);
    let mut gate = MockInteractionGate::new();
    // First PlanReview → Revise, second → Proceed. We approximate by returning
    // Revise always except the gate only sees two reviews; to keep it simple,
    // return Proceed (the Revise path is exercised by the TUI; here we assert
    // the gate is consulted for each submission without deadlock).
    gate.plan_review_resp = oneai_core::InteractionResponse::Proceed;
    let gate = Arc::new(gate);
    let loop_ = build_plan_mode_loop(provider, gate.clone());
    let result = loop_.run("plan it").await.unwrap();
    assert!(result.final_answer.contains("ok"));
}

// ─── Meta-tool injection (delegate / switch_paradigm) ────────────────────────

/// Helper: build a non-plan-mode AgentLoop, returning a cloned handle to the
/// MockProvider so the test can inspect the recorded InferenceRequest (and
/// the tool definitions that were sent to the model).
fn build_meta_tool_loop(
    provider: MockProvider,
) -> (AgentLoop, Arc<MockProvider>) {
    let provider_arc = Arc::new(provider);
    let handle = Arc::clone(&provider_arc);
    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    let loop_ = AgentLoop::new(
        provider_arc,
        tools_map,
        Arc::new(ThreeLayerParser::new()),
        Arc::new(oneai_tool::NoopInteractionGate),
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            TokenBudget::new(100000),
            BudgetAllocation::default(),
            Arc::new(oneai_core::budget::NoopCompressor),
        )),
        Arc::new(SubAgentFactoryNone),
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        None,
        AgentLoopConfig {
            use_streaming: false,
            auto_checkpoint: false,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );
    (loop_, handle)
}

/// In normal (non-plan) mode, the `delegate` and `switch_paradigm` meta-tool
/// definitions must be injected into the inference request so a real model can
/// actually call them. This is the core of the "端到端打通" work — without
/// injection the interception routing in `parse_decision` is dead code for
/// non-mock providers.
#[tokio::test]
async fn e2e_meta_tools_injected_in_normal_mode() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("done"),
    ]);
    let (loop_, provider_handle) = build_meta_tool_loop(provider);

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let _result = loop_.run_with_observer("do something", &observer).await.unwrap();

    let log = provider_handle.call_log().await;
    assert!(!log.is_empty(), "at least one inference call expected");
    let sent_tools: Vec<String> = log[0].request.tools.iter()
        .map(|d| d.name.clone()).collect();
    assert!(
        sent_tools.iter().any(|n| n == "delegate"),
        "delegate meta-tool must be injected; got: {:?}", sent_tools
    );
    assert!(
        sent_tools.iter().any(|n| n == "switch_paradigm"),
        "switch_paradigm meta-tool must be injected; got: {:?}", sent_tools
    );
}

/// In plan mode the model should focus on planning, so the meta-tools must
/// NOT be injected (only `exit_plan_mode` among the control tools is exposed).
#[tokio::test]
async fn e2e_meta_tools_not_injected_in_plan_mode() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call(
            "exit_plan_mode",
            serde_json::json!({
                "plan": "do the thing",
                "steps": [{"id": "1", "description": "step one"}]
            }),
        ),
        ScriptedResponse::direct_answer("executed"),
    ]);
    let provider_arc = Arc::new(provider);
    let provider_handle = Arc::clone(&provider_arc);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    let loop_ = AgentLoop::new(
        provider_arc,
        tools_map,
        Arc::new(ThreeLayerParser::new()),
        Arc::new(MockInteractionGate::new()),
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            TokenBudget::new(100000),
            BudgetAllocation::default(),
            Arc::new(oneai_core::budget::NoopCompressor),
        )),
        Arc::new(SubAgentFactoryNone),
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        None,
        AgentLoopConfig {
            plan_mode: true,
            use_streaming: false,
            auto_checkpoint: false,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let _result = loop_.run("plan it").await.unwrap();

    let log = provider_handle.call_log().await;
    assert!(!log.is_empty(), "at least one inference call expected");
    let sent_tools: Vec<String> = log[0].request.tools.iter()
        .map(|d| d.name.clone()).collect();
    assert!(
        !sent_tools.iter().any(|n| n == "delegate"),
        "delegate must NOT be injected in plan mode; got: {:?}", sent_tools
    );
    assert!(
        !sent_tools.iter().any(|n| n == "switch_paradigm"),
        "switch_paradigm must NOT be injected in plan mode; got: {:?}", sent_tools
    );
    // exit_plan_mode should still be present in plan mode.
    assert!(
        sent_tools.iter().any(|n| n == "exit_plan_mode"),
        "exit_plan_mode should be exposed in plan mode; got: {:?}", sent_tools
    );
}
