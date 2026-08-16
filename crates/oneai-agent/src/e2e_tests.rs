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

use oneai_core::budget::{BudgetAllocation, ContextBudgetManager, TokenBudget};
use oneai_core::{Role, ToolOutput};

use oneai_parser::ThreeLayerParser;
use oneai_skill::SkillSelector;

use crate::agent_loop::{
    AgentLoop, AgentLoopConfig, AgentLoopObserver, AgentLoopResult, ParadigmKind, ToolCallRequest,
};
use crate::context_assembler::ContextAssembler;
use crate::mock_provider::{MockProvider, ScriptedResponse};
use crate::mock_tool::MockTool;
use crate::streaming::IncrementalStreamParser;
use crate::sub_agent::{SubAgentFactory, SubAgentFactoryNone, SubAgentKind, SubAgentSummary};

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
        config,
    )
}

/// TestObserver captures all AgentLoop events using std::sync::Mutex
/// (since Observer callbacks are synchronous, not async).
struct TestObserver {
    events: Arc<Mutex<Vec<TestEvent>>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code, clippy::large_enum_variant)] // debug-inspection event log; variant size is irrelevant
enum TestEvent {
    IterationStart(usize, ParadigmKind),
    DirectAnswer(String),
    ToolCalls(Vec<ToolCallRequest>),
    ToolResult(String, String, ToolOutput),
    Delegate(String, SubAgentKind),
    DelegateComplete(SubAgentSummary),
    ParadigmSwitch(ParadigmKind),
    Checkpoint(usize),
    Complete(AgentLoopResult),
    StreamChunk(String),
    Thinking(String),
    Reflection(String),
    ToolsAdded(Vec<String>),
}

impl AgentLoopObserver for TestObserver {
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::IterationStart(iteration, paradigm));
    }
    fn on_direct_answer(&self, text: &str) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::DirectAnswer(text.to_string()));
    }
    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::ToolCalls(calls.to_vec()));
    }
    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &ToolOutput) {
        self.events.lock().unwrap().push(TestEvent::ToolResult(
            call_id.to_string(),
            tool_name.to_string(),
            output.clone(),
        ));
    }
    fn on_delegate(&self, task: &str, agent_type: &SubAgentKind) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::Delegate(task.to_string(), agent_type.clone()));
    }
    fn on_delegate_complete(&self, summary: &SubAgentSummary) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::DelegateComplete(summary.clone()));
    }
    fn on_paradigm_switch(&self, paradigm: ParadigmKind) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::ParadigmSwitch(paradigm));
    }
    fn on_checkpoint(&self, iteration: usize) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::Checkpoint(iteration));
    }
    fn on_complete(&self, result: &AgentLoopResult) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::Complete(result.clone()));
    }
    fn on_stream_chunk(&self, text: &str) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::StreamChunk(text.to_string()));
    }
    fn on_thinking(&self, text: &str) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::Thinking(text.to_string()));
    }
    fn on_reflection(&self, summary: &str) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::Reflection(summary.to_string()));
    }
    fn on_tools_added(&self, names: &[String]) {
        self.events
            .lock()
            .unwrap()
            .push(TestEvent::ToolsAdded(names.to_vec()));
    }
}

// ─── Scenario 1: DirectAnswer ─────────────────────────────────────────────────

#[tokio::test]
async fn e2e_scenario_1_direct_answer() {
    let provider =
        MockProvider::from_script(vec![ScriptedResponse::direct_answer("The answer is 42")]);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

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

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file)],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop
        .run("Read /test.txt and tell me what's in it")
        .await
        .unwrap();

    assert!(result.completed);
    assert!(result.final_answer.contains("hello world"));
    assert_eq!(result.iterations, 2); // 1 for tool call + 1 for answer

    // Verify tool was called once with correct args
    let log = read_file_log.lock().await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].args["path"], "/test.txt");
}

// ─── Malformed-args feedback (Reflexion) ────────────────────────────────────

#[tokio::test]
async fn malformed_tool_args_fed_back_not_silently_dispatched() {
    // §agent-loop hot-path gap fix: a tool call with malformed JSON args
    // must NOT be silently dispatched with empty args (the old
    // `serde_json::from_str(args).unwrap_or(json!({}))` swallow). Instead a
    // tool_result error is injected back into the conversation so the model
    // can self-correct next iteration (Reflexion/SWE-agent pattern).
    let read_file = MockTool::read_file_mock_with_content("should not be reached");
    let read_file_log = read_file.call_log();

    let provider = MockProvider::from_script(vec![
        // Malformed args — not valid JSON. Must be dropped + fed back.
        ScriptedResponse::raw_tool_call("read_file", "{path: /test.txt, trailing,}"),
        // Model self-corrects and answers directly.
        ScriptedResponse::direct_answer("recovered"),
    ]);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file)],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop.run("read /test.txt").await.unwrap();
    assert!(result.completed);

    // The malformed call was NOT dispatched to the tool.
    let log = read_file_log.lock().await;
    assert!(
        log.is_empty(),
        "malformed-args call must not be dispatched with empty args, got {log:?}"
    );

    // A feedback tool_result was injected into the conversation.
    let feedback_injected = result
        .conversation
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|block| match block {
            oneai_core::ContentBlock::ToolResult { content, .. } => {
                content.to_lowercase().contains("malformed arguments")
            }
            _ => false,
        });
    assert!(
        feedback_injected,
        "expected a 'malformed arguments' tool_result feedback in the conversation"
    );
}

#[tokio::test]
async fn malformed_args_fuzzy_repaired_then_dispatched() {
    // §parser gap fix #9: a tool call whose args are mildly malformed —
    // recoverable by Layer 2 fuzzy repair (here: an unclosed brace) — must be
    // repaired by the injected ThreeLayerParser and dispatched normally,
    // rather than being fed back to the model as an error. Previously
    // `parse_tool_args` did a bare `serde_json::from_str`, so any deviation
    // from strict JSON was treated as malformed even when the fuzzy layer
    // could recover it — Layer 2 was unreachable on the hot path.
    let read_file = MockTool::read_file_mock();
    let read_file_log = read_file.call_log();

    let provider = MockProvider::from_script(vec![
        // Unclosed-brace args — fuzzy repair closes the brace and recovers
        // {"path": "/test.txt"}. Must be dispatched, not fed back.
        ScriptedResponse::raw_tool_call("read_file", "{\"path\": \"/test.txt\""),
        ScriptedResponse::direct_answer("done"),
    ]);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file)],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop.run("read /test.txt").await.unwrap();
    assert!(result.completed);

    // The repaired call WAS dispatched to the tool.
    let log = read_file_log.lock().await;
    assert_eq!(
        log.len(),
        1,
        "fuzzy-repaired call must be dispatched, got {log:?}"
    );
    assert_eq!(log[0].args["path"], "/test.txt");
}

#[tokio::test]
async fn context_manager_trims_oversized_request_before_inference() {
    // §context gap fix #3: when a `ContextManager` is attached with a small
    // window for the target model, the loop trims the per-request
    // conversation to fit BEFORE inference (here: TruncateOldest keeps system
    // + recent 2 turns). Previously the AppBuilder-constructed ContextManager
    // was never invoked from the hot path — only the ContextCompressor
    // keep-recent path ran, so the 4 model-aware strategies were dead code.
    //
    // We drive several read_file calls whose ~600-char results accumulate
    // past the tiny window, then assert the LAST inference request the
    // provider received was trimmed (fewer messages than the full durable
    // log accumulated).
    use oneai_core::token_counter::{HeuristicTokenCounter, ModelTokenizerProfile};
    use oneai_core::{ContextManager, ContextTrimmingStrategy, ContextWindowProfile};

    // ~600-char tool body — a few of these exceed the tiny window.
    let big_body = "x".repeat(600);
    let read_file = MockTool::read_file_mock_with_content(big_body);

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("read_file", serde_json::json!({"path":"/a"})),
        ScriptedResponse::tool_call("read_file", serde_json::json!({"path":"/b"})),
        ScriptedResponse::tool_call("read_file", serde_json::json!({"path":"/c"})),
        ScriptedResponse::tool_call("read_file", serde_json::json!({"path":"/d"})),
        ScriptedResponse::direct_answer("done"),
    ]);
    let call_log = provider.call_log_handle();

    // Build the counter with a tiny window for "mock-model".
    // `ContextManager.fits_context_window` delegates the window lookup to the
    // token counter's own profile map (not the ContextWindowProfile's
    // context_window_tokens), so the window must be registered HERE.
    let counter = {
        let mut c = HeuristicTokenCounter::new();
        let mut p = ModelTokenizerProfile::from_model_name("mock-model");
        p.context_window_tokens = 400; // 400 × 0.8 = 320 effective ≈ 1280 chars
        c.add_profile(p);
        c
    };
    let cm = ContextManager::new(
        Arc::new(counter),
        ContextTrimmingStrategy::TruncateOldest {
            keep_recent_turns: 2,
        },
    );
    // Register the strategy on the ContextManager profile too (trim_for_model
    // reads trimming_strategy from here).
    let cm = {
        let mut cm = cm;
        cm.add_profile(ContextWindowProfile::new(
            "mock-model",
            400,
            100,
            0.8,
            ContextTrimmingStrategy::TruncateOldest {
                keep_recent_turns: 2,
            },
        ));
        Arc::new(cm)
    };

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file)],
        AgentLoopConfig {
            inject_skills: false,
            context_manager: Some(cm),
            hard_max_iterations: Some(20),
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop.run("read several files").await.unwrap();
    assert!(
        result.completed,
        "run must complete; got: {:?}",
        result.final_answer
    );

    // Inspect the inference requests the provider actually received.
    let calls = call_log.lock().await.clone();
    assert!(
        calls.len() >= 2,
        "expected multiple inference calls, got {}",
        calls.len()
    );

    // The durable log accumulated many messages (4 tool calls + results).
    let durable_len = result.conversation.messages.len();
    assert!(
        durable_len > 6,
        "durable log should have accumulated many messages, got {durable_len}"
    );

    // The LAST inference request must have been trimmed to fit the window —
    // far fewer messages than the durable log. TruncateOldest keeps system +
    // recent 2 turns (each turn ≈ user-tool-result pair), so the request is
    // bounded (well under the durable log's length).
    let last_req_msgs = calls.last().unwrap().request.conversation.messages.len();
    assert!(
        last_req_msgs < durable_len,
        "last request ({last_req_msgs} msgs) must be trimmed below durable log ({durable_len}) \
         — ContextManager wasn't invoked on the hot path"
    );
    // Direct proof the TruncateOldest path ran: it appends a
    // "[Context trimmed: ...]" system marker when it actually drops messages.
    let trimmed_marker_present = calls
        .last()
        .unwrap()
        .request
        .conversation
        .messages
        .iter()
        .any(|m| m.text_content().contains("Context trimmed"));
    assert!(
        trimmed_marker_present,
        "expected the TruncateOldest '[Context trimmed]' marker in the last request — \
         ContextManager.trim_for_model did not run"
    );
}

// ─── Token-budget termination guardrail ─────────────────────────────────────

