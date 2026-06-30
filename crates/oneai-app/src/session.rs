//! AppSession — a running session with conversation context, memory, and tool access.
//!
//! An AppSession represents an active conversation with an AI agent.
//! It manages conversation history, tool execution, RAG context,
//! workflow execution, and state persistence.

use std::sync::Arc;

use oneai_core::{Conversation, GlobalState, Message, MemoryEntry};
use oneai_core::error::Result;
use oneai_core::traits::StatePersistence;

use oneai_memory::MemoryManager;
use oneai_tool::ToolExecutor;
use oneai_rag::{DocumentIndex, assemble_context};
use oneai_workflow::{WorkflowConfig, WorkflowExecutor, WorkflowResult, StateGraph, GraphExecutionResult, StateGraphExecutor, NoopDelegateFactory, render_dag_ascii, render_state_graph_ascii};
use oneai_persistence::FilePersistence;
use oneai_persistence::SqliteSessionStore;
use oneai_trace::{TraceContext, SpanKind, SpanStatus, EventKind};


/// A record of a workflow execution in the session history.
#[derive(Debug, Clone)]
pub struct WorkflowHistoryEntry {
    /// The workflow or graph name.
    pub name: String,
    /// Whether this was a DAG workflow or a StateGraph execution.
    pub kind: WorkflowKind,
    /// Whether the execution completed successfully.
    pub success: bool,
    /// Timestamp of the execution.
    pub timestamp: String,
    /// Brief summary of results.
    pub summary: String,
}

/// The kind of workflow execution recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowKind {
    /// A DAG (acyclic) workflow.
    Dag,
    /// A StateGraph (cyclic) execution.
    StateGraph,
}

/// Outcome of an explicit `/compact` run.
///
/// `compact()` summarizes older turns via the LLM and replaces the session's
/// backend `Conversation` in place (summary system message + retained recent
/// turns). This struct carries what the TUI needs to refresh its display.
#[derive(Debug, Clone, Default)]
pub struct CompactOutcome {
    /// The LLM-generated structured summary. Empty when the conversation was
    /// too short to summarize (`messages <= keep_recent_turns`).
    pub summary: String,
    /// How many older messages were folded into the summary.
    pub removed_count: usize,
    /// Retained recent turns (`(role, text)`) — user/assistant only, in order —
    /// for the TUI to re-render after clearing its display list.
    pub retained: Vec<(String, String)>,
}

use oneai_agent::{AgentLoop, AgentLoopConfig, AgentLoopObserver, AgentLoopResult,
    ParadigmKind, ToolCallRequest, SubAgentKind};

/// A running agent session with conversation context and memory.
///
/// Created from an App, each session has its own conversation
/// and memory state, but shares tools and other resources.
pub struct AppSession {
    /// Reference to shared app resources.
    app: Arc<AppResources>,
    /// The conversation for this session.
    conversation: Conversation,
    /// The session ID.
    session_id: String,
    /// Trace context (optional — for trajectory logging).
    trace_context: Option<TraceContext>,
    /// Workflow execution history for this session.
    workflow_history: Vec<WorkflowHistoryEntry>,
    /// Plan mode flag — when true, the agent loop blocks tool execution and
    /// only produces a plan. Set by the TUI before `run_agent`.
    plan_mode: bool,
}

