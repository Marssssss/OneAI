//! # oneai-app-server — JSON-RPC 2.0 frontend protocol layer over the engine bus.
//!
//! Non-Rust frontends (IDE plugins, web/JS, native macOS-Swift / Windows-C#)
//! all speak one JSON-RPC 2.0 protocol to the engine, over any of
//! stdio / unix-socket / named-pipe / WebSocket. This crate is the adapter:
//! it maps JSON-RPC methods to [`Directive`](oneai_bus::Directive)s on the
//! inbound side and [`EngineYield`](oneai_bus::EngineYield)s to JSON-RPC
//! `event` notifications on the outbound side. The engine is unaware of who
//! is on the other end — it sees the same bus whether the frontend is
//! in-process (TUI) or out-of-process (this crate).
//!
//! ## Layering
//!
//! - **L3 bus** (unchanged): `Directive`/`EngineYield` newline-JSON +
//!   `InProcessBus` (`oneai-bus`). Internal canonical protocol; the TUI
//!   connects in-process, skipping this crate entirely (zero serialization).
//! - **L2 adapter** (this crate): JSON-RPC 2.0 frontend schema ↔
//!   Directive/EngineYield. See [`adapter`](mod@crate::adapter).
//! - **L1 transports** (this crate): stdio / ipc / ws. See
//!   [`transport`](mod@crate::transport).
//! - **L4 engine** (unchanged): `spawn_directive_pump` → AgentLoop. Built by
//!   the CLI (`oneai app-server`), not this crate — the crate takes an already
//!   built `Arc<InProcessBus>`.
//!
//! ## Process topology
//!
//! `oneai app-server --listen stdio --listen ipc://~/.oneai/app-server.sock
//! --listen ws://127.0.0.1:8787` binds all three concurrently; one engine
//! process feeds every frontend class (crash isolation + single binary +
//! signature decoupling). See `docs/app-server-mechanism.md`.
//!
//! ## Stability
//!
//! The outbound surface is a single `event` notification whose `params` is the
//! full `EngineYield` value (with its `kind` tag). New yield variants (the
//! bus enums are `#[non_exhaustive]`) arrive as unknown `kind`s a frontend
//! ignores, so the protocol grows with the bus without breaking old
//! frontends.

#![forbid(unsafe_code)]

pub mod adapter;
pub mod conversation;
pub mod dialog;
pub mod dispatcher;
pub mod feedback;
pub mod host_allowlist;
#[cfg(feature = "http")]
pub mod http;
pub mod probe;
pub mod protocol;
pub mod scenario;
pub mod transport;

pub use conversation::{ConversationStore, InMemoryConversationStore, SharedConversationStore};
pub use dispatcher::Dispatcher;
pub use feedback::{
    FeedbackEntry, FeedbackStore, InMemoryFeedbackStore, SharedFeedbackStore, KIND_DOWN, KIND_NOTE,
    KIND_UP,
};
pub use host_allowlist::{
    HostAllowEntry, HostAllowlistRpc, InMemoryHostAllowlistRpc, SharedHostAllowlistRpc,
};
#[cfg(feature = "http")]
pub use http::serve_web;
pub use probe::{
    AppConfigSnapshot, AppProbe, ConfigFileView, DomainPackInfo, DomainPackList, NullAppProbe,
    ProviderEntryDto, ProviderInfo, ProviderOpResult, SharedAppProbe, SkillInfo, SkillOpResult,
};
pub use protocol::{Notification, Request, Response, RpcError};
pub use scenario::{
    default_scenarios_path, FileScenarioStore, InMemoryScenarioStore, ScenarioStore,
};

/// A shared, thread-safe handle to the scenario library threaded through
/// `serve_all` → transports → `serve_connection` → `handle_request` for the
/// `scenario/*` methods. `Arc<dyn ScenarioStore + Send + Sync>` — object-safe
/// via `#[async_trait]`.
pub type SharedScenarioStore = Arc<dyn ScenarioStore + Send + Sync>;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::task::JoinHandle;

use oneai_bus::{EngineBus, InProcessBus};

use thiserror::Error;

/// Crate-local result alias.
pub type Result<T> = std::result::Result<T, AppServerError>;

/// Errors raised by the app-server (transport binding / IO).
#[derive(Debug, Error)]
pub enum AppServerError {
    #[error("transport bind error: {0}")]
    Bind(String),
    #[error("invalid listen spec: {0}")]
    InvalidSpec(String),
}