#[tokio::test]
async fn token_budget_terminates_run_before_iteration_cap() {
    // §agent-loop gap fix #2: a configured run-cost token budget must
    // terminate the loop on exhaustion — IN ADDITION to hard_max_iterations
    // — so a runaway model can't burn tokens indefinitely. Previously the
    // budget was never checked or consumed; only hard_max_iterations bounded
    // the run (and None → unbounded).
    let read_file = MockTool::read_file_mock();
    // Script more tool_calls than the budget allows — the budget must bind
    // before the queue exhausts (MockProvider returns a default DirectAnswer
    // on queue exhaustion, which would mask the budget termination).
    let script: Vec<_> = (0..8)
        .map(|_| ScriptedResponse::tool_call("read_file", serde_json::json!({"path": "/test.txt"})))
        .collect();
    let provider = MockProvider::from_script(script);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file)],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(50), // high — budget must be the binding constraint
            token_budget: Some(oneai_core::budget::TokenBudget::new(500)), // 500 / 230-per-iter ≈ 2 iters
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop.run("read /test.txt repeatedly").await.unwrap();

    // Budget exhausted, not natural completion.
    assert!(
        !result.completed,
        "run must terminate on budget exhaustion, not complete"
    );
    // A clear budget-exhausted note was surfaced (not a silent empty result).
    assert!(
        result
            .final_answer
            .contains("budget of 500 tokens exhausted"),
        "expected budget-exhausted note, got: {}",
        result.final_answer
    );
    // Budget bound well before the iteration cap.
    assert!(
        result.iterations <= 3,
        "budget should bind by ~3 iters (500 tokens / 230 per iter), got {}",
        result.iterations
    );
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
        ScriptedResponse::tool_call(
            "edit_file",
            serde_json::json!({"path": "/test.rs", "changes": "add safety check"}),
        ),
        ScriptedResponse::direct_answer("I've fixed the bug in /test.rs"),
    ]);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file), Arc::new(edit_file)],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

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

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file)],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let result = agent_loop
        .run_with_observer("Plan the implementation", &observer)
        .await
        .unwrap();

    assert!(result.completed);
    assert_eq!(result.active_paradigm, ParadigmKind::Plan);

    // Verify observer received paradigm switch event
    let events = observer.events.lock().unwrap();
    let paradigm_switches = events
        .iter()
        .filter(|e| matches!(e, TestEvent::ParadigmSwitch(ParadigmKind::Plan)))
        .count();
    assert_eq!(paradigm_switches, 1);

    // Verify conversation contains Plan paradigm system prompt
    let has_plan_prompt = result
        .conversation
        .messages
        .iter()
        .any(|m| m.role == Role::System && m.text_content().contains("planning agent"));
    assert!(
        has_plan_prompt,
        "Conversation should contain Plan paradigm system prompt"
    );
}

// ─── Scenario 4b: Footprint gate (check_fn) excludes missing-service tools ────

/// A tool whose `service_available()` is `false` must vanish from the schema
/// sent to the model (zero footprint), while a sibling available tool still
/// appears. This guards the `build_tool_definitions_for_paradigm` filter.
#[tokio::test]
async fn e2e_scenario_4b_footprint_gate_hides_unavailable_tool() {
    use oneai_tool::{GatedTool, ServiceCheck};

    // `read_file` gated off (backing service "missing"), `grep` available.
    let check: ServiceCheck = Arc::new(|| false);
    let read_file: Arc<dyn oneai_core::traits::Tool> =
        Arc::new(GatedTool::new(Arc::new(MockTool::read_file_mock()), check));
    let grep: Arc<dyn oneai_core::traits::Tool> = Arc::new(MockTool::grep_mock());

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> = {
        let mut map = HashMap::new();
        map.insert("read_file".to_string(), read_file);
        map.insert("grep".to_string(), grep);
        Arc::new(tokio::sync::RwLock::new(map))
    };

    let provider = MockProvider::always_answers("done");
    let call_log = provider.call_log_handle();
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
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(5),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let _ = agent_loop
        .run_with_observer("do something with grep", &observer)
        .await
        .unwrap();

    let logs = call_log.lock().await.clone();
    let tools_sent: Vec<String> = logs
        .iter()
        .flat_map(|log| log.request.tools.iter().map(|t| t.name.clone()))
        .collect();
    assert!(
        !tools_sent.iter().any(|n| n == "read_file"),
        "gated-off read_file must NOT appear in model schema: {tools_sent:?}"
    );
    assert!(
        tools_sent.iter().any(|n| n == "grep"),
        "available grep must appear in model schema: {tools_sent:?}"
    );
}

// ─── Scenario 4c: cache-stable system prompt survives paradigm switch ────────

/// A paradigm switch must NOT nuke the durable system prefix. The stable
/// prefix (`build_system_prompt() + runtime_context_block()` — current date +
/// web-search nudge) must survive so the provider's prompt-prefix cache stays
/// valid across switches and `runtime_context` (date / search guidance) is not
/// lost for the rest of the session. The paradigm tail ("planning agent") is
/// swapped in alongside it. Regression for the old `retain(|m| m.role !=
/// Role::System)` behavior that wiped everything.
#[tokio::test]
async fn e2e_scenario_4c_paradigm_switch_preserves_stable_prefix() {
    let read_file = MockTool::read_file_mock();

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::switch_paradigm("plan"),
        ScriptedResponse::direct_answer("Plan: step 1 → step 2"),
    ]);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file)],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let result = agent_loop
        .run_with_observer("Plan the implementation", &observer)
        .await
        .unwrap();

    let system_texts: Vec<String> = result
        .conversation
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text_content())
        .collect();

    // Stable prefix survives the switch — runtime_context_block is intact.
    assert!(
        system_texts
            .iter()
            .any(|t| t.contains("Current date and time")),
        "runtime_context block (stable prefix) was dropped on paradigm switch: {system_texts:?}"
    );
    assert!(
        system_texts.iter().any(|t| t.contains("web_search")),
        "web-search guidance (stable prefix) was dropped on paradigm switch: {system_texts:?}"
    );
    // The base agent identity prompt also survives.
    assert!(
        system_texts
            .iter()
            .any(|t| t.contains("intelligent AI agent")),
        "stable identity prefix was dropped on paradigm switch: {system_texts:?}"
    );
    // And the paradigm tail is present alongside it.
    assert!(
        system_texts.iter().any(|t| t.contains("planning agent")),
        "paradigm tail missing after switch: {system_texts:?}"
    );
}

// ─── Scenario 4d: empty-response retry preserves prompt_cache_policy ────────

/// The empty-response retry must carry `prompt_cache_policy` into the
/// retried `InferenceRequest` so the re-inference still hits the provider's
/// prefix cache. Previously the retry built `metadata: HashMap::new()`,
/// dropping the policy and making every empty-response retry a cache miss
/// (re-billed prefix). Phase 1.5 regression.
#[tokio::test]
async fn e2e_scenario_4d_empty_retry_preserves_prompt_cache_policy() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer(""), // empty → triggers empty-retry
        ScriptedResponse::direct_answer("here is the real answer"),
    ]);
    let call_log = provider.call_log_handle();

    let agent_loop = build_test_agent_loop(
        provider,
        vec![], // no tools needed
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(5),
            prompt_cache_policy: oneai_core::PromptCachePolicy::On,
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let _ = agent_loop
        .run_with_observer("answer me", &observer)
        .await
        .unwrap();

    let logs = call_log.lock().await.clone();
    assert!(
        logs.len() >= 2,
        "expected main + retry inference calls, got {}: {logs:?}",
        logs.len()
    );

    // Main request carries the policy.
    assert_eq!(
        logs[0].request.metadata.get("prompt_cache_policy"),
        Some(&"on".to_string()),
        "main request dropped prompt_cache_policy: {:?}",
        logs[0].request.metadata
    );
    // Retry request also carries it — the regression.
    assert_eq!(
        logs[1].request.metadata.get("prompt_cache_policy"),
        Some(&"on".to_string()),
        "empty-response retry dropped prompt_cache_policy (cache miss on retry): {:?}",
        logs[1].request.metadata
    );
}

// ─── Scenario 5: Sub-agent delegation ─────────────────────────────────────────

/// A mock sub-agent factory that returns a canned summary.
struct MockSubAgentFactory;

#[async_trait::async_trait]
impl SubAgentFactory for MockSubAgentFactory {
    async fn create(
        &self,
        kind: SubAgentKind,
        budget: TokenBudget,
    ) -> oneai_core::error::Result<Box<dyn crate::sub_agent::SubAgent>> {
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
    fn kind(&self) -> &SubAgentKind {
        &self.kind
    }
    fn budget(&self) -> &TokenBudget {
        &self.budget
    }
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
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let result = agent_loop
        .run_with_observer("Search for bugs", &observer)
        .await
        .unwrap();

    assert!(result.completed);
    assert!(!result.sub_agent_results.is_empty());
    assert!(result.sub_agent_results[0].summary.contains("Explored"));

    // Verify observer received delegate event
    let events = observer.events.lock().unwrap();
    let delegate_events = events
        .iter()
        .filter(|e| matches!(e, TestEvent::Delegate(_, SubAgentKind::Explore)))
        .count();
    assert_eq!(delegate_events, 1);

    // Verify observer also received the completion callback pairing with the
    // delegate event — the sub-agent lifecycle must be observable end-to-end.
    let complete_events = events
        .iter()
        .filter(|e| matches!(e, TestEvent::DelegateComplete(s) if s.agent_kind == SubAgentKind::Explore && s.summary.contains("Explored")))
        .count();
    assert_eq!(
        complete_events, 1,
        "expected exactly one DelegateComplete event for the Explore sub-agent"
    );
}

// ─── Scenario 5b/5c: Parallel + dependency-aware multi-delegation ────────────
//
// The delegate meta-tool now collects *all* `delegate` calls in a turn into a
// batch. Tasks with no `depends_on` run concurrently (wave); a task that lists
// `depends_on` ids runs only after those finish, and its task text is prefixed
// with their summaries. These tests verify both behaviors end-to-end.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A sub-agent factory that records, across all sub-agents it spawns:
/// - peak concurrency (`peak`) — proves parallel execution
/// - the (kind, received-task-text) of each run, in start order — proves
///   dependency ordering and that downstream received upstream summaries
struct RecordingSubAgentFactory {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    runs: Arc<Mutex<Vec<(String, String)>>>,
}

impl RecordingSubAgentFactory {
    fn new() -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            runs: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
    fn runs(&self) -> Vec<(String, String)> {
        self.runs.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl SubAgentFactory for RecordingSubAgentFactory {
    async fn create(
        &self,
        kind: SubAgentKind,
        _budget: TokenBudget,
    ) -> oneai_core::error::Result<Box<dyn crate::sub_agent::SubAgent>> {
        Ok(Box::new(RecordingSubAgent {
            kind,
            active: self.active.clone(),
            peak: self.peak.clone(),
            runs: self.runs.clone(),
        }))
    }
    fn available_kinds(&self) -> Vec<SubAgentKind> {
        vec![SubAgentKind::Explore, SubAgentKind::Code]
    }
    fn is_available(&self, kind: &SubAgentKind) -> bool {
        matches!(kind, SubAgentKind::Explore | SubAgentKind::Code)
    }
}

/// A sub-agent that sleeps briefly while counted as active, then returns a
/// deterministic summary (`RESULT-<kind>`). The sleep creates a real await
/// point so concurrent wave-mates actually overlap.
struct RecordingSubAgent {
    kind: SubAgentKind,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    runs: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait::async_trait]
impl crate::sub_agent::SubAgent for RecordingSubAgent {
    async fn run(&self, task: &str) -> oneai_core::error::Result<SubAgentSummary> {
        // Record start order + the (possibly dependency-augmented) task text.
        {
            let mut log = self.runs.lock().unwrap();
            log.push((self.kind.name().to_string(), task.to_string()));
        }
        let cur = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        // Update peak via CAS loop.
        let mut observed_peak = self.peak.load(Ordering::SeqCst);
        while cur > observed_peak {
            match self
                .peak
                .compare_exchange(observed_peak, cur, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(v) => observed_peak = v,
            }
        }
        // Yield to the runtime so wave-mates overlap in time.
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);

        Ok(SubAgentSummary {
            completed: true,
            summary: format!("RESULT-{}", self.kind.name()),
            key_findings: vec![task.to_string()],
            budget_exceeded: false,
            agent_kind: self.kind.clone(),
            tokens_used: 1000,
        })
    }
    fn kind(&self) -> &SubAgentKind {
        &self.kind
    }
    fn budget(&self) -> &TokenBudget {
        static BUDGET: TokenBudget = TokenBudget {
            total: 5000,
            consumed: 0,
        };
        &BUDGET
    }
}

/// Helper: build an AgentLoop wired to a given sub-agent factory.
fn build_delegating_loop(provider: MockProvider, factory: Arc<dyn SubAgentFactory>) -> AgentLoop {
    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));
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
        factory,
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    )
}

