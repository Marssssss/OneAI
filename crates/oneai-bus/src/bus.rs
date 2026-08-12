//! The engine bus trait and its in-process implementation.
//!
//! The bus is the single seam between the engine and any frontend. A frontend
//! holds a shared `Arc<InProcessBus>` (cheap to clone), submits [`Directive`]s,
//! and reads [`EngineYield`]s off a broadcast receiver. The engine side — the
//! `AgentLoop` driver (wired in `oneai-agent`, Phase 1) — reads user directives
//! off the `mpsc::Receiver` returned by [`InProcessBus::new`], emits yields via
//! [`EngineBus::emit`], and asks for approvals via [`EngineBus::request_approval`].
//!
//! Two channels (matching codex's submission/event pair, here directive/yield):
//!
//! - `directive` — `mpsc::Sender<Directive>` (bounded 512) for user directives
//!   forwarded to the engine driver. Control directives (`Approve`/`Interrupt`)
//!   are NOT forwarded; the bus resolves them itself.
//! - `yield` — `broadcast::Sender<EngineYield>` (1024) for outbound yields.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use oneai_core::{InteractionRequest, InteractionResponse};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::protocol::{Directive, EngineYield};
use crate::{BusError, Result};

/// Directive channel capacity (bounded — back-pressures a frontend that
/// out-submits the engine; matches codex's submission queue size).
const DIRECTIVE_CAPACITY: usize = 512;
/// Yield channel capacity (broadcast — multiple subscribers, lagging
/// subscribers miss events with `RecvError::Lagged`). Codex uses unbounded;
/// OneAI caps to bound memory under high-frequency streaming.
const YIELD_CAPACITY: usize = 1024;

/// The engine bus — the single seam between engine and frontends.
///
/// All methods take `&self` so a shared `Arc<dyn EngineBus>` / `Arc<InProcessBus>`
/// can be held by both the engine driver and every frontend clone.
#[async_trait]
pub trait EngineBus: Send + Sync {
    /// Submit a directive. Control directives (`Approve`, `Interrupt`) are
    /// resolved by the bus; user directives (`UserMessage`, `SwitchParadigm`,
    /// `Shutdown`) are forwarded to the engine driver's directive stream.
    async fn submit(&self, directive: Directive) -> Result<()>;

    /// Subscribe to the outbound yield stream. Each subscriber gets its own
    /// receiver; lagging subscribers see a `RecvError::Lagged`.
    fn subscribe_yields(&self) -> broadcast::Receiver<EngineYield>;

    /// Engine side: emit a yield to all subscribers.
    async fn emit(&self, y: EngineYield) -> Result<()>;

    /// Engine side: ask for an approval. Broadcasts
    /// [`EngineYield::ApprovalRequest`] with a fresh `request_id`, then blocks
    /// until the matching [`Directive::Approve`] resolves it. Returns the
    /// response (or [`BusError::Closed`] if the bus drops the pending reply).
    async fn request_approval(&self, req: InteractionRequest) -> Result<InteractionResponse>;

    /// Engine side: register the cancel token an incoming
    /// [`Directive::Interrupt`] should fire. The `AgentLoop` driver calls this
    /// at turn start with its `CancellationToken`.
    fn register_interrupt(&self, token: CancellationToken);
}

/// Pending approval: the one-shot the engine awaits, keyed by `request_id`.
type PendingApproval = oneshot::Sender<InteractionResponse>;

/// In-process bus — `mpsc` for directives + `broadcast` for yields + a pending
/// approval registry + the engine's interrupt token. The default bus for
/// in-process frontends (TUI) and the engine driver.
pub struct InProcessBus {
    directive_tx: mpsc::Sender<Directive>,
    yield_tx: broadcast::Sender<EngineYield>,
    pending_approvals: Mutex<HashMap<String, PendingApproval>>,
    next_request_id: AtomicU64,
    interrupt_token: Mutex<Option<CancellationToken>>,
}

impl InProcessBus {
    /// Create a new in-process bus plus the directive stream the engine driver
    /// reads. The bus holds the `Sender` (frontends `submit` into it); the
    /// driver holds the `Receiver`.
    pub fn new() -> (Self, mpsc::Receiver<Directive>) {
        Self::with_capacity(DIRECTIVE_CAPACITY, YIELD_CAPACITY)
    }

    /// Construct with explicit channel capacities (testing).
    pub fn with_capacity(dir_cap: usize, yield_cap: usize) -> (Self, mpsc::Receiver<Directive>) {
        let (directive_tx, directive_rx) = mpsc::channel(dir_cap);
        let (yield_tx, _) = broadcast::channel(yield_cap);
        let bus = Self {
            directive_tx,
            yield_tx,
            pending_approvals: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(0),
            interrupt_token: Mutex::new(None),
        };
        (bus, directive_rx)
    }

    fn alloc_request_id(&self) -> String {
        let n = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        format!("apr_{n}")
    }
}

impl Default for InProcessBus {
    fn default() -> Self {
        // Receiver is dropped immediately — only useful for tests that emit
        // yields without driving a turn. Prefer [`InProcessBus::new`].
        Self::new().0
    }
}

#[async_trait]
impl EngineBus for InProcessBus {
    async fn submit(&self, directive: Directive) -> Result<()> {
        match directive {
            Directive::Approve {
                request_id,
                response,
            } => {
                let tx = self
                    .pending_approvals
                    .lock()
                    .expect("pending_approvals poisoned")
                    .remove(&request_id)
                    .ok_or_else(|| {
                        BusError::NotAcceptable(format!(
                            "no pending approval for request_id={request_id}"
                        ))
                    })?;
                // Ignore send error: the requester already dropped (timed out / cancelled).
                let _ = tx.send(response);
                Ok(())
            }
            Directive::Interrupt { .. } => {
                if let Some(token) = self
                    .interrupt_token
                    .lock()
                    .expect("interrupt_token poisoned")
                    .as_ref()
                {
                    token.cancel();
                }
                Ok(())
            }
            // Forward user-facing directives to the engine driver.
            Directive::UserMessage { .. }
            | Directive::SwitchParadigm { .. }
            | Directive::Shutdown => self
                .directive_tx
                .send(directive)
                .await
                .map_err(|_| BusError::Closed),
        }
    }

    fn subscribe_yields(&self) -> broadcast::Receiver<EngineYield> {
        self.yield_tx.subscribe()
    }

    async fn emit(&self, y: EngineYield) -> Result<()> {
        // broadcast::send returns Err only when there are zero receivers — a
        // turn may legitimately run with no subscriber yet, so treat as no-op.
        let _ = self.yield_tx.send(y);
        Ok(())
    }

    async fn request_approval(&self, req: InteractionRequest) -> Result<InteractionResponse> {
        let request_id = self.alloc_request_id();
        let (tx, rx) = oneshot::channel();
        self.pending_approvals
            .lock()
            .expect("pending_approvals poisoned")
            .insert(request_id.clone(), tx);
        self.emit(EngineYield::ApprovalRequest {
            request_id,
            request: req,
        })
        .await?;
        rx.await.map_err(|_| BusError::Closed)
    }

    fn register_interrupt(&self, token: CancellationToken) {
        *self
            .interrupt_token
            .lock()
            .expect("interrupt_token poisoned") = Some(token);
    }
}