/// Shared resources for all sessions.
struct AppResources {
    tool_executor: Arc<ToolExecutor>,
    #[allow(dead_code)]
    tool_registry: Arc<oneai_tool::ToolRegistry>,
    interaction_gate: Arc<dyn oneai_core::traits::InteractionGate>,
    memory_manager: Arc<MemoryManager>,
    rag_index: Option<Arc<DocumentIndex>>,
    persistence: Option<Arc<FilePersistence>>,
    workflow_executor: Arc<WorkflowExecutor>,
    provider: Option<Arc<dyn oneai_core::traits::LlmProvider>>,
    parser: Arc<dyn oneai_core::traits::OutputParser>,
    skill_selector: Arc<oneai_skill::SkillSelector>,
    /// Shared skill registry — same `Arc` as on `App`, read by the AgentLoop
    /// for the always-on skill menu and by the `skill` tool.
    skill_registry: Arc<oneai_skill::SkillRegistry>,
    /// Manually-activated skill (via `/skill <name>`). Shared mutable so the
    /// TUI can set it between runs and the freshly-built AgentLoop reads it.
    active_skill: Arc<tokio::sync::RwLock<Option<String>>>,
    domain_pack: Option<Arc<oneai_domain::MergedDomainPack>>,
    /// SQLite session store (for memory + conversation persistence).
    sqlite_store: Option<Arc<SqliteSessionStore>>,
    /// Usage tracker — propagated into the AgentLoop so the loop records per-call
    /// token usage. Without this the usage axis (api_calls/tokens) stays at 0.
    usage_tracker: Option<Arc<dyn oneai_core::UsageTracker>>,
    /// Rate limiter — propagated into the AgentLoop.
    rate_limiter: Option<Arc<dyn oneai_core::RateLimiter>>,
    /// Circuit breaker — propagated into the AgentLoop.
    circuit_breaker: Option<Arc<dyn oneai_core::CircuitBreaker>>,
    /// Token counter — propagated into the AgentLoop for client-side token
    /// estimation when the provider returns no usage (streaming fallback).
    token_counter: Option<Arc<dyn oneai_core::TokenCounter>>,
    /// 3-layer model context resolver — used by `warm_model_context` to probe
    /// the provider for context-window sizes and cache them for the sync path.
    model_context_resolver: Option<Arc<oneai_core::ModelContextResolver>>,
    /// Whether to probe the provider for context windows at warm-up.
    probe_context_windows: bool,
    /// Sampling / generation params — propagated into the AgentLoopConfig
    /// of every agent run.
    generation_config: oneai_core::GenerationConfig,
}