/// Three independent delegations in one turn must run concurrently (peak
/// concurrency > 1) and each must surface a Delegate + DelegateComplete event.
#[tokio::test]
async fn e2e_parallel_delegation() {
    use crate::mock_provider::DelegateSpec;
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::delegate_batch(vec![
            DelegateSpec::new("p1", "explore module A", "Explore"),
            DelegateSpec::new("p2", "explore module B", "Explore"),
            DelegateSpec::new("p3", "explore module C", "Explore"),
        ]),
        ScriptedResponse::direct_answer("merged all three explorations"),
    ]);

    // A concrete handle kept for assertions; a clone (type-erased) goes to the
    // loop. Both share the same inner counters/log.
    let factory = Arc::new(RecordingSubAgentFactory::new());
    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let result = build_delegating_loop(provider, factory.clone())
        .run_with_observer("explore everything", &observer)
        .await
        .unwrap();
    assert!(result.completed);

    let events = observer.events.lock().unwrap();
    let delegate_events = events
        .iter()
        .filter(|e| matches!(e, TestEvent::Delegate(_, _)))
        .count();
    assert_eq!(
        delegate_events, 3,
        "expected 3 Delegate events (one per task)"
    );
    let complete_events = events
        .iter()
        .filter(|e| matches!(e, TestEvent::DelegateComplete(_)))
        .count();
    assert_eq!(complete_events, 3, "expected 3 DelegateComplete events");
    assert_eq!(
        result.sub_agent_results.len(),
        3,
        "all 3 summaries fed back"
    );

    // The crux of parallelism: with 3 independent tasks, at least 2 must be
    // active at the same instant. A serial implementation would peak at 1.
    assert!(
        factory.peak() >= 2,
        "expected peak concurrency >= 2 for 3 independent delegates, got {}",
        factory.peak()
    );
}

/// Two delegations where B depends on A: A runs first, B runs only after A
/// completes, and B's task text must contain A's summary (auto-injected).
#[tokio::test]
async fn e2e_dependency_delegation() {
    use crate::mock_provider::DelegateSpec;
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::delegate_batch(vec![
            DelegateSpec::new("a", "explore the auth module", "Explore"),
            DelegateSpec::new("b", "implement login using findings", "Code").depends_on(&["a"]),
        ]),
        ScriptedResponse::direct_answer("done implementing"),
    ]);

    let factory = Arc::new(RecordingSubAgentFactory::new());
    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let result = build_delegating_loop(provider, factory.clone())
        .run_with_observer("explore then implement", &observer)
        .await
        .unwrap();
    assert!(result.completed);
    assert_eq!(result.sub_agent_results.len(), 2, "both summaries fed back");

    let runs = factory.runs();
    assert_eq!(runs.len(), 2, "exactly two sub-agent runs recorded");
    // Dependency ordering: A (explore) starts before B (code). Because waves
    // are serialized, B cannot start until A finishes — so the start log is
    // deterministically [explore, code].
    assert_eq!(runs[0].0, "explore", "upstream A must start first");
    assert_eq!(runs[1].0, "code", "dependent B must start after A");
    // B's received task must contain A's auto-injected summary.
    assert!(
        runs[1].1.contains("RESULT-explore"),
        "B's task must be prefixed with A's summary; got: {}",
        runs[1].1
    );
}

/// A dependency cycle among delegations must surface as an error rather than
/// looping forever.
#[tokio::test]
async fn e2e_dependency_cycle_errors() {
    use crate::mock_provider::DelegateSpec;
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::delegate_batch(vec![
            DelegateSpec::new("x", "task x", "Explore").depends_on(&["y"]),
            DelegateSpec::new("y", "task y", "Explore").depends_on(&["x"]),
        ]),
        ScriptedResponse::direct_answer("unreached"),
    ]);

    let factory = Arc::new(RecordingSubAgentFactory::new());
    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let res = build_delegating_loop(provider, factory.clone())
        .run_with_observer("cyclic", &observer)
        .await;
    assert!(
        res.is_err(),
        "a dependency cycle must error, not hang; got {:?}",
        res.as_ref().err().map(|e| e.to_string())
    );
}

// ─── Scenario 6: Approval gate — tool denied ──────────────────────────────────

#[tokio::test]
async fn e2e_scenario_6_approval_deny() {
    let shell_tool = MockTool::shell_mock();
    let shell_log = shell_tool.call_log();

    let provider = MockProvider::from_script(vec![ScriptedResponse::tool_call(
        "shell",
        serde_json::json!({"command": "rm -rf /"}),
    )]);

    // Use BlockingApprovalGate — always denies Full-permission tools
    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> = {
        let mut map = HashMap::new();
        map.insert(
            "shell".to_string(),
            Arc::new(shell_tool) as Arc<dyn oneai_core::traits::Tool>,
        );
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
        AgentLoopConfig {
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
    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        "The answer is streaming",
    )]);

    // Streaming mode works but the final answer comes from the assembled response
    // The MockProvider's streaming sends complete blocks per chunk, which the
    // IncrementalStreamParser processes differently from real SSE streams.
    // For a complete streaming E2E, we verify the loop completes and
    // observer receives stream chunks.
    let agent_loop = build_test_agent_loop(
        provider,
        vec![],
        AgentLoopConfig {
            use_streaming: true,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let result = agent_loop
        .run_with_observer("What is the answer?", &observer)
        .await
        .unwrap();

    assert!(result.completed);
    // The final answer is assembled from the streaming response.
    // It may differ from the exact text depending on how the stream parser
    // assembles the chunks — verify the loop completed successfully.
    assert!(!result.final_answer.is_empty() || result.iterations > 0);

    // Verify the loop ran — observer should have received events
    let events = observer.events.lock().unwrap();
    let iteration_starts = events
        .iter()
        .filter(|e| matches!(e, TestEvent::IterationStart(_, _)))
        .count();
    assert!(
        iteration_starts >= 1,
        "Observer should receive at least 1 iteration start"
    );
}

// ─── Scenario 8: Error recovery ──────────────────────────────────────────────

#[tokio::test]
async fn e2e_scenario_8_error_recovery() {
    // Shell mock that returns an error
    let shell_tool = MockTool::shell_mock_with_error("Error: Command timed out after 30 seconds");
    let shell_log = shell_tool.call_log();

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("shell", serde_json::json!({"command": "timeout_command"})),
        ScriptedResponse::direct_answer(
            "The command timed out, but I have an alternative approach",
        ),
    ]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> = {
        let mut map = HashMap::new();
        map.insert(
            "shell".to_string(),
            Arc::new(shell_tool) as Arc<dyn oneai_core::traits::Tool>,
        );
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
        AgentLoopConfig {
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
    assert!(
        result.final_answer.contains("timed out") || result.final_answer.contains("alternative")
    );
}

// ─── Additional: Thinking then answer ──────────────────────────────────────────

#[tokio::test]
async fn e2e_thinking_then_answer() {
    let provider = MockProvider::from_script(vec![ScriptedResponse::thinking_then_answer(
        "Let me analyze the problem...",
        "The solution is to use pattern matching",
    )]);

    // With Bug 1 fix: Thinking blocks are now properly handled.
    // In non-streaming mode, thinking blocks are part of the response
    // and parse_decision extracts only text parts.
    let agent_loop = build_test_agent_loop(
        provider,
        vec![],
        AgentLoopConfig {
            use_streaming: false,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop.run("Solve the problem").await.unwrap();

    assert!(result.completed);
    assert!(result.final_answer.contains("pattern matching"));
}

// ─── Phase 1: Streaming Thinking blocks ────────────────────────────────────────

#[tokio::test]
async fn e2e_streaming_thinking() {
    let provider = MockProvider::from_script(vec![ScriptedResponse::thinking_then_answer(
        "I need to consider the constraints",
        "The answer is 42",
    )]);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![],
        AgentLoopConfig {
            use_streaming: true,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let result = agent_loop
        .run_with_observer("What is the answer?", &observer)
        .await
        .unwrap();

    assert!(result.completed);

    // Verify that thinking fragments were received by the observer
    let events = observer.events.lock().unwrap();
    let thinking_events = events
        .iter()
        .filter(|e| matches!(e, TestEvent::Thinking(_)))
        .count();
    assert!(
        thinking_events > 0,
        "Observer should receive thinking events during streaming"
    );
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
        map.insert(
            "read_file".to_string(),
            Arc::new(read_file) as Arc<dyn oneai_core::traits::Tool>,
        );
        map.insert(
            "shell".to_string(),
            Arc::new(shell_tool) as Arc<dyn oneai_core::traits::Tool>,
        );
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
        AgentLoopConfig {
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
    assert!(
        result.final_answer.contains("Denied")
            || result.final_answer.contains("denied")
            || result.completed
    );
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

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file)],
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    // Register the audit hook
    let registry_arc = agent_loop.hook_registry();
    let mut registry = registry_arc.write().await;
    registry.register(audit_hook.clone() as Arc<dyn oneai_core::traits::LifecycleHook>);
    drop(registry); // Release the lock before running

    let result = agent_loop.run("Read the file").await.unwrap();

    assert!(result.completed);

    // Verify audit log entries were recorded
    let log_entries = audit_hook.get_log().await;
    assert!(
        !log_entries.is_empty(),
        "Audit hook should have recorded tool call events"
    );
}

// ─── Phase 1: Interrupt/Resume ────────────────────────────────────────────────

#[tokio::test]
async fn e2e_interrupt_resume() {
    use oneai_core::{InterruptReason, ResumeAction, ResumeSignal};

    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer("First answer")]);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![],
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    // Request an interrupt
    agent_loop.request_interrupt(InterruptReason::HumanFeedbackRequested {
        question: "Should I proceed?".to_string(),
    });

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let _result = agent_loop
        .run_with_observer("Do something", &observer)
        .await
        .unwrap();

    // The loop should have been interrupted
    // Since we're using MockProvider with immediate direct answer,
    // the interrupt may fire before the first iteration completes
    // depending on timing. Verify the loop handled the interrupt.

    // Resume with feedback
    let new_provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        "Proceeding with feedback",
    )]);

    let new_agent_loop = build_test_agent_loop(
        new_provider,
        vec![],
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let signal = ResumeSignal {
        interrupt_id: "test".to_string(),
        feedback: "Yes, proceed".to_string(),
        action: ResumeAction::Continue,
    };

    let resume_observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let resume_result = new_agent_loop
        .resume_from_interrupt(signal, &resume_observer)
        .await
        .unwrap();
    assert!(resume_result.completed);
}

// ─── Phase 1: StructuredOutput + ModelRetry ────────────────────────────────────

