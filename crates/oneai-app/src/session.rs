//! AppSession — a running session with conversation context, memory, and tool access.
//!
//! An AppSession represents an active conversation with an AI agent.
//! It manages conversation history, tool execution, RAG context,
//! workflow execution, and state persistence.

use std::sync::Arc;

use oneai_core::{Conversation, GlobalState, Message, MemoryEntry, MemoryQuery};
use oneai_core::error::Result;
use oneai_core::traits::{ApprovalGate, StatePersistence};

use oneai_memory::MemoryManager;
use oneai_tool::{ToolExecutor, ToolRegistry};
use oneai_rag::{DocumentIndex, RetrievalQuery, assemble_context};
use oneai_workflow::{WorkflowConfig, WorkflowDag, WorkflowExecutor, WorkflowResult};
use oneai_persistence::FilePersistence;
use oneai_trace::{TraceContext, SpanKind, SpanStatus, EventKind};

use crate::builder::App;

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
}

/// Shared resources for all sessions.
struct AppResources {
    tool_executor: Arc<ToolExecutor>,
    approval_gate: Arc<dyn ApprovalGate>,
    memory_manager: Arc<MemoryManager>,
    rag_index: Option<Arc<DocumentIndex>>,
    persistence: Option<Arc<FilePersistence>>,
    workflow_executor: Arc<WorkflowExecutor>,
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
            let span_id = ctx.enter_span(SpanKind::SESSION, "session", None);
            ctx.set_attribute("session.id", serde_json::json!(session_id));
            ctx.set_attribute("session.platform", serde_json::json!(app.platform.name()));
            ctx.set_session_id(&session_id);
        }

        Self {
            app: Arc::new(AppResources {
                tool_executor: app.tool_executor.clone(),
                approval_gate: app.approval_gate.clone(),
                memory_manager: app.memory_manager.clone(),
                rag_index: app.rag_index.clone(),
                persistence: app.persistence.clone(),
                workflow_executor: app.workflow_executor.clone(),
            }),
            conversation,
            session_id,
            trace_context,
        }
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the conversation.
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
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

        self.app.memory_manager.add(MemoryEntry {
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            content,
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: std::collections::HashMap::from([
                ("role".to_string(), "user".to_string()),
                ("session_id".to_string(), self.session_id.clone()),
            ]),
        }).await?;

        Ok(())
    }

    /// Add an assistant message to the conversation and memory.
    pub async fn add_assistant_message(&mut self, text: impl Into<String>) -> Result<()> {
        let content = text.into();
        self.conversation.add_message(Message::assistant(content.clone()));

        self.app.memory_manager.add(MemoryEntry {
            id: format!("resp_{}", uuid::Uuid::new_v4()),
            content,
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: std::collections::HashMap::from([
                ("role".to_string(), "assistant".to_string()),
                ("session_id".to_string(), self.session_id.clone()),
            ]),
        }).await?;

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
    pub async fn retrieve_memory(&self, query: &str, top_k: usize) -> Result<Vec<MemoryEntry>> {
        // Log MemoryRetrieve event if tracing
        if let Some(ctx) = &self.trace_context {
            ctx.log_event(EventKind::MemoryRetrieve, "memory.retrieve", std::collections::HashMap::from([
                ("memory.query".to_string(), serde_json::json!(query)),
                ("memory.top_k".to_string(), serde_json::json!(top_k)),
            ]));
        }

        let memory_query = MemoryQuery {
            text: query.to_string(),
            embedding: None,
            time_range: None,
            metadata_filters: std::collections::HashMap::new(),
        };
        self.app.memory_manager.retrieve(&memory_query, top_k).await
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
    pub async fn execute_workflow(&self, config: &WorkflowConfig) -> Result<WorkflowResult> {
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
        self.app.workflow_executor.execute(&dag, config).await
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

    /// Build the trace tree from collected spans and export as JSON.
    /// Only available if tracing was enabled via AppBuilder.
    pub fn build_trace_tree(&self) -> Option<oneai_trace::TraceTree> {
        self.trace_context.as_ref().map(|ctx| ctx.build_tree())
    }

    /// End the session and close the SESSION span.
    pub fn end_session(&self, status: SpanStatus) {
        if let Some(ctx) = &self.trace_context {
            ctx.exit_span(&ctx.current_span_id().unwrap_or_default(), status);
        }
    }
}