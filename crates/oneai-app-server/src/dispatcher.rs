//! Dispatcher — resolves the app-server's *blocking-ack* JSON-RPC requests.
//!
//! Most inbound methods fire-and-ack (`bus.submit` then immediately respond
//! `{ok:true}`). But a handful have a *return value* that only materializes as a
//! later engine yield:
//!
//! | method | resolved by yield |
//! |---|---|
//! | `turn/run` | `EngineYield::TurnStart` (its `turn_id`) |
//! | `session/create` | `SessionCreated` |
//! | `session/load` | `SessionLoaded` |
//! | `session/clear` | `SessionCleared` |
//! | `session/delete` | `SessionDeleted` |
//! | `conversation/compact` | `CompactResult` |
//! | `project/init` | `InitResult` |
//!
//! The engine's directive pump is **serial** — directives are drained one at a
//! time off the bus's bounded `mpsc`, so yields for a given variant fire in the
//! same order the matching directives were submitted. The dispatcher exploits
//! this: one FIFO queue per variant, drained by a **single** yield consumer
//! task. When the consumer sees e.g. a `TurnStart`, it pops the head of
//! `pending_turns` and fulfills that request's oneshot with the yield value.
//!
//! The single consumer is essential: the bus is a `broadcast` (every
//! connection's yield forwarder sees every yield), so a per-connection
//! consumer would pop the same `TurnStart` N times. There is exactly one
//! `Dispatcher` per app-server process, shared across all
//! connections/transports.
//!
//! See `docs/app-server-mechanism.md`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};

use oneai_bus::EngineYield;

/// One pending blocking-ack request: the JSON-RPC request `id` (echoed back in
/// the response) and the oneshot that fulfills it when the matching yield
/// arrives. Dropped receivers (connection closed early) send-err silently.
struct Pending {
    /// The JSON-RPC request `id` (correlation; reserved for diagnostics).
    #[allow(dead_code)]
    id: Value,
    respond: oneshot::Sender<Value>,
}

struct State {
    pending_turns: VecDeque<Pending>,
    pending_session_create: VecDeque<Pending>,
    pending_session_load: VecDeque<Pending>,
    pending_session_clear: VecDeque<Pending>,
    pending_session_delete: VecDeque<Pending>,
    pending_compact: VecDeque<Pending>,
    pending_init: VecDeque<Pending>,
}

/// The shared, process-wide dispatcher. Cloneable (`Arc`) so every connection's
/// adapter registers pending requests on the same state; one `run` task drains
/// the bus and fulfills them.
#[derive(Clone)]
pub struct Dispatcher {
    state: Arc<Mutex<State>>,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                pending_turns: VecDeque::new(),
                pending_session_create: VecDeque::new(),
                pending_session_load: VecDeque::new(),
                pending_session_clear: VecDeque::new(),
                pending_session_delete: VecDeque::new(),
                pending_compact: VecDeque::new(),
                pending_init: VecDeque::new(),
            })),
        }
    }
}