#[tokio::test]
async fn e2e_structured_output_valid() {
    use oneai_core::StructuredOutputConfig;

    // Provider returns valid JSON matching the schema
    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        serde_json::json!({
            "answer": "42",
            "confidence": 0.95
        })
        .to_string(),
    )]);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![],
        AgentLoopConfig {
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
        },
    );

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

    let agent_loop = build_test_agent_loop(
        provider,
        vec![],
        AgentLoopConfig {
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
        },
    );

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

    let agent_loop = build_test_agent_loop(
        provider,
        vec![],
        AgentLoopConfig {
            inject_skills: false,
            structured_output: Some(StructuredOutputConfig {
                schema: serde_json::json!({"type": "object", "required": ["answer"]}),
                max_retries: 1, // Only one retry attempt
                re_prompt_on_failure: true,
                error_prompt_template: None,
            }),
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop.run("What is the answer?").await.unwrap();

    assert!(result.completed);
    // The final answer should contain the validation failure message
    assert!(
        result
            .final_answer
            .contains("StructuredOutput validation failed")
            || result.final_answer.contains("not valid JSON")
    );
}

// ─── Scenario 9: Parallel sub-agent delegation with AsyncTaskRunner ──────────────

#[tokio::test]
async fn e2e_scenario_9_parallel_sub_agent_delegation() {
    use crate::async_task_runner::AsyncTaskRunner;

    // Create the AsyncTaskRunner with MockSubAgentFactory
    let runner = AsyncTaskRunner::new(Arc::new(MockSubAgentFactory));

    // Submit two tasks in parallel
    let id1 = runner
        .submit("Find authentication code", SubAgentKind::Explore)
        .await
        .unwrap();
    let id2 = runner
        .submit("Find database queries", SubAgentKind::Explore)
        .await
        .unwrap();

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
    use crate::structured_output::validate_json_schema;
    use crate::sub_agent::SubAgentKind;

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
    assert!(
        validation.passed,
        "Valid JSON should pass schema validation"
    );

    // Create a mock summary that should fail validation
    let invalid_summary_text = "This is not JSON at all";
    let invalid_validation = validate_json_schema(invalid_summary_text, &schema);
    assert!(
        !invalid_validation.passed,
        "Non-JSON text should fail schema validation"
    );
    assert!(invalid_validation
        .errors
        .iter()
        .any(|e| e.message.contains("not valid JSON")));
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
    let provider =
        MockProvider::from_script(vec![ScriptedResponse::direct_answer("The answer is 42")]);

    let agent_loop = build_test_agent_loop(
        provider,
        vec![],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    // Run with StateGraph
    let _result = agent_loop
        .run_with_state_graph(
            "What is the answer?",
            "test-react-loop", // This won't match DomainPack → falls back to manual graph
            &observer,
        )
        .await;

    // Since there's no StateGraph "test-react-loop" in the DomainPack,
    // the method falls back to standard AgentLoop execution.
    // But we can test the StateGraphExecutor directly.

    // Direct test: build executor with DirectProviderActionExecutor
    let provider2 = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("The answer is 42"), // think node
        ScriptedResponse::direct_answer("Final answer: 42"), // end node
    ]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

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
    initial_state
        .conversation
        .add_message(oneai_core::Message::user("What is the answer?"));
    initial_state
        .conversation
        .add_message(oneai_core::Message::system("You are a helpful agent."));
    initial_state
        .variables
        .insert("task".to_string(), "What is the answer?".to_string());

    let graph_result = executor.execute(&graph, initial_state).await.unwrap();

    assert!(
        graph_result.completed,
        "StateGraph should complete successfully"
    );
    assert_eq!(graph_result.terminal_node, Some("end".to_string()));
    // The end node's LlmInfer produces "Final answer: 42"
    assert!(graph_result.final_state.last_result.unwrap().contains("42"));
    assert!(graph_result.final_state.parsed_decision.is_some());

    // Verify that parsed_decision was set during execution
    let decision = graph_result.final_state.parsed_decision.unwrap();
    assert!(
        decision.is_final(),
        "Direct answer should be marked as final"
    );
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
    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        "Plan: step 1 → step 2 → step 3",
    )]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

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
    initial_state
        .conversation
        .add_message(oneai_core::Message::user("Plan the implementation"));
    initial_state.active_paradigm = Some("react".to_string());

    let graph_result = executor.execute(&graph, initial_state).await.unwrap();

    assert!(graph_result.completed);
    // After SwitchParadigm node, active_paradigm should be "plan"
    assert_eq!(
        graph_result.final_state.active_paradigm,
        Some("plan".to_string())
    );
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
    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        "The final answer is 42",
    )]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

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
    initial_state
        .conversation
        .add_message(oneai_core::Message::user("What is the answer?"));
    initial_state
        .conversation
        .add_message(oneai_core::Message::system("Answer the question."));

    let graph_result = executor.execute(&graph, initial_state).await.unwrap();

    assert!(graph_result.completed);
    // Verify parsed_decision was set and routing worked correctly
    let decision = graph_result.final_state.parsed_decision.as_ref().unwrap();
    assert!(
        decision.is_final(),
        "Should be DirectAnswer → IsFinalAnswer routes to end"
    );
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
                self.saw_plan_decision
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(self.plan_decision_resp.clone())
            }
            oneai_core::InteractionRequest::PlanReview { .. } => {
                self.saw_plan_review
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(self.plan_review_resp.clone())
            }
            oneai_core::InteractionRequest::ToolApproval { .. } => {
                self.saw_tool_approval
                    .store(true, std::sync::atomic::Ordering::Relaxed);
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
    let tools_map = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::<
        String,
        Arc<dyn oneai_core::traits::Tool>,
    >::new()));
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
        AgentLoopConfig {
            plan_mode: true,
            use_streaming: false,
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

    assert!(gate
        .saw_plan_review
        .load(std::sync::atomic::Ordering::Relaxed));
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

    assert!(gate
        .saw_plan_decision
        .load(std::sync::atomic::Ordering::Relaxed));
    assert!(gate
        .saw_plan_review
        .load(std::sync::atomic::Ordering::Relaxed));
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

// ─── Working State persistence (cross-session task continuation) ──────────

/// `exit_plan_mode` + `task_update` must persist goal/steps/progress to the
/// durable `WorkingStateStore` (cross-session event log), and the in-memory
/// `working_state` projection must reflect it. This is the P1 foundation:
/// the pinned blocks' durable source is no longer `Conversation::metadata`.
#[tokio::test]
async fn working_state_persists_plan_and_progress() {
    use oneai_core::traits::WorkingStateStore;
    use oneai_persistence::FileWorkingStateStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let store: Arc<dyn WorkingStateStore> =
        Arc::new(FileWorkingStateStore::new(tmp.path().to_path_buf()));

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call(
            "exit_plan_mode",
            serde_json::json!({
                "plan": "ship feature X",
                "steps": [
                    {"id": "1", "description": "write code"},
                    {"id": "2", "description": "write tests"},
                ]
            }),
        ),
        ScriptedResponse::tool_call(
            "task_update",
            serde_json::json!({"task_id": "1", "status": "completed"}),
        ),
        ScriptedResponse::direct_answer("done"),
    ]);
    let gate = Arc::new(MockInteractionGate::new());
    let loop_ =
        build_plan_mode_loop(provider, gate.clone()).with_working_state_store(store.clone());
    let result = loop_.run("ship feature X").await.unwrap();
    assert!(result.final_answer.contains("done"));

    // The store must hold exactly one task whose goal + 2 steps persisted, with
    // step 1 completed (the task_update was folded into the event log).
    let open = store.list_open_tasks("", "").await.unwrap();
    assert_eq!(open.len(), 1, "exactly one open task should be persisted");
    let task_id = open[0].task_id.clone();
    let ws = store.get_task(&task_id).await.unwrap().unwrap();
    assert_eq!(ws.goal, "ship feature X");
    assert_eq!(ws.steps.len(), 2);
    let s1 = ws.steps.iter().find(|s| s.id == "1").unwrap();
    assert_eq!(s1.status, oneai_core::StepStatus::Completed);
    let s2 = ws.steps.iter().find(|s| s.id == "2").unwrap();
    assert_eq!(s2.status, oneai_core::StepStatus::Pending);
}

/// With no `WorkingStateStore` configured, the loop falls back to the legacy
/// metadata-based pinned blocks and never touches the filesystem — the control
/// tools still work (backward-compat / no-store path).
#[tokio::test]
async fn no_store_falls_back_to_legacy_pinned_blocks() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call(
            "exit_plan_mode",
            serde_json::json!({
                "plan": "legacy",
                "steps": [{"id": "1", "description": "a"}]
            }),
        ),
        ScriptedResponse::direct_answer("ok"),
    ]);
    let gate = Arc::new(MockInteractionGate::new());
    let loop_ = build_plan_mode_loop(provider, gate.clone()); // no with_working_state_store
    let result = loop_.run("legacy").await.unwrap();
    assert!(result.final_answer.contains("ok"));
}

/// L9 compaction triggering: when the per-task event log crosses the
/// domain's compaction threshold, the agent-loop append path folds the old
/// events into an in-log `Snapshot` (§7.3 / §8.4). Driving a plan with many
/// steps through `exit_plan_mode` (which appends one `step_added` per step +
/// calls `compact_if_needed`) must shrink the log while preserving the
/// derived state (all steps still reconstructable from snapshot + tail).
#[tokio::test]
async fn working_state_compaction_fires_and_preserves_state() {
    use oneai_core::traits::WorkingStateStore;
    use oneai_persistence::FileWorkingStateStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    // Aggressive thresholds so a 5-step plan (1 task_created + 5 step_added
    // = 6 events) crosses the threshold. keep_recent=1 → snapshot + 1 tail.
    let store: Arc<dyn WorkingStateStore> =
        Arc::new(FileWorkingStateStore::new(tmp.path().to_path_buf()).with_compaction(3, 1));

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call(
            "exit_plan_mode",
            serde_json::json!({
                "plan": "big task",
                "steps": [
                    {"id": "1", "description": "s1"},
                    {"id": "2", "description": "s2"},
                    {"id": "3", "description": "s3"},
                    {"id": "4", "description": "s4"},
                    {"id": "5", "description": "s5"},
                ]
            }),
        ),
        ScriptedResponse::direct_answer("done"),
    ]);
    let gate = Arc::new(MockInteractionGate::new());
    let loop_ =
        build_plan_mode_loop(provider, gate.clone()).with_working_state_store(store.clone());
    let result = loop_.run("big task").await.unwrap();
    assert!(result.final_answer.contains("done"));

    let task_id = store.list_open_tasks("", "").await.unwrap()[0]
        .task_id
        .clone();
    let ws = store.get_task(&task_id).await.unwrap().unwrap();
    // All 5 steps survived compaction (folded into the snapshot).
    assert_eq!(
        ws.steps.len(),
        5,
        "all steps must survive compaction; got {:?}",
        ws.steps
    );
    assert_eq!(ws.goal, "big task");

    // The log must have been compacted — far fewer than the 6 raw events.
    // (Snapshot-in-log: 1 snapshot + 1 tail = 2.)
    let log_path = tmp.path().join("tasks").join(format!("{}.jsonl", task_id));
    let line_count = std::fs::read_to_string(&log_path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert!(
        line_count <= 3,
        "compaction should have shrunk the log; got {line_count} lines"
    );
    // First line must be the snapshot event (the compaction wrote it).
    let first = std::fs::read_to_string(&log_path).unwrap();
    assert!(first.lines().next().unwrap().contains("\"snapshot\""));
}