impl From<std::io::Error> for AppServerError {
    fn from(e: std::io::Error) -> Self {
        Self::Bind(e.to_string())
    }
}

/// One transport endpoint to listen on. Parsed from scheme-prefixed strings by
/// [`ListenSpec::parse`]; passed to [`serve_all`].
#[derive(Debug, Clone)]
pub enum ListenSpec {
    /// `stdio` — the spawning process's stdin/stdout (IDE LSP-style spawn).
    /// Exactly one connection; no accept loop.
    Stdio,
    /// `ipc://path` (also accepts `unix://` / `pipe://`) — a Unix domain
    /// socket (Unix) or Windows named pipe (Windows) via
    /// `oneai-supervisor::IpcListener`.
    Ipc(PathBuf),
    /// `ws://host:port` — a WebSocket listener for browser/JS frontends
    /// (feature `ws`).
    Ws(SocketAddr),
    /// `native-messaging` (also `nm`) — stdin/stdout framed with a 4-byte
    /// little-endian length prefix, the Chrome/Firefox native-messaging wire
    /// format. A browser extension connects via `chrome.runtime.connectNative`;
    /// the browser spawns this process as the host. stdout is the message
    /// stream (no banners). Exactly one connection; no accept loop.
    NativeMessaging,
}

impl ListenSpec {
    /// Parse a scheme-prefixed listen string. Accepted forms:
    /// - `stdio`
    /// - `native-messaging`, `nm`
    /// - `ipc://<path>`, `unix://<path>`, `pipe://<path>`
    /// - `ws://<host>:<port>` (feature `ws`)
    ///
    /// `~` in an ipc path is expanded to the home directory.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("stdio") {
            return Ok(Self::Stdio);
        }
        if spec.eq_ignore_ascii_case("native-messaging") || spec.eq_ignore_ascii_case("nm") {
            return Ok(Self::NativeMessaging);
        }
        if let Some(rest) = spec
            .strip_prefix("ipc://")
            .or_else(|| spec.strip_prefix("unix://"))
            .or_else(|| spec.strip_prefix("pipe://"))
        {
            let expanded = expand_tilde(rest);
            return Ok(Self::Ipc(PathBuf::from(expanded)));
        }
        #[cfg(feature = "ws")]
        if let Some(rest) = spec.strip_prefix("ws://") {
            let addr: SocketAddr = rest
                .parse()
                .map_err(|e| AppServerError::InvalidSpec(format!("ws:// addr '{rest}': {e}")))?;
            return Ok(Self::Ws(addr));
        }
        #[cfg(not(feature = "ws"))]
        if spec.strip_prefix("ws://").is_some() {
            return Err(AppServerError::InvalidSpec(
                "ws:// transport requires the `ws` feature".into(),
            ));
        }
        Err(AppServerError::InvalidSpec(format!(
            "unknown listen spec: {spec} (expected stdio | native-messaging | ipc://<path> | ws://<host>:<port>)"
        )))
    }
}

/// Expand a leading `~` to the home directory (no-op otherwise).
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "~".to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// Default IPC socket path — `~/.oneai/app-server.sock` (separate from the
/// supervisor's `server.sock` and the sidecar's `serve.sock`).
pub fn default_ipc_socket() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".oneai")
        .join("app-server.sock")
}