impl AppSession {
    /// Create a new session from an App.
    pub(crate) fn new(app: &crate::builder::App) -> Self {
        let session_id = uuid::Uuid::new_v4().to_string();
        let conversation = Conversation::with_id(session_id.clone());

        // Create trace context for this session (if tracing is enabled)
        let trace_context = app.trace_context.clone();

        // Start a SESSION span if tracing is enabled
        if let Some(ctx) = &trace_context {
            let _span_id = ctx.enter_span(SpanKind::SESSION, "session", None);
            ctx.set_attribute("session.id", serde_json::json!(session_id));
            ctx.set_attribute("session.platform", serde_json::json!(app.platform.name()));
            ctx.set_session_id(&session_id);
        }

        Self {
            app: Arc::new(AppResources {
                tool_executor: app.tool_executor.clone(),
                tool_registry: app.tool_registry.clone(),
                interaction_gate: app.interaction_gate.clone(),
                memory_manager: app.memory_manager.clone(),
                rag_index: app.rag_index.clone(),
                persistence: app.persistence.clone(),
                workflow_executor: app.workflow_executor.clone(),
                provider: app.provider.clone(),
                parser: app.parser.clone(),
                skill_selector: app.skill_selector.clone(),
                skill_registry: app.skill_registry.clone(),
                active_skill: app.active_skill.clone(),
                domain_pack: app.domain_pack.clone(),
                sqlite_store: app.sqlite_store.clone(),
                usage_tracker: app.usage_tracker.clone(),
                rate_limiter: app.rate_limiter.clone(),
                circuit_breaker: app.circuit_breaker.clone(),
                token_counter: app.token_counter.clone(),
                model_context_resolver: app.model_context_resolver.clone(),
                probe_context_windows: app.probe_context_windows,
                generation_config: app.generation_config.clone(),
            }),
            conversation,
            session_id,
            trace_context,
            workflow_history: Vec::new(),
            plan_mode: false,
        }
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Enable/disable plan mode for subsequent `run_agent` calls.
    pub fn set_plan_mode(&mut self, on: bool) {
        self.plan_mode = on;
    }

    /// Get the conversation.
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    /// Get the shared skill registry (read by the AgentLoop and the `skill` tool).
    pub fn skill_registry(&self) -> &Arc<oneai_skill::SkillRegistry> {
        &self.app.skill_registry
    }

    /// Manually activate a skill by name (via `/skill <name>`). Its prompt will
    /// be injected on every subsequent agent run. Pass `None` to deactivate.
    pub async fn set_active_skill(&self, name: Option<String>) {
        let mut guard = self.app.active_skill.write().await;
        *guard = name;
    }

    /// Get the currently active skill name, if any.
    pub async fn active_skill(&self) -> Option<String> {
        self.app.active_skill.read().await.clone()
    }

    /// Get the LLM provider, if configured.
    pub fn provider(&self) -> Option<&Arc<dyn oneai_core::traits::LlmProvider>> {
        self.app.provider.as_ref()
    }

    /// Get the 3-layer model context resolver, if configured.
    pub fn model_context_resolver(&self) -> Option<&Arc<oneai_core::ModelContextResolver>> {
        self.app.model_context_resolver.as_ref()
    }

    /// Whether provider context-window probing is enabled at warm-up.
    pub fn probe_context_windows(&self) -> bool {
        self.app.probe_context_windows
    }

    /// Warm the model-context resolver by probing the provider for the
    /// configured model's context window (L2), caching the result so the sync
    /// `TokenCounter::context_window_size` path reads it without re-probing.
    ///
    /// Mirrors opencode's "probe at session start, read cache thereafter" pattern.
    /// Safe to call when no provider/resolver is configured (no-op). Best-effort:
    /// probe failures silently fall through to the built-in library.
    pub async fn warm_model_context(&self) {
        if !self.app.probe_context_windows {
            return;
        }
        let (Some(resolver), Some(provider)) =
            (self.app.model_context_resolver.as_ref(), self.app.provider.as_ref())
        else {
            return;
        };
        let Some(model) = provider.config().model_name.as_deref() else {
            return;
        };
        // resolve_with_provider probes (L2) on cache miss and seeds the cache.
        let _ = resolver.resolve_with_provider(model, provider).await;
    }

    /// Send a user message and add it to memory.
    pub async fn send_user_message(&mut self, text: impl Into<String>) -> Result<()> {
        let content = text.into();

        // Log Thought event if tracing
        if let Some(ctx) = &self.trace_context {
            ctx.log_event(EventKind::Thought, "user.message", std::collections::HashMap::from([
                ("input.message".to_string(), serde_json::json!(content)),
            ]));
        }

        self.conversation.add_message(Message::user(content.clone()));
        // Working memory is single-sourced on the Conversation (M1) — no
        // parallel STM write. Long-term memory is the canonical fact_archive,
        // written by FactExtractor on compression, not by every message.

        Ok(())
    }

    /// Add an assistant message to the conversation and memory.
    pub async fn add_assistant_message(&mut self, text: impl Into<String>) -> Result<()> {
        let content = text.into();
        self.conversation.add_message(Message::assistant(content.clone()));
        // Working memory single-source (M1): no STM double-write.

        Ok(())
    }

    /// Execute a tool by name.
    pub async fn execute_tool(&self, name: &str, args: serde_json::Value) -> Result<oneai_core::ToolOutput> {
        // Start a TOOL span and log Action/Observation events if tracing
        let tool_span_id = if let Some(ctx) = &self.trace_context {
            let span_id = ctx.enter_span(SpanKind::TOOL, &format!("tool.{}", name), None);
            ctx.log_event(EventKind::Action, "tool.call", std::collections::HashMap::from([
                ("tool.name".to_string(), serde_json::json!(name)),
                ("tool.args".to_string(), args.clone()),
            ]));
            Some(span_id)
        } else {
            None
        };

        let result = self.app.tool_executor.execute(name, args).await;

        // Log Observation event and close the span
        if let Some(ctx) = &self.trace_context {
            if let Some(span_id) = &tool_span_id {
                ctx.log_event_in_span(span_id, EventKind::Observation, "tool.result", std::collections::HashMap::from([
                    ("tool.result.success".to_string(), serde_json::json!(result.as_ref().map(|r| r.success).unwrap_or(false))),
                    ("tool.result.content".to_string(), serde_json::json!(result.as_ref().map(|r| r.content.clone()).unwrap_or_default())),
                ]));
                let status = if result.is_ok() && result.as_ref().unwrap().success {
                    SpanStatus::Ok
                } else {
                    SpanStatus::Error
                };
                ctx.exit_span(span_id, status);
            }
        }

        result
    }

    /// Retrieve relevant context from memory.
    ///
    /// Returns canonical archival facts (recall_facts) mapped back to
    /// `MemoryEntry` form for API/FFI compatibility. The canonical long-term
    /// memory is the `fact_archive` (Mem0 layer); the legacy STM/LTM
    /// `MemoryEntry` stores are no longer the recall target.
    pub async fn retrieve_memory(&self, query: &str, top_k: usize) -> Result<Vec<MemoryEntry>> {
        // Log MemoryRetrieve event if tracing
        if let Some(ctx) = &self.trace_context {
            ctx.log_event(EventKind::MemoryRetrieve, "memory.retrieve", std::collections::HashMap::from([
                ("memory.query".to_string(), serde_json::json!(query)),
                ("memory.top_k".to_string(), serde_json::json!(top_k)),
            ]));
        }

        let facts = self.app.memory_manager.recall_facts(query, top_k).await?;
        Ok(facts.into_iter().map(|f| MemoryEntry {
            id: f.id,
            content: format!("{} {}: {}", f.subject, f.predicate, f.content),
            timestamp: f.updated_at,
            embedding: f.embedding,
            metadata: std::collections::HashMap::from([
                ("role".to_string(), "memory".to_string()),
                ("fact_type".to_string(), f.fact_type.as_str().to_string()),
                ("session_id".to_string(), f.session_id),
            ]),
        }).collect())
    }

    /// Retrieve relevant context from RAG (keyword-based).
    pub async fn retrieve_rag(&self, query: &str, top_k: usize) -> Result<String> {
        if let Some(rag_index) = &self.app.rag_index {
            let results = rag_index.search_by_keyword(query, top_k);
            Ok(assemble_context(&results, 2000))
        } else {
            Ok(String::new())
        }
    }

    /// Execute a workflow (compile, validate, and run).
    ///
    /// Records the execution result in the session's workflow history.
    pub async fn execute_workflow(&mut self, config: &WorkflowConfig) -> Result<WorkflowResult> {
        let dag = oneai_workflow::compile(config);
        let validation = oneai_workflow::validate(config, &dag);
        if !validation.is_valid {
            let errors: Vec<String> = validation.errors().iter()
                .map(|e| e.description.clone())
                .collect();
            return Err(oneai_core::error::OneAIError::Workflow(format!(
                "Workflow validation failed: {}", errors.join(", ")
            )));
        }
        let result = self.app.workflow_executor.execute(&dag, config).await?;

        // Record in history
        self.workflow_history.push(WorkflowHistoryEntry {
            name: config.name.clone(),
            kind: WorkflowKind::Dag,
            success: result.success,
            timestamp: chrono::Utc::now().to_rfc3339(),
            summary: format!(
                "{} steps, {} completed, {} failed, {}ms",
                result.step_results.len(),
                result.completed_steps().len(),
                result.failed_steps().len(),
                result.total_time_ms
            ),
        });

        Ok(result)
    }

    /// Get all available predefined workflows from the domain pack.
    pub fn get_available_workflows(&self) -> Vec<WorkflowConfig> {
        self.app.domain_pack.as_ref()
            .map(|dp| dp.workflows.clone())
            .unwrap_or_default()
    }

    /// Get a predefined workflow configuration by name.
    pub fn get_workflow_config(&self, name: &str) -> Option<WorkflowConfig> {
        self.app.domain_pack.as_ref()
            .and_then(|dp| dp.get_workflow_config(name))
            .cloned()
    }

    /// Get a predefined StateGraph by name.
    pub fn get_state_graph(&self, name: &str) -> Option<StateGraph> {
        self.app.domain_pack.as_ref()
            .and_then(|dp| dp.get_state_graph(name))
            .cloned()
    }

    /// Get all available StateGraph names from the domain pack.
    pub fn get_state_graph_names(&self) -> Vec<String> {
        self.app.domain_pack.as_ref()
            .map(|dp| dp.state_graph_names())
            .unwrap_or_default()
    }

    /// Render a WorkflowDag as ASCII text (for `/wf show`).
    pub fn render_workflow_dag(&self, config: &WorkflowConfig) -> String {
        let dag = oneai_workflow::compile(config);
        render_dag_ascii(&dag)
    }

    /// Render a StateGraph as ASCII text (for `/wf graph`).
    pub fn render_state_graph(&self, graph: &StateGraph) -> String {
        render_state_graph_ascii(graph)
    }

    /// Execute a StateGraph (create executor, run, and return result).
    ///
    /// Requires a configured LLM provider. Returns an error if no provider is available.
    /// Records the execution result in the session's workflow history.
    pub async fn execute_state_graph(&mut self, graph: &StateGraph) -> Result<GraphExecutionResult> {
        let provider = self.app.provider.as_ref()
            .ok_or(oneai_core::error::OneAIError::Provider(
                "No LLM provider configured. Cannot execute StateGraph.".to_string()
            ))?;

        let executor = StateGraphExecutor::with_direct_provider_defaults(
            provider.clone(),
            self.app.workflow_executor.tools_handle(),
            Arc::new(NoopDelegateFactory), // Use noop for now; real impl wired in later
            self.app.interaction_gate.clone(),
        );

        let initial_state = oneai_workflow::GraphState::new();
        let result = executor.execute(graph, initial_state).await?;

        // Record in history
        self.workflow_history.push(WorkflowHistoryEntry {
            name: graph.name.clone(),
            kind: WorkflowKind::StateGraph,
            success: result.completed,
            timestamp: chrono::Utc::now().to_rfc3339(),
            summary: format!(
                "{} iterations, completed: {}, terminal: {}",
                result.iterations,
                result.completed,
                result.terminal_node.as_deref().unwrap_or("none")
            ),
        });

        Ok(result)
    }

    /// Get the workflow execution history for this session.
    pub fn workflow_history(&self) -> &[WorkflowHistoryEntry] {
        &self.workflow_history
    }

    /// Save a checkpoint of the current session state.
    pub async fn save_checkpoint(&self) -> Result<String> {
        // Log checkpoint save event if tracing
        if let Some(ctx) = &self.trace_context {
            ctx.log_event(EventKind::CheckpointSave, "checkpoint.save", std::collections::HashMap::from([
                ("checkpoint.session_id".to_string(), serde_json::json!(self.session_id)),
            ]));
        }

        if let Some(persistence) = &self.app.persistence {
            let state = oneai_core::AgentState {
                session_id: self.session_id.clone(),
                global_state: GlobalState::new(),
                active_paradigm: "session".to_string(),
                active_step: None,
                timestamp: chrono::Utc::now(),
            };
            persistence.save_checkpoint(&state).await
        } else {
            Err(oneai_core::error::OneAIError::Persistence(
                "No persistence layer configured".to_string()
            ))
        }
    }

    /// Get the memory manager.
    pub fn memory_manager(&self) -> &Arc<MemoryManager> {
        &self.app.memory_manager
    }

    /// Get the tool executor.
    pub fn tool_executor(&self) -> &Arc<ToolExecutor> {
        &self.app.tool_executor
    }

    /// Get the trace context (for trajectory logging).
    pub fn trace_context(&self) -> Option<&TraceContext> {
        self.trace_context.as_ref()
    }

    /// Run the Agentic Loop on a user task.
    ///
    /// This is the primary interactive entry point: send a task to the
    /// AgentLoop, which dynamically decides how to handle it (direct answer,
    /// tool calls, delegation, paradigm switching). Returns the final result.
    ///
    /// Requires a configured LLM provider (set via AppBuilder).
    /// Returns an error if no provider is available.
    ///
    /// Multi-turn: the session's full conversation history is passed to the
    /// AgentLoop, so the model sees prior messages and can maintain context.
    /// Memory: relevant memories are retrieved and injected as context.
    pub async fn run_agent(
        &mut self,
        task: &str,
        observer: &dyn AgentLoopObserver,
        interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>>,
    ) -> Result<AgentLoopResult> {
        let provider = self.app.provider.as_ref()
            .ok_or(oneai_core::error::OneAIError::Provider(
                "No LLM provider configured. Set ONEAI_API_KEY and ONEAI_BASE_URL environment variables.".to_string()
            ))?;

        // Snapshot the manually-activated skill (set via `/skill <name>`) for
        // this run — its prompt_template is injected every turn by the loop.
        let active_skill = self.app.active_skill.read().await.clone();

        // Store user message in memory
        // P5: set the per-run session id on the memory manager so fact
        // extraction / self-managed tools namespace facts to this session.
        self.app.memory_manager.set_session_id(self.session_id.clone()).await;
        // Load durable facts (cross-session habits + this session's episodic)
        // into the archival tier. Idempotent (upsert-deduped), so safe per turn.
        self.app.memory_manager.load_persisted_facts().await;
        // Working memory is single-sourced on the Conversation (M1): the user
        // task is added to the conversation by the agent loop; no parallel STM
        // write here. Canonical long-term memory lives in fact_archive.

        // Recall relevant facts from the canonical archival tier for this turn
        // (R1). Recall is routed through the CoreMemorySource (an
        // anti-compression ContextSource injected every iteration) instead of a
        // one-shot system message that could be summarized away. The core
        // block also carries the agent's curated core-memory facts.
        let recall_top_k = self.app.domain_pack.as_ref()
            .map(|d| d.memory_profile.recall.top_k)
            .unwrap_or(5);
        let recalled_facts = self.app.memory_manager
            .recall_facts(task, recall_top_k).await?;

        // Build the core memory source and populate its recall for this turn.
        let core_memory_source = std::sync::Arc::new(
            oneai_memory::CoreMemorySource::new(self.app.memory_manager.core_memory().clone())
        );
        core_memory_source.set_recall(recalled_facts).await;

        // Build conversation with history (memory is now injected via the
        // CoreMemorySource each iteration, not as a one-shot system message).
        let conversation = self.conversation.clone();

        // Build context assembler (core source first, then domain sources).
        let context_assembler = if let Some(domain) = &self.app.domain_pack {
            let mut sources = vec![core_memory_source as std::sync::Arc<dyn oneai_domain::ContextSource>];
            sources.extend(domain.context_sources.clone());
            oneai_agent::ContextAssembler::with_context_sources(sources)
        } else {
            oneai_agent::ContextAssembler::with_context_sources(
                vec![core_memory_source as std::sync::Arc<dyn oneai_domain::ContextSource>]
            )
        };

        // Propagate usage/rate/circuit from the App into the loop
        // config. Without this, the loop builds `..AgentLoopConfig::default()`
        // (usage_tracker = None) and never records usage — the usage axis
        // (api_calls, tokens) silently stays at 0 even though the app holds a
        // configured tracker.
        let usage_tracker = self.app.usage_tracker.clone();
        let rate_limiter = self.app.rate_limiter.clone();
        let circuit_breaker = self.app.circuit_breaker.clone();
        let token_counter = self.app.token_counter.clone();

        // Build the AgentLoop from session resources
        let agent_loop = if let Some(domain) = &self.app.domain_pack {
            let mut config = AgentLoopConfig {
                system_prompt: if domain.system_prompt_template.is_empty() {
                    AgentLoopConfig::default().system_prompt
                } else {
                    domain.system_prompt_template.clone()
                },
                use_streaming: true,
                plan_mode: self.plan_mode,
                usage_tracker,
                rate_limiter,
                circuit_breaker,
                token_counter,
                // Hand the loop the SAME trace context the session holds (Arc-backed,
                // cheap clone) so its per-iteration/inference/tool spans land in the
                // tree compute_from_tree reads. Without this the 效率 axis (per-call
                // latency, tool_call_count, avg_iterations) is all zeros.
                trace_context: self.trace_context.clone(),
                ..AgentLoopConfig::default()
            };
            // Apply user-configured generation params (temperature/top_p/
            // max_tokens/thinking_budget/stop_sequences) on top of the defaults.
            config.apply_generation_config(&self.app.generation_config);
            // Use the real ContextCompressor with the domain's CompressionTemplate,
            // so that compression preserves domain-critical information.
            // P3: also wire compression-coupled fact extraction — discarded
            // (summarized-away) turns are extracted per the domain's
            // MemoryProfile schema and conflict-resolved into the archival
            // tier, so compressed-out information is not lost.
            let compressor: Arc<dyn oneai_core::budget::ContextCompressorTrait> =
                Arc::new(oneai_memory::ContextCompressor::with_template(
                    80000,  // threshold_tokens — trigger compression at 80% of budget
                    6,      // keep_recent_turns
                    provider.clone(),
                    domain.compression_template.clone(),
                )
                .with_fact_extraction(
                    domain.memory_profile.extraction_schema.clone(),
                    self.app.memory_manager.fact_archive().clone(),
                    self.app.memory_manager.user_id().await,
                    self.session_id.clone(),
                ));
            AgentLoop::with_domain_pack(
                provider.clone(),
                self.app.tool_executor.tools_map(),
                self.app.parser.clone(),
                self.app.interaction_gate.clone(),
                self.app.skill_selector.clone(),
                Arc::new(oneai_core::budget::ContextBudgetManager::new(
                    oneai_core::budget::TokenBudget::new(100000),
                    oneai_core::budget::BudgetAllocation::default(),
                    compressor,
                )
                .with_discarded_sink(
                    Arc::new(oneai_memory::ArchivalDiscardedSink::new(
                        self.app.memory_manager.clone(),
                    )),
                    self.session_id.clone(),
                )),
                Arc::new(oneai_agent::DefaultSubAgentFactory::new(
                    provider.clone(),
                    self.app.parser.clone(),
                    self.app.interaction_gate.clone(),
                    self.app.tool_executor.tools_map(),
                )),
                context_assembler,
                oneai_agent::IncrementalStreamParser::new(),
                None, // checkpoint manager — optional
                config,
                domain.clone(),
            )
            .with_skill_registry(
                self.app.skill_registry.clone(),
                active_skill.clone(),
            )
        } else {
            let mut config = AgentLoopConfig {
                use_streaming: true,
                plan_mode: self.plan_mode,
                usage_tracker,
                rate_limiter,
                circuit_breaker,
                token_counter,
                // Hand the loop the SAME trace context the session holds (Arc-backed,
                // cheap clone) so its per-iteration/inference/tool spans land in the
                // tree compute_from_tree reads. Without this the 效率 axis (per-call
                // latency, tool_call_count, avg_iterations) is all zeros.
                trace_context: self.trace_context.clone(),
                ..AgentLoopConfig::default()
            };
            // Apply user-configured generation params on top of the defaults.
            config.apply_generation_config(&self.app.generation_config);
            // No domain pack — still use a real ContextCompressor with a
            // generic summarization prompt and default fact extraction (B3/C3),
            // so compression-coupled fact extraction and discarded archival
            // work even without a domain's MemoryProfile. (Previously this
            // used NoopCompressor: zero compression, zero extraction, and raw
            // discarded transcript was silently lost.)
            let default_schema = vec![
                oneai_core::FactType::new("user_tooling_pref"),
                oneai_core::FactType::new("decision"),
                oneai_core::FactType::new("open_task"),
            ];
            let compressor: Arc<dyn oneai_core::budget::ContextCompressorTrait> =
                Arc::new(oneai_memory::ContextCompressor::new(
                    80000, // threshold_tokens
                    6,     // keep_recent_turns
                    provider.clone(),
                )
                .with_fact_extraction(
                    default_schema,
                    self.app.memory_manager.fact_archive().clone(),
                    self.app.memory_manager.user_id().await,
                    self.session_id.clone(),
                ));
            AgentLoop::new(
                provider.clone(),
                self.app.tool_executor.tools_map(),
                self.app.parser.clone(),
                self.app.interaction_gate.clone(),
                self.app.skill_selector.clone(),
                Arc::new(oneai_core::budget::ContextBudgetManager::new(
                    oneai_core::budget::TokenBudget::new(100000),
                    oneai_core::budget::BudgetAllocation::default(),
                    compressor,
                )
                .with_discarded_sink(
                    Arc::new(oneai_memory::ArchivalDiscardedSink::new(
                        self.app.memory_manager.clone(),
                    )),
                    self.session_id.clone(),
                )),
                Arc::new(oneai_agent::DefaultSubAgentFactory::new(
                    provider.clone(),
                    self.app.parser.clone(),
                    self.app.interaction_gate.clone(),
                    self.app.tool_executor.tools_map(),
                )),
                context_assembler,
                oneai_agent::IncrementalStreamParser::new(),
                None, // checkpoint manager — optional
                config,
            )
            .with_skill_registry(
                self.app.skill_registry.clone(),
                active_skill.clone(),
            )
        };

        // Register the running AgentLoop so the TUI can request an interrupt
        // (Esc) without holding the session lock — the slot is a separate Arc.
        // Cloning the AgentLoop is cheap (all Arc fields).
        *interrupt_slot.lock().await = Some(agent_loop.clone());

        // Run with full conversation history (multi-turn)
        let result = agent_loop.run_with_conversation(conversation, task, observer).await;

        // Clear the interrupt slot now that the run is over.
        *interrupt_slot.lock().await = None;

        let result = result?;

        // The assistant's final answer is already part of the loop's
        // conversation (merged below), so working memory captures it without a
        // parallel STM write (M1 single-source).

        // Merge the loop's conversation back into the session
        self.conversation = result.conversation.clone();

        // ─── Auto-save session to SQLite ──────────────────────────────
        // If SQLite persistence is enabled, save the conversation and STM
        // after each agent run. This enables session resume on restart.
        if let Some(_sqlite) = &self.app.sqlite_store {
            if let Err(e) = self.app.memory_manager.save_session(&self.session_id, &self.conversation).await {
                tracing::warn!("Failed to auto-save session '{}': {}", self.session_id, e);
            }
        }

        // ─── STM↔LTM Closed Loop: Memory Reflection ──────────────
        // At session end, reflect on the STM entries and generate
        // an episodic memory for long-term retention.
        // This is only triggered when a MemoryReflection engine is
        // configured (via AppBuilder.with_memory_reflection()).
        if let Some(reflection) = self.app.memory_manager.reflection() {
            let config = reflection.config();
            if config.auto_reflect {
                // Reflect over the live conversation (M1 single source) and
                // store the episodic as a canonical archival fact + persist.
                let episodic = self.app.memory_manager
                    .reflect(&self.session_id, &self.conversation).await?;
                if let Some(episodic) = episodic {
                    tracing::info!(
                        "Memory reflection completed for session {}: {} insights, {} decisions",
                        self.session_id,
                        episodic.key_insights.len(),
                        episodic.decisions.len()
                    );
                }
            }
        }

        Ok(result)
    }

    /// Run the Agentic Loop silently (no observer callbacks).
    pub async fn run_agent_silent(&mut self, task: &str) -> Result<AgentLoopResult> {
        struct SilentObserver;
        impl AgentLoopObserver for SilentObserver {
            fn on_iteration_start(&self, _: usize, _: ParadigmKind) {}
            fn on_direct_answer(&self, _: &str) {}
            fn on_tool_calls(&self, _: &[ToolCallRequest]) {}
            fn on_tool_result(&self, _: &str, _: &str, _: &oneai_core::ToolOutput) {}
            fn on_delegate(&self, _: &str, _: &SubAgentKind) {}
            fn on_paradigm_switch(&self, _: ParadigmKind) {}
            fn on_checkpoint(&self, _: usize) {}
            fn on_complete(&self, _: &AgentLoopResult) {}
        }
        let throwaway_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        self.run_agent(task, &SilentObserver, throwaway_slot).await
    }

    /// Compact the conversation in place — the manual `/compact` entry point.
    ///
    /// Unlike the AgentLoop's budget-triggered auto-compression (which fires at
    /// ~80% of the token budget), this always summarizes: it folds the older
    /// turns into a single LLM-generated summary (using the domain's
    /// `CompressionTemplate` when available — CodingPack's is already a
    /// Claude-Code-style structured template) and keeps the last
    /// `keep_recent_turns` messages intact. The summary is injected as a system
    /// message at the head of the *backend* `Conversation`, so the model
    /// actually sees it on the next `run_agent` — context is preserved, not
    /// dropped. The session id and sidebar entry stay the same (no reset).
    ///
    /// Returns the summary text, the count of folded messages, and the retained
    /// recent turns for display. `summary` is empty when the conversation was
    /// too short to summarize.
    pub async fn compact(&mut self, keep_recent_turns: usize) -> Result<CompactOutcome> {
        let provider = self.app.provider.as_ref()
            .ok_or_else(|| oneai_core::error::OneAIError::Provider(
                "No LLM provider configured. Cannot /compact.".to_string()
            ))?;

        // Build the compressor mirroring run_agent's wiring, but with a zero
        // threshold so /compact always compresses regardless of budget.
        let compressor = if let Some(domain) = &self.app.domain_pack {
            oneai_memory::ContextCompressor::with_template(
                0,
                keep_recent_turns,
                provider.clone(),
                domain.compression_template.clone(),
            )
            .with_fact_extraction(
                domain.memory_profile.extraction_schema.clone(),
                self.app.memory_manager.fact_archive().clone(),
                self.app.memory_manager.user_id().await,
                self.session_id.clone(),
            )
        } else {
            oneai_memory::ContextCompressor::new(0, keep_recent_turns, provider.clone())
            .with_fact_extraction(
                vec![
                    oneai_core::FactType::new("user_tooling_pref"),
                    oneai_core::FactType::new("decision"),
                    oneai_core::FactType::new("open_task"),
                ],
                self.app.memory_manager.fact_archive().clone(),
                self.app.memory_manager.user_id().await,
                self.session_id.clone(),
            )
        };

        let result = compressor.compress(&self.conversation).await?;
        let summary = result.summary.unwrap_or_default();
        let removed_count = result.removed_entries.len();
        let had_summary = !summary.is_empty();

        // Archive the discarded raw transcript (C2 "压缩即不丢") before it
        // leaves the live conversation — fact extraction already ran inside
        // the compressor; this persists the raw segment as a snapshot.
        self.app.memory_manager
            .archive_discarded_snapshot(&self.session_id, result.discarded_messages.clone())
            .await
            .ok(); // non-critical: a failed snapshot must not break /compact

        // Replace the backend conversation in place: summary system message
        // followed by the retained recent turns. This is the core fix — the
        // summary now lives in the conversation the model sees, not just the
        // TUI display.
        self.conversation = result.compressed_conversation;

        // Persist the compacted conversation, mirroring run_agent's auto-save.
        if self.app.sqlite_store.is_some() {
            if let Err(e) = self.app.memory_manager
                .save_session(&self.session_id, &self.conversation).await
            {
                tracing::warn!("Failed to persist compacted session '{}': {}", self.session_id, e);
            }
        }

        // Collect retained recent turns for the TUI to re-render. When a summary
        // was produced, ContextCompressor prepends it as a leading system
        // message — skip it. When the conversation was too short to summarize,
        // the conversation is returned unchanged (no leading summary), so skip
        // nothing.
        let retained: Vec<(String, String)> = self.conversation.messages.iter()
            .skip(if had_summary { 1 } else { 0 })
            .filter_map(|m| match m.role {
                oneai_core::Role::User => Some(("user".to_string(), m.text_content())),
                oneai_core::Role::Assistant => Some(("assistant".to_string(), m.text_content())),
                _ => None,
            })
            .collect();

        Ok(CompactOutcome { summary, removed_count, retained })
    }
}