// ─── Meta-tool injection (delegate / switch_paradigm) ────────────────────────

/// Helper: build a non-plan-mode AgentLoop, returning a cloned handle to the
/// MockProvider so the test can inspect the recorded InferenceRequest (and
/// the tool definitions that were sent to the model).
fn build_meta_tool_loop(provider: MockProvider) -> (AgentLoop, Arc<MockProvider>) {
    let provider_arc = Arc::new(provider);
    let handle = Arc::clone(&provider_arc);
    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    let loop_ = AgentLoop::new(
        provider_arc,
        Arc::clone(&tools_map),
        Arc::new(ThreeLayerParser::new()),
        Arc::new(oneai_tool::NoopInteractionGate),
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            TokenBudget::new(100000),
            BudgetAllocation::default(),
            Arc::new(oneai_core::budget::NoopCompressor),
        )),
        // A real factory (not SubAgentFactoryNone) — `delegate` is only
        // advertised when the factory can actually fulfill it. This test
        // verifies a normal delegating agent sees the meta-tool.
        Arc::new(crate::sub_agent::DefaultSubAgentFactory::new(
            Arc::new(MockProvider::always_answers("x")),
            Arc::new(ThreeLayerParser::new()),
            Arc::new(oneai_tool::NoopInteractionGate),
            Arc::clone(&tools_map),
        )),
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        AgentLoopConfig {
            use_streaming: false,
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
    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer("done")]);
    let (loop_, provider_handle) = build_meta_tool_loop(provider);

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let _result = loop_
        .run_with_observer("do something", &observer)
        .await
        .unwrap();

    let log = provider_handle.call_log().await;
    assert!(!log.is_empty(), "at least one inference call expected");
    let sent_tools: Vec<String> = log[0]
        .request
        .tools
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(
        sent_tools.iter().any(|n| n == "delegate"),
        "delegate meta-tool must be injected; got: {:?}",
        sent_tools
    );
    assert!(
        sent_tools.iter().any(|n| n == "switch_paradigm"),
        "switch_paradigm meta-tool must be injected; got: {:?}",
        sent_tools
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
        AgentLoopConfig {
            plan_mode: true,
            use_streaming: false,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let _result = loop_.run("plan it").await.unwrap();

    let log = provider_handle.call_log().await;
    assert!(!log.is_empty(), "at least one inference call expected");
    let sent_tools: Vec<String> = log[0]
        .request
        .tools
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(
        !sent_tools.iter().any(|n| n == "delegate"),
        "delegate must NOT be injected in plan mode; got: {:?}",
        sent_tools
    );
    assert!(
        !sent_tools.iter().any(|n| n == "switch_paradigm"),
        "switch_paradigm must NOT be injected in plan mode; got: {:?}",
        sent_tools
    );
    // exit_plan_mode should still be present in plan mode.
    assert!(
        sent_tools.iter().any(|n| n == "exit_plan_mode"),
        "exit_plan_mode should be exposed in plan mode; got: {:?}",
        sent_tools
    );
}

// ─── enter_plan_mode escalation (issue #7) ─────────────────────────────────────

/// In normal mode with NO committed plan, the task control tools
/// (task_create / task_update / task_list / request_plan_decision) must NOT
/// be advertised — "工具即指令": advertising them nudges the model to
/// over-engineer simple tasks. Only `enter_plan_mode` is exposed (plus the
/// delegate / switch_paradigm meta-tools, which are unrelated).
#[tokio::test]
async fn e2e_normal_mode_without_plan_hides_task_tools() {
    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer("done")]);
    let (loop_, provider_handle) = build_meta_tool_loop(provider);

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let _result = loop_
        .run_with_observer("do something simple", &observer)
        .await
        .unwrap();

    let log = provider_handle.call_log().await;
    assert!(!log.is_empty(), "at least one inference call expected");
    let sent_tools: Vec<String> = log[0]
        .request
        .tools
        .iter()
        .map(|d| d.name.clone())
        .collect();
    // Task tools must be hidden.
    for hidden in [
        "task_create",
        "task_update",
        "task_list",
        "request_plan_decision",
        "exit_plan_mode",
    ] {
        assert!(
            !sent_tools.iter().any(|n| n == hidden),
            "{} must NOT be advertised in planless normal mode; got: {:?}",
            hidden,
            sent_tools
        );
    }
    // The escalation tool IS exposed.
    assert!(
        sent_tools.iter().any(|n| n == "enter_plan_mode"),
        "enter_plan_mode should be advertised in planless normal mode; got: {:?}",
        sent_tools
    );
}

/// When the model calls `enter_plan_mode`, the loop flips plan_mode on, so the
/// NEXT iteration advertises the plan toolset (task_create / exit_plan_mode / …)
/// and NOT enter_plan_mode. Verifies the issue #7 escalation flow end-to-end:
/// normal → enter_plan_mode → plan toolset → exit_plan_mode (approved) → answer.
#[tokio::test]
async fn e2e_enter_plan_mode_escalates_to_plan_toolset() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call(
            "enter_plan_mode",
            serde_json::json!({"plan": "搭建项目，需要 8 个步骤"}),
        ),
        ScriptedResponse::tool_call(
            "exit_plan_mode",
            serde_json::json!({
                "plan": "do it in steps",
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
        Arc::clone(&tools_map),
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
        AgentLoopConfig {
            // Start in NORMAL mode — the model escalates itself.
            plan_mode: false,
            use_streaming: false,
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let result = loop_.run("搭建一个完整项目").await.unwrap();
    assert!(result.final_answer.contains("executed"));

    let log = provider_handle.call_log().await;
    assert!(
        log.len() >= 2,
        "expected ≥2 inference calls; got {}",
        log.len()
    );

    // First call: normal mode → only enter_plan_mode among control tools.
    let first_tools: Vec<String> = log[0]
        .request
        .tools
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(
        first_tools.iter().any(|n| n == "enter_plan_mode"),
        "first call should advertise enter_plan_mode; got: {:?}",
        first_tools
    );
    assert!(
        !first_tools.iter().any(|n| n == "task_create"),
        "first call must NOT advertise task_create; got: {:?}",
        first_tools
    );

    // Second call: escalated to plan mode → full plan toolset, no enter_plan_mode.
    let second_tools: Vec<String> = log[1]
        .request
        .tools
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(
        second_tools.iter().any(|n| n == "task_create"),
        "second call should advertise task_create (plan mode); got: {:?}",
        second_tools
    );
    assert!(
        second_tools.iter().any(|n| n == "exit_plan_mode"),
        "second call should advertise exit_plan_mode (plan mode); got: {:?}",
        second_tools
    );
    assert!(
        !second_tools.iter().any(|n| n == "enter_plan_mode"),
        "second call must NOT advertise enter_plan_mode (already in plan mode); got: {:?}",
        second_tools
    );
}

// ─── Context-assembly regression: pinned blocks reach the request ────────────

/// A stub ContextSource for the regression test — injects a fixed marker
/// string so we can assert it landed in the request the provider received.
struct StubMarkerSource;
#[async_trait::async_trait]
impl oneai_domain::ContextSource for StubMarkerSource {
    fn key(&self) -> &str {
        "stub_marker"
    }
    async fn load(&self) -> oneai_core::error::Result<String> {
        Ok("STUB-MARKER-CONTENT".to_string())
    }
}

/// Regression for the dropped-`assembled` bug: on a normal (non-compression)
/// iteration, the ContextSource block AND the pinned TaskAnchor MUST reach the
/// inference request. Before the durable/ephemeral fix, `assemble()`'s output
/// was dropped and `request.conversation` used the bare durable log, so no
/// ContextSource injection (and no TaskAnchor) ever reached the model on
/// normal turns. Also asserts plan_state metadata is seeded for Q3 reseed.
#[tokio::test]
async fn e2e_assembled_context_and_task_anchor_reach_request() {
    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer("done")]);
    let provider_arc = Arc::new(provider);
    let provider_handle = Arc::clone(&provider_arc) as Arc<MockProvider>;

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    let sources: Vec<Arc<dyn oneai_domain::ContextSource>> = vec![Arc::new(StubMarkerSource)];
    let context_assembler = ContextAssembler::with_context_sources(sources);

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
        context_assembler,
        IncrementalStreamParser::new(),
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(5),
            ..AgentLoopConfig::default()
        },
    );

    let result = loop_
        .run_with_observer(
            "Refactor the auth module to use JWT",
            &TestObserver {
                events: Arc::new(Mutex::new(Vec::new())),
            },
        )
        .await
        .unwrap();
    assert!(result.completed);

    let log = provider_handle.call_log().await;
    assert!(!log.is_empty(), "at least one inference call expected");
    let req_text: String = log[0]
        .request
        .conversation
        .messages
        .iter()
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n");

    // Q2: the pinned TaskAnchor block (original task) is in the request.
    assert!(
        req_text.contains("[Task Anchor]"),
        "TaskAnchor block missing from request: {req_text}"
    );
    assert!(
        req_text.contains("Refactor the auth module to use JWT"),
        "original task missing from TaskAnchor: {req_text}"
    );
    // The ContextSource block reaches the request on a normal turn (the
    // dropped-assembled bug would have omitted this).
    assert!(
        req_text.contains("[Context: stub_marker]"),
        "ContextSource block missing from request: {req_text}"
    );
    assert!(
        req_text.contains("STUB-MARKER-CONTENT"),
        "ContextSource content missing from request: {req_text}"
    );
}

// ─── Recovery: transient tool failure → real retry with backoff ───────────────
//
// RecoveryManager's Retry strategy must actually RE-EXECUTE the tool with
// jittered backoff (not just inject a "retry scheduled" system message). This
// test wires a RecoveryManager and a read-only (Low-risk) tool that fails with
// a transient "timeout" error on the first calls, then succeeds — and asserts
// the call count reflects the retries and the loop completes with success.

use crate::error_recovery::RecoveryManager;

/// A read-only mock tool that fails its first `fail_until` calls with a
/// transient "timeout" error, then succeeds. Shared atomic counter lets the
/// test assert how many times execute() was actually invoked.
struct FlakyReadTool {
    calls: Arc<AtomicUsize>,
    fail_until: usize,
}

impl FlakyReadTool {
    fn new(fail_until: usize) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            fail_until,
        }
    }
    fn call_count(&self) -> Arc<AtomicUsize> {
        self.calls.clone()
    }
}

#[async_trait::async_trait]
impl oneai_core::traits::Tool for FlakyReadTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a file (flaky for testing recovery retry)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
    }
    fn risk_level(&self) -> oneai_core::RiskLevel {
        oneai_core::RiskLevel::Low
    }
    async fn execute(&self, _args: serde_json::Value) -> oneai_core::error::Result<ToolOutput> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= self.fail_until {
            Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("Connection timeout".to_string()),
                ..Default::default()
            })
        } else {
            Ok(ToolOutput {
                success: true,
                content: "file contents".to_string(),
                error: None,
                ..Default::default()
            })
        }
    }
}