/// Bind all given transports concurrently against one engine bus. Spawns the
/// shared [`Dispatcher`] yield-consumer, one listener task per spec, and
/// returns a single [`JoinHandle`] that completes only when every listener
/// task ends (i.e. never, under normal operation — abort it on shutdown).
///
/// The caller is expected to wrap this in a tokio runtime and abort the
/// returned handle on Ctrl-C / parent disconnect. The engine (`Arc<InProcessBus>`
/// + `spawn_directive_pump`) must already be built and driving the bus.
pub async fn serve_all(
    specs: Vec<ListenSpec>,
    bus: Arc<InProcessBus>,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    feedback_store: SharedFeedbackStore,
    host_allowlist_rpc: SharedHostAllowlistRpc,
    probe: SharedAppProbe,
) -> Result<JoinHandle<()>> {
    if specs.is_empty() {
        return Err(AppServerError::InvalidSpec(
            "no --listen specs given".into(),
        ));
    }

    // The shared, process-wide dispatcher: one yield consumer resolves all
    // blocking-ack requests across all connections/transports. Subscribe BEFORE
    // spawning so the receiver exists before any yield is emitted.
    let dispatcher = Dispatcher::default();
    let yield_rx = bus.subscribe_yields();
    tokio::spawn(dispatcher.clone().run(yield_rx));

    let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(specs.len());
    for spec in specs {
        let bus = bus.clone();
        let dispatcher = dispatcher.clone();
        let scenario_store = scenario_store.clone();
        let session_store = session_store.clone();
        let feedback_store = feedback_store.clone();
        let host_allowlist_rpc = host_allowlist_rpc.clone();
        let probe = probe.clone();
        let handle = match spec {
            ListenSpec::Stdio => {
                // stdio is a single pre-connected stream; serve_stdio spawns
                // the stdin/stdout pumps + serve_connection and returns the
                // serve task handle.
                transport::serve_stdio(
                    bus,
                    dispatcher,
                    scenario_store,
                    session_store,
                    feedback_store,
                    host_allowlist_rpc,
                    probe,
                )
            }
            ListenSpec::Ipc(path) => {
                let h = transport::serve_ipc(
                    &path,
                    bus,
                    dispatcher,
                    scenario_store,
                    session_store,
                    feedback_store,
                    host_allowlist_rpc,
                    probe,
                )
                .await?;
                h
            }
            ListenSpec::Ws(addr) => {
                #[cfg(feature = "ws")]
                {
                    let (h, _bound) = transport::serve_ws(
                        addr,
                        bus,
                        dispatcher,
                        scenario_store,
                        session_store,
                        feedback_store,
                        host_allowlist_rpc,
                        probe,
                    )
                    .await?;
                    h
                }
                #[cfg(not(feature = "ws"))]
                {
                    let _ = (
                        addr,
                        bus,
                        dispatcher,
                        scenario_store,
                        session_store,
                        feedback_store,
                        host_allowlist_rpc,
                        probe,
                    );
                    return Err(AppServerError::InvalidSpec(
                        "ws:// transport requires the `ws` feature".into(),
                    ));
                }
            }
            ListenSpec::NativeMessaging => transport::serve_native_messaging(
                bus,
                dispatcher,
                scenario_store,
                session_store,
                feedback_store,
                host_allowlist_rpc,
                probe,
            ),
        };
        handles.push(handle);
    }

    // Supervisor: never returns under normal operation (listeners loop
    // forever). Awaiting it blocks until all listener tasks end.
    Ok(tokio::spawn(async move {
        let mut all = futures::future::join_all(handles).await;
        let _ = &mut all;
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stdio_case_insensitive() {
        assert!(matches!(
            ListenSpec::parse("stdio").unwrap(),
            ListenSpec::Stdio
        ));
        assert!(matches!(
            ListenSpec::parse("STDIO").unwrap(),
            ListenSpec::Stdio
        ));
    }

    #[test]
    fn parse_native_messaging_aliases() {
        assert!(matches!(
            ListenSpec::parse("native-messaging").unwrap(),
            ListenSpec::NativeMessaging
        ));
        assert!(matches!(
            ListenSpec::parse("NM").unwrap(),
            ListenSpec::NativeMessaging
        ));
    }

    #[test]
    fn parse_ipc_schemes() {
        let s = ListenSpec::parse("ipc:///tmp/x.sock").unwrap();
        assert!(matches!(s, ListenSpec::Ipc(p) if *p == *"/tmp/x.sock"));
        let s = ListenSpec::parse("unix:///tmp/y.sock").unwrap();
        assert!(matches!(s, ListenSpec::Ipc(p) if *p == *"/tmp/y.sock"));
        let s = ListenSpec::parse("pipe://\\\\.\\pipe\\oneai").unwrap();
        assert!(matches!(s, ListenSpec::Ipc(_)));
    }

    #[cfg(feature = "ws")]
    #[test]
    fn parse_ws() {
        let s = ListenSpec::parse("ws://127.0.0.1:8787").unwrap();
        assert!(matches!(s, ListenSpec::Ws(a) if a.port() == 8787));
    }

    #[test]
    fn parse_bad_spec_errors() {
        assert!(ListenSpec::parse("ftp://nope").is_err());
        assert!(ListenSpec::parse("ws://not-an-addr").is_err());
    }

    #[test]
    fn expand_tilde_home() {
        let p = expand_tilde("~/oneai");
        assert!(!p.starts_with('~'));
        assert!(p.ends_with("oneai"));
        // Bare path unchanged.
        assert_eq!(expand_tilde("/tmp/x"), "/tmp/x");
    }
}

#[cfg(test)]
mod integration {
    //! Adapter↔bus integration over mpsc channels (the test acts as the
    //! transport: it writes raw JSON-RPC messages into `inbound_tx` and reads
    //! them off `outbound_rx`). A fake driver drains the bus's directive stream
    //! and emits yields — the same shape as `oneai-bus`'s `serve.rs` test
    //! harness, but driving [`serve_connection`] instead of `bridge_connection`.
    use super::{
        adapter::serve_connection,
        conversation::{InMemoryConversationStore, SharedConversationStore},
        dispatcher::Dispatcher,
        feedback::{InMemoryFeedbackStore, SharedFeedbackStore},
        host_allowlist::{InMemoryHostAllowlistRpc, SharedHostAllowlistRpc},
        protocol::{method, Request, Response},
        scenario::{builtin_presets, InMemoryScenarioStore},
        serve_all, NullAppProbe, SharedScenarioStore,
    };
    use oneai_bus::{
        BusParadigmKind, BusTurnSummary, Directive, EngineBus, EngineYield, InProcessBus,
    };
    use oneai_core::{ContentBlock, InteractionRequest, SessionInfo};
    use serde_json::{json, Value};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Fake engine driver: drains forwarded directives and emits yields. On a
    /// `UserMessage`, emits `TurnStart` → `StreamChunk` → `TurnComplete`. When
    /// `needs_approval` is set, calls `bus.request_approval` (which emits
    /// `ApprovalRequest` and blocks until the matching `Approve` arrives).
    fn spawn_fake_driver(
        mut directive_rx: mpsc::Receiver<Directive>,
        bus: std::sync::Arc<InProcessBus>,
        needs_approval: bool,
    ) -> (tokio::task::JoinHandle<()>, CancellationToken) {
        let token = CancellationToken::new();
        let registered = token.clone();
        let h = tokio::spawn(async move {
            while let Some(d) = directive_rx.recv().await {
                match d {
                    Directive::UserMessage { content } => {
                        let task: String = content
                            .into_iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        let turn_id = "t1".to_string();
                        bus.register_interrupt(registered.clone());
                        let _ = bus.emit(EngineYield::TurnStart {
                            turn_id: turn_id.clone(),
                            task: task.clone(),
                        });
                        if needs_approval {
                            let resp = bus
                                .request_approval(InteractionRequest::NetworkApproval {
                                    host: "example.com".to_string(),
                                    requested_by: "test".to_string(),
                                })
                                .await
                                .expect("approval resolved");
                            let _ = bus.emit(EngineYield::DirectAnswer {
                                turn_id: turn_id.clone(),
                                text: format!("{resp:?}"),
                                speaker: None,
                            });
                        } else {
                            let _ = bus.emit(EngineYield::StreamChunk {
                                turn_id: turn_id.clone(),
                                text: format!("echo: {task}"),
                                speaker: None,
                            });
                        }
                        let _ = bus.emit(EngineYield::TurnComplete {
                            turn_id,
                            summary: BusTurnSummary {
                                final_answer: format!("echo: {task}"),
                                iterations: 1,
                                completed: true,
                                active_paradigm: BusParadigmKind::ReAct,
                            },
                        });
                    }
                    // Session lifecycle: emit the corresponding yield. Because
                    // this runs AFTER the directive pump forwarded the
                    // directive (which is AFTER the adapter's register-before-
                    // submit), the dispatcher's pending is already queued — no
                    // race. Lets session/create etc. resolve naturally.
                    Directive::CreateSession { id, .. } => {
                        let sid = id.unwrap_or_else(|| "auto".into());
                        let _ = bus.emit(EngineYield::SessionCreated { id: sid });
                    }
                    Directive::ClearSession => {
                        let _ = bus.emit(EngineYield::SessionCleared {
                            id: "cleared".into(),
                        });
                    }
                    Directive::DeleteSession { id } => {
                        let _ = bus.emit(EngineYield::SessionDeleted { id });
                    }
                    // Other directives are no-ops in this stub.
                    _ => {}
                }
            }
        });
        (h, token)
    }

    /// Wire a fake engine + the adapter over mpsc channels. Returns the
    /// inbound channel, the dispatcher task handle (abort on drop), and the
    /// driver's interrupt token (registered into the bus at TurnStart).
    /// Outbound messages are buffered into `collected` so a test can assert
    /// against them out-of-order without dropping interleaved notifications.
    struct Harness {
        inbound_tx: mpsc::Sender<String>,
        collected: Vec<Value>,
        outbound_rx: mpsc::Receiver<String>,
        _disp: tokio::task::JoinHandle<()>,
        interrupt_token: CancellationToken,
    }

    impl Harness {
        async fn send_req(&mut self, req: Request) {
            self.inbound_tx
                .send(serde_json::to_string(&req).unwrap())
                .await
                .unwrap();
        }

        async fn send_raw(&mut self, line: &str) {
            self.inbound_tx.send(line.to_string()).await.unwrap();
        }

        /// Drain outbound into `collected` and return the first buffered
        /// JSON-RPC response matching `id` (removing it from the buffer).
        async fn wait_for_response(&mut self, id: &Value) -> Response {
            loop {
                for i in 0..self.collected.len() {
                    let is_resp = self.collected[i].get("method").is_none();
                    if is_resp {
                        let v = self.collected[i].clone();
                        if let Ok(r) = serde_json::from_value::<Response>(v) {
                            if &r.id == id {
                                self.collected.remove(i);
                                return r;
                            }
                        }
                    }
                }
                let line = self.outbound_rx.recv().await.expect("outbound closed");
                let v: Value = serde_json::from_str(&line).expect("valid json");
                self.collected.push(v);
            }
        }

        /// Drain outbound into `collected` and return the first buffered
        /// `event` notification matching `pred` (removing it from the buffer).
        async fn wait_for_event<F: Fn(&Value) -> bool>(&mut self, pred: F) -> Value {
            loop {
                for i in 0..self.collected.len() {
                    let is_event = self.collected[i].get("method").and_then(|m| m.as_str())
                        == Some(method::EVENT);
                    if is_event && pred(&self.collected[i]) {
                        return self.collected.remove(i);
                    }
                }
                let line = self.outbound_rx.recv().await.expect("outbound closed");
                let v: Value = serde_json::from_str(&line).expect("valid json");
                self.collected.push(v);
            }
        }
    }

    fn harness(needs_approval: bool) -> (std::sync::Arc<InProcessBus>, Harness) {
        harness_with(
            needs_approval,
            std::sync::Arc::new(InMemoryConversationStore::new()),
        )
    }

    fn harness_with(
        needs_approval: bool,
        session_store: SharedConversationStore,
    ) -> (std::sync::Arc<InProcessBus>, Harness) {
        let (bus, directive_rx) = InProcessBus::new();
        let bus = std::sync::Arc::new(bus);
        let (_driver, interrupt_token) =
            spawn_fake_driver(directive_rx, bus.clone(), needs_approval);
        let dispatcher = Dispatcher::default();
        let yield_rx = bus.subscribe_yields();
        let disp = tokio::spawn(dispatcher.clone().run(yield_rx));
        let (inbound_tx, inbound_rx) = mpsc::channel(64);
        let (outbound_tx, outbound_rx) = mpsc::channel(64);
        let scenario_store: SharedScenarioStore =
            std::sync::Arc::new(InMemoryScenarioStore::from_seed(builtin_presets()));
        let feedback_store: SharedFeedbackStore = std::sync::Arc::new(InMemoryFeedbackStore::new());
        let host_allowlist_rpc: SharedHostAllowlistRpc =
            std::sync::Arc::new(InMemoryHostAllowlistRpc::new());
        tokio::spawn(serve_connection(
            bus.clone(),
            dispatcher,
            scenario_store,
            session_store,
            feedback_store,
            host_allowlist_rpc,
            std::sync::Arc::new(NullAppProbe),
            inbound_rx,
            outbound_tx,
        ));
        (
            bus,
            Harness {
                inbound_tx,
                collected: Vec::new(),
                outbound_rx,
                _disp: disp,
                interrupt_token,
            },
        )
    }

    #[tokio::test]
    async fn turn_run_streams_events_and_returns_turn_id() {
        let (_bus, mut h) = harness(false);
        let req = Request::new(
            json!(1),
            method::TURN_RUN,
            json!({"content": [{"type": "text", "text": "hello"}]}),
        );
        h.send_req(req).await;
        let resp = h.wait_for_response(&json!(1)).await;
        let result = resp.result.expect("ok response");
        assert_eq!(result["turn_id"], "t1");
        // An event(turn_start) notification must also have been forwarded.
        let ev = h
            .wait_for_event(|v| v["params"]["kind"] == "turn_start")
            .await;
        assert_eq!(ev["params"]["turn_id"], "t1");
        // And a stream_chunk + turn_complete arrive.
        let _ = h
            .wait_for_event(|v| v["params"]["kind"] == "stream_chunk")
            .await;
        let _ = h
            .wait_for_event(|v| v["params"]["kind"] == "turn_complete")
            .await;
    }

    #[tokio::test]
    async fn approval_roundtrip_resolves_request() {
        let (_bus, mut h) = harness(true);
        let req = Request::new(
            json!(1),
            method::TURN_RUN,
            json!({"content": [{"type": "text", "text": "go"}]}),
        );
        h.send_req(req).await;
        // Engine calls request_approval → event(approval_request) with request_id.
        let ev = h
            .wait_for_event(|v| v["params"]["kind"] == "approval_request")
            .await;
        let request_id = ev["params"]["request_id"].as_str().unwrap().to_string();
        // Frontend responds.
        let resp_req = Request::new(
            json!(2),
            method::APPROVAL_RESPOND,
            json!({"request_id": request_id, "response": "Proceed"}),
        );
        h.send_req(resp_req).await;
        let ack = h.wait_for_response(&json!(2)).await;
        assert_eq!(ack.result.unwrap()["ok"], true);
        // Driver's request_approval resolves → it emits DirectAnswer.
        let _ = h
            .wait_for_event(|v| v["params"]["kind"] == "direct_answer")
            .await;
    }

    #[tokio::test]
    async fn turn_cancel_fires_registered_token() {
        let (_bus, mut h) = harness(false);
        // turn/run first — the driver registers an interrupt token at TurnStart.
        let req = Request::new(
            json!(1),
            method::TURN_RUN,
            json!({"content": [{"type": "text", "text": "hi"}]}),
        );
        h.send_req(req).await;
        let _ = h.wait_for_response(&json!(1)).await;
        // Wait for TurnStart so the driver has registered its token.
        let _ = h
            .wait_for_event(|v| v["params"]["kind"] == "turn_start")
            .await;
        // Now send turn/cancel — the bus fires the registered token.
        let cancel = Request::new(
            json!(2),
            method::TURN_CANCEL,
            json!({"reason": {"Custom": {"reason": "user_stop"}}}),
        );
        h.send_req(cancel).await;
        let ack = h.wait_for_response(&json!(2)).await;
        assert_eq!(ack.result.unwrap()["ok"], true);
        assert!(
            h.interrupt_token.is_cancelled(),
            "turn/cancel must fire the driver's registered interrupt token"
        );
    }

    #[tokio::test]
    async fn session_create_returns_id() {
        let (_bus, mut h) = harness(false);
        // The fake driver emits SessionCreated after the pump forwards
        // CreateSession (which the adapter submits AFTER register-before-
        // submit), so the dispatcher's pending is queued first — no race.
        let req = Request::new(json!(7), method::SESSION_CREATE, json!({"id": "s1"}));
        h.send_req(req).await;
        let resp = h.wait_for_response(&json!(7)).await;
        assert_eq!(resp.result.unwrap()["id"], "s1");
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let (_bus, mut h) = harness(false);
        let req = Request::new(json!(9), "no/such", json!({}));
        h.send_req(req).await;
        let resp = h.wait_for_response(&json!(9)).await;
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn malformed_inbound_is_parse_error() {
        let (_bus, mut h) = harness(false);
        h.send_raw("{ not json").await;
        let resp = h.wait_for_response(&Value::Null).await;
        assert_eq!(resp.error.unwrap().code, -32700);
    }

    /// `serve_all` with an empty spec list is an error (nothing to listen on).
    #[tokio::test]
    async fn serve_all_empty_specs_errors() {
        let (bus, _rx) = InProcessBus::new();
        let bus = std::sync::Arc::new(bus);
        let store: SharedScenarioStore = std::sync::Arc::new(InMemoryScenarioStore::new());
        let sessions: SharedConversationStore =
            std::sync::Arc::new(InMemoryConversationStore::new());
        let feedback: SharedFeedbackStore = std::sync::Arc::new(InMemoryFeedbackStore::new());
        let host_allowlist_rpc: SharedHostAllowlistRpc =
            std::sync::Arc::new(InMemoryHostAllowlistRpc::new());
        let err = serve_all(
            vec![],
            bus,
            store,
            sessions,
            feedback,
            host_allowlist_rpc,
            std::sync::Arc::new(NullAppProbe),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, super::AppServerError::InvalidSpec(_)));
    }

    /// End-to-end over a real WebSocket: bind `serve_ws` on an ephemeral port,
    /// connect a `tokio-tungstenite` client, send `turn/run`, and read back the
    /// `event(turn_start)` notification. Exercises the ws transport framing +
    /// handshake + the full adapter path.
    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn ws_transport_roundtrips_turn_run() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (bus, directive_rx) = InProcessBus::new();
        let bus = std::sync::Arc::new(bus);
        let _driver = spawn_fake_driver(directive_rx, bus.clone(), false);
        let dispatcher = Dispatcher::default();
        let yield_rx = bus.subscribe_yields();
        let _disp = tokio::spawn(dispatcher.clone().run(yield_rx));

        let scenario_store: SharedScenarioStore =
            std::sync::Arc::new(InMemoryScenarioStore::from_seed(builtin_presets()));
        let session_store: SharedConversationStore =
            std::sync::Arc::new(InMemoryConversationStore::new());
        let feedback_store: SharedFeedbackStore = std::sync::Arc::new(InMemoryFeedbackStore::new());
        let host_allowlist_rpc: SharedHostAllowlistRpc =
            std::sync::Arc::new(InMemoryHostAllowlistRpc::new());
        let (_handle, bound) = super::transport::serve_ws(
            "127.0.0.1:0".parse().unwrap(),
            bus.clone(),
            dispatcher.clone(),
            scenario_store,
            session_store,
            feedback_store,
            host_allowlist_rpc,
            std::sync::Arc::new(NullAppProbe),
        )
        .await
        .expect("ws bind");

        let url = format!("ws://{bound}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .expect("ws connect");
        let req = Request::new(
            json!(1),
            method::TURN_RUN,
            json!({"content": [{"type": "text", "text": "ws hello"}]}),
        );
        ws.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
            .await
            .unwrap();

        // Read messages until we see the event(turn_start) notification.
        let mut saw_turn_start = false;
        let mut saw_response = false;
        // Bound the loop so a regression doesn't hang the test forever.
        for _ in 0..200 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let s = t.to_string();
                    let val: Value = match serde_json::from_str(&s) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if val.get("method").and_then(|m| m.as_str()) == Some(method::EVENT)
                        && val["params"]["kind"] == "turn_start"
                    {
                        saw_turn_start = true;
                    }
                    if val.get("result").is_some() && val["id"] == json!(1) {
                        assert_eq!(val["result"]["turn_id"], "t1");
                        saw_response = true;
                    }
                    if saw_turn_start && saw_response {
                        return;
                    }
                }
                _ => break,
            }
        }
        panic!(
            "did not observe turn_start event + turn_id response over ws \
             (saw_turn_start={saw_turn_start}, saw_response={saw_response})"
        );
    }

    // ── session/list over the full adapter path ─────────────────────────

    /// `session/list` returns the seeded conversations with the epoch-millis
    /// shape the FFI `SessionInfoView` exposes, so a foreign UI decodes one
    /// struct regardless of transport.
    #[tokio::test]
    async fn session_list_returns_seeded_sessions_with_millis_shape() {
        let now = chrono::Utc::now();
        let seed = vec![
            SessionInfo::with_title("s1".into(), now, now, 3, Some("first user msg".into())),
            SessionInfo::new("s2".into(), now, now, 0),
        ];
        let (_bus, mut h) = harness_with(
            false,
            std::sync::Arc::new(InMemoryConversationStore::from_seed(seed)),
        );
        h.send_req(Request::new(json!(20), method::SESSION_LIST, json!({})))
            .await;
        let resp = h.wait_for_response(&json!(20)).await;
        let result = resp.result.expect("ok response");
        let sessions = result["sessions"].as_array().expect("sessions array");
        assert_eq!(sessions.len(), 2);
        // Epoch-millis fields present (the FFI SessionInfoView shape).
        assert_eq!(sessions[0]["id"], "s1");
        assert!(sessions[0]["created_at_ms"].as_i64().is_some());
        assert!(sessions[0]["updated_at_ms"].as_i64().is_some());
        assert_eq!(sessions[0]["message_count"], 3);
        assert_eq!(sessions[0]["title"], "first user msg");
        assert_eq!(sessions[1]["id"], "s2");
        assert!(sessions[1]["title"].is_null());
    }

    // ── scenario/* over the full adapter path ────────────────────────────

    /// A minimal valid scenario for upsert/validate tests.
    fn valid_scenario_json(id: &str) -> Value {
        json!({
            "id": id,
            "name": "Custom",
            "members": [{
                "id": "a", "name": "A", "system_prompt": "p", "kind": "openai", "model": ""
            }],
            "turn_policy": "roundrobin"
        })
    }

    #[tokio::test]
    async fn scenario_list_returns_seeded_presets() {
        let (_bus, mut h) = harness(false);
        h.send_req(Request::new(json!(10), method::SCENARIO_LIST, json!({})))
            .await;
        let resp = h.wait_for_response(&json!(10)).await;
        let result = resp.result.unwrap();
        let scenarios = result["scenarios"].as_array().unwrap();
        assert!(scenarios.len() >= 2, "seeded presets present");
        assert!(scenarios.iter().any(|s| s["id"] == "preset-interview"));
        assert!(scenarios
            .iter()
            .any(|s| s["id"] == "preset-writing-workshop"));
    }

    #[tokio::test]
    async fn scenario_validate_rejects_empty_members() {
        let (_bus, mut h) = harness(false);
        let bad = json!({
            "id": "bad", "name": "Bad", "members": [],
            "turn_policy": "roundrobin"
        });
        h.send_req(Request::new(
            json!(11),
            method::SCENARIO_VALIDATE,
            json!({"scenario": bad}),
        ))
        .await;
        let resp = h.wait_for_response(&json!(11)).await;
        let result = resp.result.unwrap();
        assert_eq!(result["ok"], false);
        let errs = result["errors"].as_array().unwrap();
        assert!(errs
            .iter()
            .any(|e| e["field"] == "members" && e["code"] == "empty"));
    }

    #[tokio::test]
    async fn scenario_upsert_then_get_roundtrip() {
        let (_bus, mut h) = harness(false);
        h.send_req(Request::new(
            json!(12),
            method::SCENARIO_UPSERT,
            json!({"scenario": valid_scenario_json("custom-1")}),
        ))
        .await;
        let resp = h.wait_for_response(&json!(12)).await;
        assert_eq!(resp.result.unwrap()["ok"], true);

        h.send_req(Request::new(
            json!(13),
            method::SCENARIO_GET,
            json!({"id": "custom-1"}),
        ))
        .await;
        let resp = h.wait_for_response(&json!(13)).await;
        let result = resp.result.unwrap();
        assert_eq!(result["id"], "custom-1");
        assert_eq!(result["members"][0]["id"], "a");
    }

    #[tokio::test]
    async fn scenario_upsert_rejects_invalid_without_storing() {
        let (_bus, mut h) = harness(false);
        let bad = json!({
            "id": "bad-1", "name": "Bad",
            "members": [{"id": "a", "name": "", "system_prompt": "", "kind": "openai", "model": ""}],
            "turn_policy": "scripted",
            "script_order": ["a", "ghost"]
        });
        h.send_req(Request::new(
            json!(14),
            method::SCENARIO_UPSERT,
            json!({"scenario": bad}),
        ))
        .await;
        let resp = h.wait_for_response(&json!(14)).await;
        let result = resp.result.unwrap();
        assert_eq!(result["ok"], false, "invalid scenario is not stored");
        // Must NOT be retrievable — the store rejected it.
        h.send_req(Request::new(
            json!(15),
            method::SCENARIO_GET,
            json!({"id": "bad-1"}),
        ))
        .await;
        let resp = h.wait_for_response(&json!(15)).await;
        // Not found ⇒ JSON-RPC error (-32602), not a result.
        assert!(resp.error.is_some(), "rejected scenario was not stored");
    }

    #[tokio::test]
    async fn scenario_delete_removes_preset() {
        let (_bus, mut h) = harness(false);
        h.send_req(Request::new(
            json!(16),
            method::SCENARIO_DELETE,
            json!({"id": "preset-interview"}),
        ))
        .await;
        let resp = h.wait_for_response(&json!(16)).await;
        assert_eq!(resp.result.unwrap()["ok"], true);
        // Confirm it's gone from list.
        h.send_req(Request::new(json!(17), method::SCENARIO_LIST, json!({})))
            .await;
        let resp = h.wait_for_response(&json!(17)).await;
        let result = resp.result.unwrap();
        let scenarios = result["scenarios"].as_array().unwrap();
        assert!(!scenarios.iter().any(|s| s["id"] == "preset-interview"));
    }
}
