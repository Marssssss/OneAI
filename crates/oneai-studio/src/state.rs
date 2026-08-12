//! StudioState — shared state that connects trace, AgentLoop, StateGraph,
//! and checkpoint data to the Studio frontend via the unified engine bus.
//!
//! P5 convergence: `StudioState` no longer implements `AgentLoopObserver`
//! directly. The CLI attaches an `oneai_bus::InProcessBus` (via `set_bus`)
//! and drives the turn with a `BusObserver` that emits `EngineYield`s to it;
//! the WebSocket handler subscribes to that bus and forwards `serialize_yield`
//! lines to the browser. This deletes the per-frontend `StudioEvent` enum +
//! hand-rolled observer projection that previously drifted against the TUI
//! and supervisor schemas.

use std::collections::HashMap;
use std::sync::Arc;

use oneai_bus::EngineBus;
use oneai_persistence::FilePersistence;
use oneai_tool::ToolRegistry;
use oneai_trace::{InMemoryCollector, TraceContext};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};

// ─── StudioRunner ────────────────────────────────────────────────────

/// Drives an agent turn in response to a `POST /api/run` request.
///
/// This lives in the feature crate only as a trait: `oneai-studio` sits
/// *below* `oneai-app` in the layering and so cannot hold an `AppSession`
/// or call `run_agent` directly. The CLI (`examples/cli/cmd_studio`)
/// builds the real `App`/`AppSession` + a `oneai_bus::InProcessBus`, wires a
/// `BusObserver` to the loop, sets that bus on `StudioState` (so `/ws`
/// forwards the yields), and supplies a `StudioRunner` impl; `StudioState`
/// holds it (`set_runner`) and the `/api/run` handler calls it.
#[async_trait::async_trait]
pub trait StudioRunner: Send + Sync {
    /// Whether the runner has a configured provider and is not currently
    /// running a turn.
    fn status(&self) -> RunnerStatus;

    /// Run one agent turn for `task`. Iteration / tool-call / streaming /
    /// completion events flow to all WebSocket subscribers through the
    /// `EngineYield` stream on the bus the runner holds (set on
    /// `StudioState` via `set_bus`).
    async fn run_task(&self, task: &str) -> RunOutcome;
}

/// Snapshot of runner availability, surfaced to the `/api/run` handler.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunnerStatus {
    /// A provider (API key/base URL) is configured.
    pub has_provider: bool,
    /// A turn is currently in flight.
    pub busy: bool,
}

/// Outcome of a `run_task` call — used by the runner to report completion.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The turn completed (agent reached a final answer or exhausted budget).
    Done { completed: bool, iterations: usize },
    /// The runner could not start (e.g. no provider / still busy).
    Rejected { reason: String },
    /// The turn failed with an error.
    Error { message: String },
}

// ─── SessionView ─────────────────────────────────────────────────────

/// A tracked session in the Studio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    /// Session ID.
    pub id: String,
    /// Active paradigm.
    pub paradigm: String,
    /// Current iteration number.
    pub iteration: usize,
    /// Whether the session is running.
    pub running: bool,
    /// Total tokens used.
    pub total_tokens: u64,
}

// ─── SessionUpdate ───────────────────────────────────────────────────

/// Partial update to a session's state.
#[derive(Debug, Clone)]
pub enum SessionUpdate {
    Paradigm(String),
    Iteration(usize),
    Running(bool),
    Tokens(u64),
}

// ─── StudioState ─────────────────────────────────────────────────────

/// Shared state for the Studio server — connects all data sources and
/// broadcasts `EngineYield`s to WebSocket subscribers via the engine bus.
///
/// The CLI attaches an `InProcessBus` (built alongside the `App`/`AppSession`)
/// via `set_bus`; the WS handler subscribes through `subscribe()`. With no
/// bus attached (standalone `serve()`), the WS sits idle — no agent, no yields.
pub struct StudioState {
    /// Trace context for collecting execution data.
    trace_context: TraceContext,

    /// Persistence for checkpoint time-travel.
    persistence: Arc<FilePersistence>,

    /// Tool registry for listing available tools.
    tool_registry: Arc<ToolRegistry>,

    /// Active sessions being tracked.
    sessions: RwLock<HashMap<String, SessionView>>,

    /// The engine bus the `BusObserver` emits `EngineYield`s to. Set by the
    /// CLI when it attaches a runner; `None` for the standalone read-only
    /// server (no agent → no yields → WS idle).
    bus: RwLock<Option<Arc<dyn EngineBus>>>,

    /// Optional agent driver — set by the CLI (`cmd_studio`) so the
    /// `/api/run` endpoint can launch real agent turns. `None` for the
    /// standalone `serve()` server (read-only observer).
    runner: RwLock<Option<Arc<dyn StudioRunner>>>,
}