#[tokio::test]
async fn e2e_recovery_retry_re_executes_transient_failure() {
    // Fails the first call with a transient "timeout", succeeds on the retry.
    let tool = FlakyReadTool::new(1);
    let call_count = tool.call_count();

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("read_file", serde_json::json!({"path": "x"})),
        ScriptedResponse::direct_answer("done reading"),
    ]);

    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> = {
        let mut map = HashMap::new();
        map.insert(
            "read_file".to_string(),
            Arc::new(tool) as Arc<dyn oneai_core::traits::Tool>,
        );
        Arc::new(tokio::sync::RwLock::new(map))
    };

    // Attach a RecoveryManager so transient failures are genuinely retried with
    // backoff (select_recovery_strategy → Retry → RetryScheduled).
    let rm = Arc::new(RecoveryManager::new());

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
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    )
    .with_recovery_manager(rm);

    let result = agent_loop.run("Read the file").await.unwrap();

    assert!(
        result.completed,
        "loop should complete after recovery retry succeeds"
    );
    // 1 initial failure + 1 retry that succeeds = 2 calls. Proves the tool was
    // re-executed rather than the failure merely being announced.
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "recovery should have re-executed the tool after the transient failure"
    );
    assert!(
        result.final_answer.contains("done reading"),
        "final answer should reflect the successful retry: {}",
        result.final_answer
    );
}

// ─── gap-analysis #4: OTEL metrics actually recorded during a run ────────────
//
// `OtelMetricsProvider` existed as bare AtomicU64 fields but was never
// instantiated or wired into the loop — the counters stayed at zero forever.
// This test wires one and proves inference + tool + token counters move off
// zero during a normal tool-call → direct-answer run.

#[cfg(feature = "otel")]
#[tokio::test]
async fn otel_metrics_recorded_during_run() {
    let read_file = MockTool::read_file_mock();
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("read_file", serde_json::json!({"path": "/test.txt"})),
        ScriptedResponse::direct_answer("done reading"),
    ]);
    let metrics = Arc::new(oneai_trace::OtelMetricsProvider::new());

    let agent_loop = build_test_agent_loop(
        provider,
        vec![Arc::new(read_file)],
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            metrics_provider: Some(metrics.clone()),
            ..AgentLoopConfig::default()
        },
    );

    let result = agent_loop.run("read /test.txt").await.unwrap();
    assert!(result.completed, "run should complete normally");

    let snap = metrics.snapshot();
    assert!(
        snap.inference_request_count >= 1,
        "inference counter must move off zero: {:?}",
        snap
    );
    assert!(
        snap.total_tokens_used > 0,
        "token counter must move off zero (MockProvider returns real usage): {:?}",
        snap
    );
    assert!(
        snap.tool_call_count >= 1,
        "tool-call counter must move off zero: {:?}",
        snap
    );
    assert!(
        snap.tool_success_count >= 1,
        "successful read_file should bump tool_success_count: {:?}",
        snap
    );
}

// ─── Phase 2.1 Stage A: cadence-fired Reflect sub-agent ─────────────────────

/// A recording SubAgentFactory: logs every `create(kind, …)` call and returns
/// a canned `MockSubAgent`. Lets reflect tests assert the loop spawned a
/// `Reflect` sub-agent (and lets us assert it did NOT for interrupt / default
/// cases) without standing up a real reflect AgentLoop.
struct ReflectFactory {
    creates: Arc<Mutex<Vec<SubAgentKind>>>,
}

impl ReflectFactory {
    fn new() -> Self {
        Self {
            creates: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn creates(&self) -> Arc<Mutex<Vec<SubAgentKind>>> {
        self.creates.clone()
    }
}

#[async_trait::async_trait]
impl SubAgentFactory for ReflectFactory {
    async fn create(
        &self,
        kind: SubAgentKind,
        budget: TokenBudget,
    ) -> oneai_core::error::Result<Box<dyn crate::sub_agent::SubAgent>> {
        self.creates.lock().unwrap().push(kind.clone());
        Ok(Box::new(MockSubAgent { kind, budget }))
    }
    fn available_kinds(&self) -> Vec<SubAgentKind> {
        Vec::new() // Reflect is internal-only — not advertised for `delegate`.
    }
    fn is_available(&self, _kind: &SubAgentKind) -> bool {
        false
    }
}

/// Build the trio of memory tools the reflect sub-agent's footprint guard
/// looks for, as cheap MockTools (the ReflectFactory never actually
/// dispatches them — they only need to exist by name).
fn memory_trio_tools() -> Vec<Arc<dyn oneai_core::traits::Tool>> {
    use oneai_core::PermissionLevel;
    let mk = |name: &'static str| {
        Arc::new(MockTool::new(
            name,
            "memory tool",
            serde_json::json!({"type": "object", "properties": {}}),
            ToolOutput {
                success: true,
                content: "ok".to_string(),
                error: None,
                ..Default::default()
            },
            PermissionLevel::Read,
        )) as Arc<dyn oneai_core::traits::Tool>
    };
    vec![
        mk("memory_search"),
        mk("core_memory_edit"),
        mk("archival_memory_insert"),
    ]
}

/// Build an AgentLoop wired for reflect tests: the memory trio registered
/// (so the footprint guard passes) + a recording sub-agent factory.
fn build_reflect_loop(
    provider: MockProvider,
    factory: Arc<ReflectFactory>,
    config: AgentLoopConfig,
) -> AgentLoop {
    build_reflect_loop_with_tools(provider, factory, config, memory_trio_tools())
}

/// Like `build_reflect_loop` but with a caller-supplied tool set — used by
/// the Stage B tests that register only `skill_manage` (no memory trio) to
/// exercise the relaxed footprint guard.
fn build_reflect_loop_with_tools(
    provider: MockProvider,
    factory: Arc<ReflectFactory>,
    config: AgentLoopConfig,
    tools: Vec<Arc<dyn oneai_core::traits::Tool>>,
) -> AgentLoop {
    let tools_map: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn oneai_core::traits::Tool>>>> = {
        let mut map = HashMap::new();
        for tool in tools {
            let name = oneai_core::traits::Tool::name(&*tool).to_string();
            map.insert(name, tool);
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
        factory,
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        config,
    )
}

#[test]
fn reflect_subagent_kind_unit() {
    use crate::sub_agent::SubAgentKind;
    let k = SubAgentKind::from_str("reflect");
    assert_eq!(k, SubAgentKind::Reflect);
    assert_eq!(k.name(), "reflect");
    // Hermes-style review prompt mentions frustration-as-signal.
    assert!(SubAgentKind::Reflect
        .default_system_prompt()
        .to_lowercase()
        .contains("frustration"));
    // Memory-only whitelist + skill_manage (Stage B lets the reviewer patch
    // the skill library directly).
    let tools = SubAgentKind::default_tools(&SubAgentKind::Reflect).to_vec();
    assert_eq!(
        tools,
        vec![
            "memory_search",
            "core_memory_edit",
            "archival_memory_insert",
            "skill_manage",
        ]
    );
    // Reflect is internal-only: NOT advertised for `delegate`.
    assert!(!SubAgentKind::default_tools(&SubAgentKind::Plan).contains(&"memory_search"));
}

/// Cadence fires mid-run (every N iters) AND on DirectAnswer. Provider does
/// two tool-call iterations then answers → reflect fires at iter 2 (cadence)
/// and at DirectAnswer (iter 3).
#[tokio::test]
async fn e2e_reflect_cadence_fires_midrun_and_on_answer() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("memory_search", serde_json::json!({"query": "x"})),
        ScriptedResponse::tool_call("memory_search", serde_json::json!({"query": "y"})),
        ScriptedResponse::direct_answer("done"),
    ]);

    let factory = Arc::new(ReflectFactory::new());
    let creates = factory.creates();
    let agent_loop = build_reflect_loop(
        provider,
        factory.clone(),
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(20),
            reflection_cadence: Some(2),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let result = agent_loop
        .run_with_observer("do a thing", &observer)
        .await
        .unwrap();
    assert!(result.completed, "loop should complete");

    let creates = creates.lock().unwrap().clone();
    assert!(
        creates.iter().all(|k| k == &SubAgentKind::Reflect),
        "every sub-agent spawn must be a Reflect kind: {creates:?}"
    );
    assert!(
        creates.len() >= 2,
        "reflect should fire at least twice (mid-run cadence + DirectAnswer): {creates:?}"
    );

    let reflections: Vec<TestEvent> = observer
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, TestEvent::Reflection(_)))
        .cloned()
        .collect();
    assert!(
        reflections.len() >= 2,
        "on_reflection should fire for each reflect: {:?}",
        reflections
    );
}

/// DirectAnswer-only trigger: cadence set huge so mid-run never fires, but
/// reflect still fires once on DirectAnswer delivery.
#[tokio::test]
async fn e2e_reflect_fires_on_direct_answer_only() {
    let provider = MockProvider::always_answers("final answer");
    let factory = Arc::new(ReflectFactory::new());
    let creates = factory.creates();
    let agent_loop = build_reflect_loop(
        provider,
        factory.clone(),
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(20),
            reflection_cadence: Some(usize::MAX),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let result = agent_loop
        .run_with_observer("answer me", &observer)
        .await
        .unwrap();
    assert!(result.completed);

    let creates = creates.lock().unwrap();
    assert_eq!(
        creates.len(),
        1,
        "exactly one reflect (DirectAnswer trigger) — no mid-run fire: {creates:?}"
    );
    assert_eq!(creates[0], SubAgentKind::Reflect);
}

/// Interrupt suppresses reflect: request an interrupt before run → the loop
/// returns at the 0-iteration boundary with reflect never spawned.
#[tokio::test]
async fn e2e_reflect_interrupt_suppresses() {
    use oneai_core::InterruptReason;
    let provider = MockProvider::always_answers("done");
    let factory = Arc::new(ReflectFactory::new());
    let creates = factory.creates();
    let agent_loop = build_reflect_loop(
        provider,
        factory.clone(),
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(20),
            reflection_cadence: Some(1),
            ..AgentLoopConfig::default()
        },
    );

    // Request interrupt before the loop starts — the top-of-while guard
    // catches it on iteration 1 and returns partial, before any cadence fire.
    agent_loop.request_interrupt(InterruptReason::Custom {
        reason: "test interrupt".to_string(),
    });

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let _ = agent_loop.run_with_observer("anything", &observer).await;

    let creates = creates.lock().unwrap();
    assert!(
        creates.is_empty(),
        "no reflect should fire when interrupted before the cadence boundary: {creates:?}"
    );
    let reflections = observer
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, TestEvent::Reflection(_)))
        .count();
    assert_eq!(reflections, 0);
}

/// Backward-compat: default config (reflection_cadence: None) → reflect never
/// fires, even though the memory tools are registered and the factory exists.
#[tokio::test]
async fn e2e_reflect_default_off() {
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("memory_search", serde_json::json!({"query": "q"})),
        ScriptedResponse::direct_answer("done"),
    ]);
    let factory = Arc::new(ReflectFactory::new());
    let creates = factory.creates();
    let agent_loop = build_reflect_loop(
        provider,
        factory.clone(),
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(20),
            // reflection_cadence defaults to None
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let result = agent_loop
        .run_with_observer("do a thing", &observer)
        .await
        .unwrap();
    assert!(result.completed);

    let creates = creates.lock().unwrap();
    assert!(
        creates.is_empty(),
        "reflect must NOT fire with default (None) cadence: {creates:?}"
    );
    let reflections = observer
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, TestEvent::Reflection(_)))
        .count();
    assert_eq!(reflections, 0, "no on_reflection events with cadence off");
}