impl Dispatcher {
    /// Register a pending `turn/run` and get the receiver its `TurnStart`
    /// response will arrive on. Register BEFORE `bus.submit(UserMessage)` so the
    /// consumer is ready before the yield fires.
    pub fn register_turn(&self, id: Value) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        self.state
            .lock()
            .expect("dispatcher state poisoned")
            .pending_turns
            .push_back(Pending { id, respond: tx });
        rx
    }
    pub fn register_session_create(&self, id: Value) -> oneshot::Receiver<Value> {
        self.register(id, |s| &mut s.pending_session_create)
    }
    pub fn register_session_load(&self, id: Value) -> oneshot::Receiver<Value> {
        self.register(id, |s| &mut s.pending_session_load)
    }
    pub fn register_session_clear(&self, id: Value) -> oneshot::Receiver<Value> {
        self.register(id, |s| &mut s.pending_session_clear)
    }
    pub fn register_session_delete(&self, id: Value) -> oneshot::Receiver<Value> {
        self.register(id, |s| &mut s.pending_session_delete)
    }
    pub fn register_compact(&self, id: Value) -> oneshot::Receiver<Value> {
        self.register(id, |s| &mut s.pending_compact)
    }
    pub fn register_init(&self, id: Value) -> oneshot::Receiver<Value> {
        self.register(id, |s| &mut s.pending_init)
    }

    fn register(
        &self,
        id: Value,
        select: impl Fn(&mut State) -> &mut VecDeque<Pending>,
    ) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        select(&mut self.state.lock().expect("dispatcher state poisoned"))
            .push_back(Pending { id, respond: tx });
        rx
    }

    /// Run the single yield consumer over a **pre-subscribed** receiver.
    /// Subscribing before spawning (caller's thread) guarantees the receiver
    /// exists before any yield is emitted — the production engine emits only
    /// after a turn is submitted (by which point `run` is polled), but tests
    /// emit directly, so eager subscription avoids a lost-first-yield race.
    ///
    /// Returns when the bus's yield stream closes (engine shutdown).
    pub async fn run(self, mut rx: broadcast::Receiver<EngineYield>) {
        loop {
            match rx.recv().await {
                Ok(yield_) => self.dispatch(yield_),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    fn dispatch(&self, yield_: EngineYield) {
        // Pick the queue for this variant and pop its head, all under a short
        // lock; then serialize + fulfill outside the lock.
        let pending = {
            let mut st = self.state.lock().expect("dispatcher state poisoned");
            let queue: &mut VecDeque<Pending> = match &yield_ {
                EngineYield::TurnStart { .. } => &mut st.pending_turns,
                EngineYield::SessionCreated { .. } => &mut st.pending_session_create,
                EngineYield::SessionLoaded { .. } => &mut st.pending_session_load,
                EngineYield::SessionCleared { .. } => &mut st.pending_session_clear,
                EngineYield::SessionDeleted { .. } => &mut st.pending_session_delete,
                EngineYield::CompactResult { .. } => &mut st.pending_compact,
                EngineYield::InitResult { .. } => &mut st.pending_init,
                // Not a resolving yield — every other variant is just an
                // `event` notification the forwarders mirror; the dispatcher
                // ignores it.
                _ => return,
            };
            queue.pop_front()
        };
        if let Some(p) = pending {
            // Strip the `kind` tag so the response `result` carries only the
            // yield's fields (e.g. `{turn_id, task}`), not the bus's variant
            // discriminator. If serialization fails (shouldn't — all yields
            // are Serialize), fall back to a minimal ok object.
            let mut val = serde_json::to_value(&yield_).unwrap_or_else(|_| json!({}));
            if let Value::Object(ref mut map) = val {
                map.remove("kind");
            }
            // Ignore send error: the requesting connection closed before the
            // yield arrived (its oneshot receiver was dropped) — nothing to do.
            let _ = p.respond.send(val);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_bus::{BusParadigmKind, BusTurnSummary, EngineBus, InProcessBus};
    use serde_json::json;

    #[tokio::test]
    async fn turn_run_resolves_on_turn_start() {
        let (bus, _directive_rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let disp = Dispatcher::default();
        let yield_rx = bus.subscribe_yields();
        let disp_run = disp.clone();
        let handle = tokio::spawn(async move { disp_run.run(yield_rx).await });
        // Register a pending turn, then emit TurnStart, expect resolution.
        let rx_resp = disp.register_turn(json!(1));
        let _ = bus.emit(EngineYield::TurnStart {
            turn_id: "t1".into(),
            task: "hello".into(),
        });
        let resp = rx_resp.await.expect("resolved");
        assert_eq!(resp["turn_id"], "t1");
        assert_eq!(resp["task"], "hello");
        assert!(resp.get("kind").is_none(), "kind tag should be stripped");
        handle.abort();
    }

    #[tokio::test]
    async fn unknown_yield_does_not_resolve_turn() {
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let disp = Dispatcher::default();
        let yield_rx = bus.subscribe_yields();
        let disp_run = disp.clone();
        let handle = tokio::spawn(async move { disp_run.run(yield_rx).await });
        let mut rx_resp = disp.register_turn(json!(1));
        // A non-resolving yield (StreamChunk) must NOT fulfill the pending turn.
        let _ = bus.emit(EngineYield::StreamChunk {
            turn_id: "x".into(),
            text: "ignore me".into(),
            speaker: None,
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            rx_resp.try_recv().is_err(),
            "non-resolving yield must not resolve a pending turn"
        );
        // Now the resolving yield fires.
        let _ = bus.emit(EngineYield::TurnStart {
            turn_id: "t1".into(),
            task: "go".into(),
        });
        let resp = rx_resp.await.expect("resolved");
        assert_eq!(resp["turn_id"], "t1");
        handle.abort();
    }

    #[tokio::test]
    async fn fifo_order_preserved_for_turns() {
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let disp = Dispatcher::default();
        let yield_rx = bus.subscribe_yields();
        let disp_run = disp.clone();
        let handle = tokio::spawn(async move { disp_run.run(yield_rx).await });
        let r1 = disp.register_turn(json!("a"));
        let r2 = disp.register_turn(json!("b"));
        let _ = bus.emit(EngineYield::TurnStart {
            turn_id: "t1".into(),
            task: "one".into(),
        });
        let _ = bus.emit(EngineYield::TurnStart {
            turn_id: "t2".into(),
            task: "two".into(),
        });
        let v1 = r1.await.unwrap();
        let v2 = r2.await.unwrap();
        assert_eq!(v1["turn_id"], "t1");
        assert_eq!(v2["turn_id"], "t2");
        handle.abort();
    }

    #[tokio::test]
    async fn compact_result_strips_kind() {
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let disp = Dispatcher::default();
        let yield_rx = bus.subscribe_yields();
        let disp_run = disp.clone();
        let handle = tokio::spawn(async move { disp_run.run(yield_rx).await });
        let rx_resp = disp.register_compact(json!(9));
        let _ = bus.emit(EngineYield::CompactResult {
            summary: "done".into(),
            removed_count: 3,
            retained: vec![("user".into(), "hi".into())],
        });
        let resp = rx_resp.await.unwrap();
        assert_eq!(resp["summary"], "done");
        assert_eq!(resp["removed_count"], 3);
        assert!(resp.get("kind").is_none());
        handle.abort();
    }

    // Reuse a yield variant only to ensure non-fragment variants compile in the
    // match above; this is a type-level sanity check.
    #[test]
    fn turn_complete_yields_compiles() {
        let y = EngineYield::TurnComplete {
            turn_id: "t".into(),
            summary: BusTurnSummary {
                final_answer: "a".into(),
                iterations: 1,
                completed: true,
                active_paradigm: BusParadigmKind::ReAct,
            },
        };
        // TurnComplete is NOT a resolving yield — the dispatcher ignores it
        // (it arrives as an `event` notification). Just ensure it serializes.
        let _ = serde_json::to_string(&y).unwrap();
    }
}