impl StudioState {
    /// Create a new StudioState with the given components.
    pub fn new(
        trace_context: TraceContext,
        persistence: Arc<FilePersistence>,
        tool_registry: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            trace_context,
            persistence,
            tool_registry,
            sessions: RwLock::new(HashMap::new()),
            bus: RwLock::new(None),
            runner: RwLock::new(None),
        }
    }

    /// Create a StudioState with default components for standalone Studio.
    pub fn new_default() -> Self {
        let trace_context = TraceContext::new(Arc::new(InMemoryCollector::new()));
        let persistence = Arc::new(FilePersistence::new("/tmp/oneai-studio-checkpoints"));
        let tool_registry = Arc::new(ToolRegistry::new());

        Self::new(trace_context, persistence, tool_registry)
    }

    /// Subscribe to the engine bus's `EngineYield` stream — each WebSocket
    /// connection subscribes via this method. Returns `None` when no bus is
    /// attached (standalone server); the WS handler then idles after the
    /// welcome message.
    pub async fn subscribe(&self) -> Option<broadcast::Receiver<oneai_bus::EngineYield>> {
        self.bus.read().await.clone().map(|b| b.subscribe_yields())
    }

    /// Attach the engine bus the CLI built alongside the `App`/`AppSession`.
    /// The `BusObserver` driving the turn emits to this bus; `subscribe()`
    /// reads the same stream.
    pub async fn set_bus(&self, bus: Option<Arc<dyn EngineBus>>) {
        *self.bus.write().await = bus;
    }

    /// Attach (or detach) the agent driver. Called by the CLI after
    /// building the `App`/`AppSession` + bus so `/api/run` can launch turns.
    pub async fn set_runner(&self, runner: Option<Arc<dyn StudioRunner>>) {
        *self.runner.write().await = runner;
    }

    /// Get a clone of the attached runner, if any.
    pub async fn runner(&self) -> Option<Arc<dyn StudioRunner>> {
        self.runner.read().await.clone()
    }

    /// Convenience: is a runner currently attached? (cheap — does not check
    /// provider/busy; use `runner().status()` for that.)
    pub async fn has_runner(&self) -> bool {
        self.runner.read().await.is_some()
    }

    /// Get the trace context.
    pub fn trace_context(&self) -> &TraceContext {
        &self.trace_context
    }

    /// Get the persistence layer.
    pub fn persistence(&self) -> &Arc<FilePersistence> {
        &self.persistence
    }

    /// Get the tool registry.
    pub fn tool_registry(&self) -> &Arc<ToolRegistry> {
        &self.tool_registry
    }

    /// Register a new session in the Studio.
    pub async fn register_session(&self, session: SessionView) {
        self.sessions
            .write()
            .await
            .insert(session.id.clone(), session);
    }

    /// Update a session's state.
    pub async fn update_session(&self, id: &str, update: SessionUpdate) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(id) {
            match update {
                SessionUpdate::Paradigm(p) => session.paradigm = p,
                SessionUpdate::Iteration(i) => session.iteration = i,
                SessionUpdate::Running(r) => session.running = r,
                SessionUpdate::Tokens(t) => session.total_tokens = t,
            }
        }
    }

    /// List all tracked sessions.
    pub async fn list_sessions(&self) -> Vec<SessionView> {
        self.sessions.read().await.values().cloned().collect()
    }

    /// Get a specific session.
    pub async fn get_session(&self, id: &str) -> Option<SessionView> {
        self.sessions.read().await.get(id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_bus::{EngineBus, EngineYield, InProcessBus};

    #[test]
    fn test_studio_state_creation() {
        let state = StudioState::new_default();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let sessions = rt.block_on(state.list_sessions());
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn subscribe_without_bus_is_none() {
        let state = StudioState::new_default();
        assert!(state.subscribe().await.is_none());
    }

    #[tokio::test]
    async fn bus_forwards_yields_to_subscriber() {
        let state = StudioState::new_default();
        let bus: Arc<dyn EngineBus> = Arc::new(InProcessBus::default());
        state.set_bus(Some(bus.clone())).await;

        let mut rx = state.subscribe().await.expect("bus attached");
        bus.emit(EngineYield::StreamChunk {
            turn_id: "t1".into(),
            text: "hi".into(),
        })
        .unwrap();

        let y = rx.recv().await.unwrap();
        match y {
            EngineYield::StreamChunk { text, .. } => assert_eq!(text, "hi"),
            _ => panic!("expected StreamChunk"),
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive() {
        let state = StudioState::new_default();
        let bus: Arc<dyn EngineBus> = Arc::new(InProcessBus::default());
        state.set_bus(Some(bus.clone())).await;

        let mut rx1 = state.subscribe().await.unwrap();
        let mut rx2 = state.subscribe().await.unwrap();
        bus.emit(EngineYield::DirectAnswer {
            turn_id: "t".into(),
            text: "42".into(),
        })
        .unwrap();

        assert!(matches!(
            rx1.recv().await.unwrap(),
            EngineYield::DirectAnswer { .. }
        ));
        assert!(matches!(
            rx2.recv().await.unwrap(),
            EngineYield::DirectAnswer { .. }
        ));
    }

    #[tokio::test]
    async fn serialize_yield_line_roundtrips() {
        let y = EngineYield::ToolResult {
            turn_id: "t1".into(),
            call_id: "c1".into(),
            tool_name: "shell".into(),
            output: oneai_core::ToolOutput {
                success: true,
                content: "OK".into(),
                error: None,
                ..Default::default()
            },
        };
        let line = oneai_bus::serialize_yield(&y).unwrap();
        let back = oneai_bus::parse_yield(line.trim()).unwrap();
        match back {
            EngineYield::ToolResult { call_id, .. } => assert_eq!(call_id, "c1"),
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn test_session_registration() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let state = StudioState::new_default();

        rt.block_on(state.register_session(SessionView {
            id: "sess_1".to_string(),
            paradigm: "react".to_string(),
            iteration: 0,
            running: true,
            total_tokens: 0,
        }));

        let sessions = rt.block_on(state.list_sessions());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "sess_1");
    }

    #[test]
    fn test_session_update() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let state = StudioState::new_default();

        rt.block_on(state.register_session(SessionView {
            id: "sess_1".to_string(),
            paradigm: "react".to_string(),
            iteration: 0,
            running: true,
            total_tokens: 0,
        }));

        rt.block_on(state.update_session("sess_1", SessionUpdate::Iteration(5)));
        rt.block_on(state.update_session("sess_1", SessionUpdate::Tokens(1200)));

        let session = rt.block_on(state.get_session("sess_1")).unwrap();
        assert_eq!(session.iteration, 5);
        assert_eq!(session.total_tokens, 1200);
    }
}