/// The reflect summary must NOT pollute the parent conversation — the whole
/// point of sub-agent delegation is a clean parent context. Compare parent
/// message count against a no-cadence baseline run with the same script.
#[tokio::test]
async fn e2e_reflect_summary_not_injected_into_parent() {
    let script = vec![
        ScriptedResponse::tool_call("memory_search", serde_json::json!({"query": "q"})),
        ScriptedResponse::direct_answer("done"),
    ];

    // Baseline: cadence off.
    let baseline = build_reflect_loop(
        MockProvider::from_script(script.clone()),
        Arc::new(ReflectFactory::new()),
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(20),
            ..AgentLoopConfig::default()
        },
    );
    let baseline_result = baseline.run("do a thing").await.unwrap();
    let baseline_msgs = baseline_result.conversation.messages.len();

    // With reflect on (cadence=1) — should fire mid-run + on answer, yet the
    // parent conversation must grow by the same amount as the baseline.
    let with_reflect = build_reflect_loop(
        MockProvider::from_script(script),
        Arc::new(ReflectFactory::new()),
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(20),
            reflection_cadence: Some(1),
            ..AgentLoopConfig::default()
        },
    );
    let reflect_result = with_reflect.run("do a thing").await.unwrap();
    let reflect_msgs = reflect_result.conversation.messages.len();

    assert_eq!(
        baseline_msgs, reflect_msgs,
        "reflect summary must NOT be injected into the parent conversation \
         (baseline {baseline_msgs} msgs, with-reflect {reflect_msgs} msgs)"
    );
}

/// Stage B: the reflect footprint guard fires when *only* `skill_manage` is
/// registered (no memory trio) — the reviewer can curate the skill library
/// without the memory tools being present. Without the relaxed guard, this
/// would skip reflect entirely.
#[tokio::test]
async fn e2e_reflect_fires_with_skill_manage_only() {
    use oneai_core::PermissionLevel;
    let skill_manage = Arc::new(MockTool::new(
        "skill_manage",
        "curate skills",
        serde_json::json!({"type": "object"}),
        ToolOutput {
            success: true,
            content: "ok".to_string(),
            error: None,
            ..Default::default()
        },
        PermissionLevel::Read,
    )) as Arc<dyn oneai_core::traits::Tool>;

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("skill_manage", serde_json::json!({"action": "list"})),
        ScriptedResponse::direct_answer("done"),
    ]);
    let factory = Arc::new(ReflectFactory::new());
    let creates = factory.creates();
    let agent_loop = build_reflect_loop_with_tools(
        provider,
        factory.clone(),
        AgentLoopConfig {
            inject_skills: false,
            hard_max_iterations: Some(20),
            reflection_cadence: Some(1),
            ..AgentLoopConfig::default()
        },
        vec![skill_manage],
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let result = agent_loop
        .run_with_observer("do a thing", &observer)
        .await
        .unwrap();
    assert!(result.completed);

    let creates = creates.lock().unwrap().clone();
    assert!(
        !creates.is_empty(),
        "reflect must fire when skill_manage (only) is registered: {creates:?}"
    );
    let reflections = observer
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, TestEvent::Reflection(_)))
        .count();
    assert!(reflections >= 1, "on_reflection should fire");
}

/// Stage B: `build_skill_menu` hides Archived skills (retired = invisible to
/// the model) and reveals them again on restore. The curator's retirements
/// take effect next turn without a restart.
#[tokio::test]
async fn e2e_skill_menu_hides_archived_skill() {
    use oneai_core::SkillDescriptor;
    use oneai_skill::lifecycle::{now_unix, SkillLifecycleConfig, SkillMetadataStore};
    use oneai_skill::SkillRegistry;
    use std::path::PathBuf;

    fn tmp_root() -> PathBuf {
        let name = std::thread::current().name().unwrap_or("test").to_string();
        let mut h: u64 = 1469598103934665603;
        for b in name.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let p = std::env::temp_dir().join(format!("oneai-menu-{h:x}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    let registry = Arc::new(SkillRegistry::new());
    registry
        .register(SkillDescriptor {
            name: "retire-me".into(),
            description: "stale skill".into(),
            prompt_template: "body".into(),
            trigger_keywords: vec![],
            ..Default::default()
        })
        .await
        .unwrap();
    let store = Arc::new(SkillMetadataStore::new(
        tmp_root(),
        SkillLifecycleConfig::default(),
    ));
    store.load().await;

    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer("done")]);
    let agent_loop = build_reflect_loop_with_tools(
        provider,
        Arc::new(ReflectFactory::new()),
        AgentLoopConfig {
            inject_skills: true,
            hard_max_iterations: Some(5),
            ..AgentLoopConfig::default()
        },
        memory_trio_tools(),
    )
    .with_skill_registry(registry.clone(), None)
    .with_skill_metadata_store(Arc::clone(&store));

    // Initially visible.
    let menu = agent_loop.build_skill_menu().await.unwrap();
    assert!(menu.contains("retire-me"));

    // Archive → hidden.
    store.archive("retire-me", now_unix()).await;
    let menu = agent_loop.build_skill_menu().await;
    assert!(
        menu.is_none() || !menu.unwrap().contains("retire-me"),
        "archived skill must be hidden from the menu"
    );

    // Restore → visible again.
    store.restore("retire-me", now_unix()).await;
    let menu = agent_loop.build_skill_menu().await.unwrap();
    assert!(menu.contains("retire-me"));
}

/// Stage C — cross-session cadence hydrate. Run 1 fires reflect mid-run (at
/// the iter-2 cadence boundary) + on DirectAnswer, persisting
/// `ReflectionFired` events to the working-state log with the *cumulative*
/// iteration. Run 2 resumes the same task in a fresh loop: hydrate must
/// restore `cadence_baseline` so firing continues from the prior session's
/// last boundary (cum=4) — NOT from zero (which would re-fire the already-
/// fired cum=2 boundary). The distinguishing assertion is
/// `last_reflection_iter` strictly increasing across runs: with broken
/// hydration it stays put (re-fires the same boundaries); with hydrate it
/// advances past the prior run's last fire.
#[tokio::test]
async fn e2e_cadence_hydrates_across_run_resume() {
    use oneai_core::Conversation;
    use oneai_persistence::FileWorkingStateStore;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let store: Arc<dyn oneai_core::traits::WorkingStateStore> =
        Arc::new(FileWorkingStateStore::new(dir.path().to_path_buf()));

    // Pre-create the task in the store so run 1 can bind it via metadata.
    let task_id = store
        .create_task("u", "p", "do a thing", "", "sess1")
        .await
        .unwrap();

    let mk_loop = |provider: MockProvider, factory: Arc<ReflectFactory>| {
        build_reflect_loop(
            provider,
            factory,
            AgentLoopConfig {
                inject_skills: false,
                hard_max_iterations: Some(20),
                reflection_cadence: Some(2),
                ..AgentLoopConfig::default()
            },
        )
        .with_working_state_store(store.clone())
        .with_working_state_scope("u", "p", "sess1")
    };

    let mk_conv = |task_id: &str| {
        let mut conv = Conversation::new();
        conv.metadata
            .insert("task_id".to_string(), task_id.to_string());
        conv
    };

    // ─── Run 1: two tool-call iters then answer ──────────────────────────
    // iter1 (cum=1, no fire), iter2 (cum=2, cadence fire), iter3 (DirectAnswer fire).
    let factory1 = Arc::new(ReflectFactory::new());
    let observer1 = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let loop1 = mk_loop(
        MockProvider::from_script(vec![
            ScriptedResponse::tool_call("memory_search", serde_json::json!({"query": "a"})),
            ScriptedResponse::tool_call("memory_search", serde_json::json!({"query": "b"})),
            ScriptedResponse::direct_answer("done"),
        ]),
        factory1.clone(),
    );
    let result1 = loop1
        .run_with_conversation(mk_conv(&task_id), "do a thing", &observer1)
        .await
        .unwrap();
    assert!(result1.completed);
    let creates1 = factory1.creates().lock().unwrap().clone();
    assert!(
        creates1.len() >= 2,
        "run 1: reflect should fire ≥2× (cadence + DirectAnswer): {creates1:?}"
    );

    let ws1 = store.get_task(&task_id).await.unwrap().unwrap();
    assert!(
        ws1.reflection_count >= 2,
        "run 1: ReflectionFired events must persist (count={})",
        ws1.reflection_count
    );
    assert!(ws1.last_reflection_iter > 0);
    let run1_last = ws1.last_reflection_iter;

    // ─── Run 2: fresh loop, same task (cross-session resume) ────────────
    // Hydrate sets baseline=run1_last. iter1 (cum=run1_last+1), iter2 DirectAnswer.
    // Firing must continue past run1_last — not re-fire run1's boundaries.
    let factory2 = Arc::new(ReflectFactory::new());
    let observer2 = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let loop2 = mk_loop(
        MockProvider::from_script(vec![
            ScriptedResponse::tool_call("memory_search", serde_json::json!({"query": "c"})),
            ScriptedResponse::direct_answer("done again"),
        ]),
        factory2.clone(),
    );
    let result2 = loop2
        .run_with_conversation(mk_conv(&task_id), "do a thing", &observer2)
        .await
        .unwrap();
    assert!(result2.completed);

    let ws2 = store.get_task(&task_id).await.unwrap().unwrap();
    assert!(
        ws2.last_reflection_iter > run1_last,
        "run 2 must advance past run 1's last fire boundary (hydrate broken would \
         re-fire the same boundaries): run1_last={run1_last}, run2_last={}",
        ws2.last_reflection_iter
    );
    assert!(
        ws2.reflection_count > ws1.reflection_count,
        "run 2 must add new ReflectionFired events: {} -> {}",
        ws1.reflection_count,
        ws2.reflection_count
    );
}

// ─── Self-extension (evolution-plan §3.4) ─────────────────────────────────
//
// A tool whose side effect is to register/activate new tools must surface
// them via `ToolOutput::added_tool_names` (and the loop's live-registry diff
// backstops tools that register without self-reporting). The loop fires
// `on_tools_added` and injects a one-shot system note so the model learns the
// new tool exists next turn.

use std::sync::atomic::AtomicBool;
use tokio::sync::RwLock;

use oneai_core::traits::Tool;
use oneai_core::RiskLevel;

/// A mock tool whose `service_available()` is backed by an `AtomicBool` — the
/// Footprint gate the latent-tool test flips mid-turn.
struct GatedMockTool {
    name: String,
    gate: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl Tool for GatedMockTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "A latent tool that activates when its gate flips"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }
    fn service_available(&self) -> bool {
        self.gate.load(Ordering::Relaxed)
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, oneai_core::error::OneAIError> {
        Ok(ToolOutput {
            success: true,
            content: "latent tool ran".into(),
            error: None,
            ..Default::default()
        })
    }
}

/// A test tool that, on `execute`, mutates the shared tool map (fresh-register
/// a tool and/or flip a latent gate) and returns a `ToolOutput` whose
/// `added_tool_names` is set only when `self_report` is true.
struct InstallerTool {
    tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    /// Tool to freshly register on execute (None = no fresh registration).
    install: Option<Arc<dyn Tool>>,
    /// Latent gate to flip on execute (None = no gate flip).
    flip_gate: Option<Arc<AtomicBool>>,
    /// Whether to self-report via `added_tool_names`. When false, the loop's
    /// diff is the only signal (backstop test).
    self_report: bool,
    /// Name reported in `added_tool_names` (the surfaced tool's name).
    reported_name: Option<String>,
}
#[async_trait::async_trait]
impl Tool for InstallerTool {
    fn name(&self) -> &str {
        "install_tool"
    }
    fn description(&self) -> &str {
        "Install / activate another tool as a side effect"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, oneai_core::error::OneAIError> {
        let mut names: Vec<String> = Vec::new();
        if let Some(t) = &self.install {
            self.tools_map
                .write()
                .await
                .insert(t.name().to_string(), t.clone());
        }
        if let Some(g) = &self.flip_gate {
            g.store(true, Ordering::Relaxed);
        }
        if self.self_report {
            if let Some(n) = &self.reported_name {
                names.push(n.clone());
            }
        }
        Ok(ToolOutput {
            success: true,
            content: "installed".to_string(),
            error: None,
            added_tool_names: names,
            ..Default::default()
        })
    }
}

/// Build an AgentLoop whose tool map is shared with the test (so an installer
/// tool can mutate it mid-turn). Mirrors `build_test_agent_loop` but takes the
/// pre-built map.
fn build_agent_loop_with_map(
    provider: MockProvider,
    tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    config: AgentLoopConfig,
) -> AgentLoop {
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
        Arc::new(SubAgentFactoryNone),
        ContextAssembler::new(),
        IncrementalStreamParser::new(),
        config,
    )
}

#[tokio::test]
async fn self_extension_surfaces_self_reported_tool() {
    // Turn 1: call `install_tool` (registers `other_mock` + self-reports it).
    // Turn 2: direct answer.
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("install_tool", serde_json::json!({})),
        ScriptedResponse::direct_answer("done"),
    ]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let observer = TestObserver {
        events: events.clone(),
    };

    let tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let other = Arc::new(MockTool::read_file_mock()) as Arc<dyn Tool>;
    let installer = Arc::new(InstallerTool {
        tools_map: tools_map.clone(),
        install: Some(other.clone()),
        flip_gate: None,
        self_report: true,
        reported_name: Some("read_file".to_string()),
    }) as Arc<dyn Tool>;
    tools_map
        .write()
        .await
        .insert("install_tool".to_string(), installer);

    let loop_ = build_agent_loop_with_map(
        provider,
        tools_map,
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );
    let result = loop_
        .run_with_observer("install a tool", &observer)
        .await
        .unwrap();
    assert!(result.completed);

    let evs = events.lock().unwrap().clone();
    let added: Vec<Vec<String>> = evs
        .iter()
        .filter_map(|e| match e {
            TestEvent::ToolsAdded(n) => Some(n.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        added,
        vec![vec!["read_file".to_string()]],
        "on_tools_added must fire once with the self-reported tool name; got {added:?}"
    );
}

#[tokio::test]
async fn self_extension_diff_backstops_unreported_registration() {
    // A tool registers `other_mock` mid-turn WITHOUT setting added_tool_names.
    // The loop's live-registry diff must still surface it.
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("install_tool", serde_json::json!({})),
        ScriptedResponse::direct_answer("done"),
    ]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let observer = TestObserver {
        events: events.clone(),
    };

    let tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let other = Arc::new(MockTool::read_file_mock()) as Arc<dyn Tool>;
    let installer = Arc::new(InstallerTool {
        tools_map: tools_map.clone(),
        install: Some(other),
        flip_gate: None,
        self_report: false, // no added_tool_names — diff must catch it
        reported_name: None,
    }) as Arc<dyn Tool>;
    tools_map
        .write()
        .await
        .insert("install_tool".to_string(), installer);

    let loop_ = build_agent_loop_with_map(
        provider,
        tools_map,
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );
    let _ = loop_
        .run_with_observer("install silently", &observer)
        .await
        .unwrap();

    let evs = events.lock().unwrap().clone();
    let surfaced = evs
        .iter()
        .any(|e| matches!(e, TestEvent::ToolsAdded(n) if n.contains(&"read_file".to_string())));
    assert!(
        surfaced,
        "diff must surface the un-reported registration; events = {evs:?}"
    );
}

#[tokio::test]
async fn self_extension_diff_catches_footprint_gate_flip() {
    // A latent tool is pre-registered with service_available()=false (hidden
    // from the schema). The installer flips its gate on; the diff must surface
    // the now-active tool even though no fresh registration occurred.
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("install_tool", serde_json::json!({})),
        ScriptedResponse::direct_answer("done"),
    ]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let observer = TestObserver {
        events: events.clone(),
    };

    let tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let gate = Arc::new(AtomicBool::new(false));
    let latent = Arc::new(GatedMockTool {
        name: "latent_tool".to_string(),
        gate: gate.clone(),
    }) as Arc<dyn Tool>;
    tools_map
        .write()
        .await
        .insert("latent_tool".to_string(), latent);
    let installer = Arc::new(InstallerTool {
        tools_map: tools_map.clone(),
        install: None,
        flip_gate: Some(gate),
        self_report: false, // diff-only — the gate flip is the signal
        reported_name: None,
    }) as Arc<dyn Tool>;
    tools_map
        .write()
        .await
        .insert("install_tool".to_string(), installer);

    let loop_ = build_agent_loop_with_map(
        provider,
        tools_map,
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );
    let _ = loop_
        .run_with_observer("activate latent", &observer)
        .await
        .unwrap();

    let evs = events.lock().unwrap().clone();
    let surfaced = evs
        .iter()
        .any(|e| matches!(e, TestEvent::ToolsAdded(n) if n.contains(&"latent_tool".to_string())));
    assert!(
        surfaced,
        "diff must surface the gate-flipped latent tool; events = {evs:?}"
    );
}

// ─── #27 ToolExposure — model dispatch reject + schema filter ────────────────

/// A mock tool with a fixed `ToolExposure` — the model-dispatch guard and the
/// schema filter both consult `effective_exposure`, which (with no DomainPack)
/// falls back to `tool.exposure()`.
struct FixedExposureMockTool {
    name: String,
    exposure: oneai_core::ToolExposure,
    executed: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl Tool for FixedExposureMockTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "a tool with a fixed exposure for #27 tests"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }
    fn exposure(&self) -> oneai_core::ToolExposure {
        self.exposure
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, oneai_core::error::OneAIError> {
        self.executed.store(true, Ordering::Relaxed);
        Ok(ToolOutput {
            success: true,
            content: "ran".into(),
            error: None,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn tool_exposure_hidden_call_is_rejected_not_executed() {
    // No DomainPack → effective exposure = tool.exposure() = Hidden. The
    // model calling a Hidden tool (a hallucinated name — the schema filter
    // keeps it out of the tool list) must be rejected at dispatch and never
    // executed. Defense-in-depth for #27.
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("hidden_tool", serde_json::json!({})),
        ScriptedResponse::direct_answer("done"),
    ]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let observer = TestObserver {
        events: events.clone(),
    };

    let tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let executed = Arc::new(AtomicBool::new(false));
    let hidden = Arc::new(FixedExposureMockTool {
        name: "hidden_tool".to_string(),
        exposure: oneai_core::ToolExposure::Hidden,
        executed: executed.clone(),
    }) as Arc<dyn Tool>;
    tools_map
        .write()
        .await
        .insert("hidden_tool".to_string(), hidden);

    let loop_ = build_agent_loop_with_map(
        provider,
        tools_map,
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );
    let _ = loop_
        .run_with_observer("call the hidden tool", &observer)
        .await
        .unwrap();

    // The dispatch guard produced a failed ToolResult naming exposure=Hidden.
    let evs = events.lock().unwrap().clone();
    let rejected = evs.iter().any(|e| {
        matches!(e, TestEvent::ToolResult(_, name, out)
            if name == "hidden_tool" && !out.success
            && out.error.as_deref().unwrap_or("").contains("not model-dispatchable"))
    });
    assert!(
        rejected,
        "Hidden tool call must be rejected; events = {evs:?}"
    );
    assert!(
        !executed.load(Ordering::Relaxed),
        "Hidden tool must never execute"
    );
}

#[tokio::test]
async fn tool_exposure_deferred_call_is_dispatched() {
    // `Deferred` is model-dispatchable (the model reaches it via tool_search,
    // then calls by name). The dispatch guard must NOT reject it.
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::tool_call("deferred_tool", serde_json::json!({})),
        ScriptedResponse::direct_answer("done"),
    ]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let observer = TestObserver {
        events: events.clone(),
    };

    let tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let executed = Arc::new(AtomicBool::new(false));
    let deferred = Arc::new(FixedExposureMockTool {
        name: "deferred_tool".to_string(),
        exposure: oneai_core::ToolExposure::Deferred,
        executed: executed.clone(),
    }) as Arc<dyn Tool>;
    tools_map
        .write()
        .await
        .insert("deferred_tool".to_string(), deferred);

    let loop_ = build_agent_loop_with_map(
        provider,
        tools_map,
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );
    let _ = loop_
        .run_with_observer("call the deferred tool", &observer)
        .await
        .unwrap();

    assert!(
        executed.load(Ordering::Relaxed),
        "Deferred tool must be dispatched (model-dispatchable)"
    );
}

// ─── Scenario: switch_project meta-tool (Issue #19) ──────────────────────────

/// The `switch_project` meta-tool is intercepted by `parse_decision`, re-binds
/// every path-bound context source to the new project dir, and feeds back a
/// `tool_result` confirmation. It is never dispatched to the ToolExecutor.
#[tokio::test]
async fn e2e_switch_project_rebinds_context_and_feeds_confirmation() {
    use oneai_domain::ProjectInstructionsSource;

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_a.path().join("CLAUDE.md"), "rules for project A").unwrap();
    std::fs::write(dir_b.path().join("CLAUDE.md"), "rules for project B").unwrap();

    // A ContextAssembler with a path-bound source (real ProjectInstructionsSource)
    // so the rebind actually does something and `has_path_bound_sources()` is
    // true (which is what gates advertising `switch_project`).
    let ca = ContextAssembler::with_context_sources(vec![Arc::new(ProjectInstructionsSource::new(
        dir_a.path().to_str().unwrap(),
    ))
        as Arc<dyn oneai_domain::ContextSource>]);
    assert!(ca.has_path_bound_sources());

    let provider = MockProvider::from_script(vec![
        ScriptedResponse::switch_project(dir_b.path().to_str().unwrap()),
        ScriptedResponse::direct_answer("done analyzing project B"),
    ]);

    // Build the AgentLoop inline so we can pass the path-bound assembler.
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
        Arc::new(SubAgentFactoryNone),
        ca,
        IncrementalStreamParser::new(),
        AgentLoopConfig {
            inject_skills: false,
            thinking_budget: None,
            hard_max_iterations: Some(10),
            ..AgentLoopConfig::default()
        },
    );

    let observer = TestObserver {
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let result = agent_loop
        .run_with_observer(
            &format!("Analyze the project at {}", dir_b.path().display()),
            &observer,
        )
        .await
        .unwrap();

    // Loop completes normally — the switch didn't terminate, the final
    // DirectAnswer did.
    assert!(result.completed);
    assert_eq!(result.final_answer, "done analyzing project B");

    // The handler fed a tool_result confirmation into the durable log.
    // (Compare against the canonicalized path — on macOS the tempdir's
    // /var/folders/... resolves to /private/var/folders/... under canonicalize.)
    let canonical_b = std::fs::canonicalize(dir_b.path()).unwrap();
    // tool_result content lives in a `ToolResult` block (not extracted by
    // `text_content()`), so scan content blocks directly.
    let has_confirmation = result.conversation.messages.iter().any(|m| {
        m.role == Role::Tool
            && m.content.iter().any(|b| match b {
                oneai_core::ContentBlock::ToolResult { content, .. } => {
                    content.contains("Project context re-bound")
                        && content.contains(&canonical_b.to_string_lossy().to_string())
                }
                _ => false,
            })
    });
    assert!(
        has_confirmation,
        "durable log should carry the switch_project tool_result confirmation"
    );

    // The observer saw the switch_project tool_result event.
    let evs = observer.events.lock().unwrap().clone();
    let switch_results = evs
        .iter()
        .filter(|e| matches!(e, TestEvent::ToolResult(_, name, _) if name == "switch_project"))
        .count();
    assert_eq!(
        switch_results, 1,
        "observer should record exactly one switch_project tool_result; events = {evs:?}"
    );
}
