//! Adapter — the JSON-RPC ↔ bus mapping, transport-agnostic.
//!
//! [`serve_connection`] binds one frontend connection to the engine bus. A
//! connection is two channels of raw JSON text: `inbound_rx` (frontend →
//! app-server, one JSON-RPC message per item) and `outbound_tx`
//! (app-server → frontend). Each transport (stdio/ipc/ws) bridges its concrete
//! byte stream to these channels; the adapter is pure JSON-RPC logic.
//!
//! Two halves run concurrently per connection:
//!
//! - **Outbound forwarder**: subscribes to the bus's yield broadcast, maps each
//!   [`EngineYield`] to a JSON-RPC `event` notification, and sends it to the
//!   frontend. This is the per-connection view of the shared bus (every
//!   connection sees every yield).
//! - **Inbound dispatcher**: decodes each inbound message; for a
//!   [`Request`](crate::protocol::Request), spawns a handler that maps the
//!   method to a [`Directive`] and `bus.submit`s it. Blocking-ack methods
//!   (turn/run, session/create, …) register a pending with the shared
//!   [`Dispatcher`] and await the matching yield before responding.
//!
//! The shared [`Dispatcher`] (one per process) resolves blocking-ack requests
//! — see `dispatcher.rs` for why it's a single consumer.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use oneai_bus::{
    BusGroupScenario, BusParadigmKind, BusScenario, Directive, EngineBus, InProcessBus,
};
use oneai_core::{ContentBlock, InteractionResponse, InterruptReason, SessionInfo};

use crate::dispatcher::Dispatcher;
use crate::protocol::{decode_inbound, method, Notification, Response, RpcError};
use crate::{SharedConversationStore, SharedScenarioStore};

/// Serve one connection until either side closes.
///
/// `inbound_rx` yields one JSON-RPC message per item (raw JSON text, as framed
/// by the transport — a line for stdio/ipc, a text frame for ws). `outbound_tx`
/// accepts one JSON-RPC message per item to write back. The adapter owns neither
/// channel's transport framing — it only moves parsed messages.
pub async fn serve_connection(
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    mut inbound_rx: mpsc::Receiver<String>,
    outbound_tx: mpsc::Sender<String>,
) {
    // ── Outbound forwarder: bus yields → `event` notification ───────────────
    let out_tx = outbound_tx.clone();
    let forwarder = spawn_yield_forwarder(bus.clone(), out_tx);

    // ── Inbound dispatcher ──────────────────────────────────────────────────
    while let Some(line) = inbound_rx.recv().await {
        match decode_inbound(&line) {
            // Malformed / invalid request — reply with a null-id error so a
            // strict JSON-RPC client sees the protocol violation.
            Err(rpc_err) => {
                let resp = Response::err(Value::Null, rpc_err);
                send(&outbound_tx, &resp);
            }
            // Inbound notification — the app-server defines no inbound
            // notifications (the schema is request-only inbound); ignore.
            Ok(Err(_notification)) => {}
            Ok(Ok(req)) => {
                let id = req.id.clone();
                let bus = bus.clone();
                let dispatcher = dispatcher.clone();
                let scenario_store = scenario_store.clone();
                let session_store = session_store.clone();
                let out_tx = outbound_tx.clone();
                // Each request runs on its own task so a long turn/run (awaiting
                // TurnStart) doesn't block the next inbound message (e.g. an
                // approval/respond arriving mid-turn). JSON-RPC id correlation
                // makes out-of-order responses fine.
                tokio::spawn(async move {
                    let resp =
                        handle_request(req, bus, dispatcher, scenario_store, session_store).await;
                    send(&out_tx, &resp);
                });
                let _ = id; // id is read inside handle_request via req.id
            }
        }
    }
    forwarder.abort();
}

/// Spawn the outbound forwarder: drain the bus's yield broadcast and send each
/// yield as a JSON-RPC `event` notification on the connection's outbound
/// channel. Returns when the yield stream closes (engine shutdown) or the
/// outbound channel's receiver drops (connection closed).
fn spawn_yield_forwarder(bus: Arc<InProcessBus>, out_tx: mpsc::Sender<String>) -> JoinHandle<()> {
    let mut rx = bus.subscribe_yields();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(yield_) => {
                    if let Ok(n) = Notification::event(&yield_) {
                        if let Ok(line) = serde_json::to_string(&n) {
                            if out_tx.send(line).await.is_err() {
                                return; // connection closed
                            }
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

/// Serialize a response and send it on the outbound channel. Best-effort — a
/// send error (connection closed) just drops the response; the connection task
/// will tear down anyway.
fn send(out_tx: &mpsc::Sender<String>, resp: &Response) {
    if let Ok(line) = serde_json::to_string(resp) {
        let _ = out_tx.try_send(line);
    }
}

/// Map one JSON-RPC request to a Directive + bus submission and produce the
/// JSON-RPC response.
async fn handle_request(
    req: crate::protocol::Request,
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
) -> Response {
    let id = req.id.clone();
    let params = req.params;

    // Helper: build an ack response `{ok:true}` after a successful submit.
    let ack = |id: Value| Response::ok(id, json!({"ok": true}));

    match req.method.as_str() {
        // ── Turn lifecycle ───────────────────────────────────────────────
        method::TURN_RUN => {
            let content = match field::<Vec<ContentBlock>>(&params, "content") {
                Ok(c) => c,
                Err(e) => return Response::err(id, e),
            };
            // Register BEFORE submit so the dispatcher is ready before TurnStart
            // fires. If submit fails (bus closed — the only UserMessage failure
            // mode), the stale pending's receiver is dropped on return, so any
            // later TurnStart send no-ops harmlessly; no next turn fires anyway
            // (bus closed is terminal).
            let rx = dispatcher.register_turn(id.clone());
            if let Err(e) = bus.submit(Directive::UserMessage { content }).await {
                return Response::err(id, RpcError::internal(e.to_string()));
            }
            match rx.await {
                Ok(val) => Response::ok(id, val),
                Err(_) => {
                    Response::err(id, RpcError::internal("engine closed before turn started"))
                }
            }
        }
        method::TURN_CANCEL => {
            // reason is optional — default to a generic client cancel.
            let reason = opt_field::<InterruptReason>(&params, "reason").unwrap_or(
                InterruptReason::Custom {
                    reason: "client requested cancel".to_string(),
                },
            );
            submit_ack(bus, id, Directive::Interrupt { reason }).await
        }

        // ── Approval ─────────────────────────────────────────────────────
        method::APPROVAL_RESPOND => {
            let request_id = match field::<String>(&params, "request_id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let response = match field::<InteractionResponse>(&params, "response") {
                Ok(r) => r,
                Err(e) => return Response::err(id, e),
            };
            // The bus resolves Approve itself; submit errors (unknown /
            // already-resolved request_id) → -32603.
            match bus
                .submit(Directive::Approve {
                    request_id,
                    response,
                })
                .await
            {
                Ok(()) => ack(id),
                Err(e) => Response::err(id, RpcError::internal(e.to_string())),
            }
        }

        // ── Paradigm / config ────────────────────────────────────────────
        method::PARADIGM_SWITCH => {
            let to = match field::<BusParadigmKind>(&params, "to") {
                Ok(k) => k,
                Err(e) => return Response::err(id, e),
            };
            submit_ack(bus, id, Directive::SwitchParadigm { to }).await
        }
        method::CONFIG_UPDATE => {
            // plan_mode optional — None ⇒ leave unchanged.
            let plan_mode = opt_field::<bool>(&params, "plan_mode");
            submit_ack(bus, id, Directive::UpdateConfig { plan_mode }).await
        }

        // ── Session lifecycle (blocking-ack where the pump always emits
        //     the result yield) ───────────────────────────────────────────
        method::SESSION_CREATE => {
            let sid = opt_field::<String>(&params, "id");
            let rx = dispatcher.register_session_create(id.clone());
            if let Err(e) = bus.submit(Directive::CreateSession { id: sid }).await {
                return Response::err(id, RpcError::internal(e.to_string()));
            }
            resolve_or_closed(id, rx).await
        }
        method::SESSION_LOAD => {
            let sid = match field::<String>(&params, "id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let rx = dispatcher.register_session_load(id.clone());
            if let Err(e) = bus.submit(Directive::LoadSession { id: sid }).await {
                return Response::err(id, RpcError::internal(e.to_string()));
            }
            resolve_or_closed(id, rx).await
        }
        method::SESSION_CLEAR => {
            let rx = dispatcher.register_session_clear(id.clone());
            if let Err(e) = bus.submit(Directive::ClearSession).await {
                return Response::err(id, RpcError::internal(e.to_string()));
            }
            resolve_or_closed(id, rx).await
        }
        // session/list — synchronous CRUD against the shared conversation store
        // (no Directive/bus round-trip; mirrors scenario/list). Returns the
        // epoch-millis shape the FFI SessionInfoView exposes so a foreign UI
        // renders one list regardless of transport.
        method::SESSION_LIST => {
            let sessions = session_store.list().await;
            let arr: Vec<Value> = sessions.iter().map(session_info_to_json).collect();
            Response::ok(id, json!({"sessions": arr}))
        }
        // session/delete + conversation/compact are ack methods — their result
        // (SessionDeleted / CompactResult / Error) arrives as an `event`
        // notification, not a response, because the pump emits `Error` (not the
        // result yield) on failure, which would otherwise hang a blocking-ack.
        method::SESSION_DELETE => {
            let sid = match field::<String>(&params, "id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            submit_ack(bus, id, Directive::DeleteSession { id: sid }).await
        }
        method::CONVERSATION_COMPACT => {
            let keep = match field::<usize>(&params, "keep_recent_turns") {
                Ok(k) => k,
                Err(e) => return Response::err(id, e),
            };
            submit_ack(
                bus,
                id,
                Directive::Compact {
                    keep_recent_turns: keep,
                },
            )
            .await
        }

        // ── Project init (blocking-ack — InitResult always fires) ────────
        method::PROJECT_INIT => {
            let format = opt_field::<String>(&params, "format");
            let force = opt_field::<bool>(&params, "force").unwrap_or(false);
            let no_llm = opt_field::<bool>(&params, "no_llm").unwrap_or(false);
            let rx = dispatcher.register_init(id.clone());
            if let Err(e) = bus
                .submit(Directive::InitProject {
                    format,
                    force,
                    no_llm,
                })
                .await
            {
                return Response::err(id, RpcError::internal(e.to_string()));
            }
            resolve_or_closed(id, rx).await
        }

        // ── Group chat (all ack — results stream as event notifications) ─
        method::GROUP_START => {
            let scenario = match field::<BusGroupScenario>(&params, "scenario") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            submit_ack(bus, id, Directive::StartGroupChat { scenario }).await
        }
        method::GROUP_OPEN => submit_ack(bus, id, Directive::GroupStart).await,
        method::GROUP_RUN => {
            let user_input = match field::<String>(&params, "user_input") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            submit_ack(bus, id, Directive::GroupUserMessage { user_input }).await
        }
        method::GROUP_SET_ORDER => {
            let order = match field::<Vec<String>>(&params, "order") {
                Ok(o) => o,
                Err(e) => return Response::err(id, e),
            };
            submit_ack(bus, id, Directive::GroupSetScriptedOrder { order }).await
        }

        // ── Scenario library (synchronous CRUD — pure shared state, no
        // Directive/bus. Every frontend's editor + launch flow reads/writes
        // the same store; `scenario/validate` is the single authoritative
        // validator that replaces each frontend's client-side mirror.)
        method::SCENARIO_LIST => match scenario_store.list().await {
            Ok(scenarios) => Response::ok(id, json!({"scenarios": scenarios})),
            Err(e) => Response::err(id, RpcError::internal(e.to_string())),
        },
        method::SCENARIO_GET => {
            let sid = match field::<String>(&params, "id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            match scenario_store.get(&sid).await {
                Ok(Some(s)) => Response::ok(id, json!(s)),
                Ok(None) => Response::err(
                    id,
                    RpcError::invalid_params(format!("unknown scenario id: {sid}")),
                ),
                Err(e) => Response::err(id, RpcError::internal(e.to_string())),
            }
        }
        method::SCENARIO_UPSERT => {
            let scenario = match field::<BusScenario>(&params, "scenario") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let errs = scenario.validate();
            if !errs.is_empty() {
                // Don't store an unlaunchable scenario — return the validation
                // problems so the editor flags every field inline. Not a
                // JSON-RPC *error* (this is a normal result the editor renders).
                return Response::ok(id, json!({"ok": false, "errors": errs}));
            }
            let sid = scenario.id.clone();
            match scenario_store.upsert(scenario).await {
                Ok(()) => Response::ok(id, json!({"ok": true, "id": sid})),
                Err(e) => Response::err(id, RpcError::internal(e.to_string())),
            }
        }
        method::SCENARIO_DELETE => {
            let sid = match field::<String>(&params, "id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            match scenario_store.delete(&sid).await {
                Ok(()) => Response::ok(id, json!({"ok": true})),
                Err(e) => Response::err(id, RpcError::internal(e.to_string())),
            }
        }
        method::SCENARIO_VALIDATE => {
            let scenario = match field::<BusScenario>(&params, "scenario") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let errs = scenario.validate();
            Response::ok(id, json!({"ok": errs.is_empty(), "errors": errs}))
        }

        method::SHUTDOWN => submit_ack(bus, id, Directive::Shutdown).await,

        // Unknown method.
        _ => Response::err(id, RpcError::method_not_found(&req.method)),
    }
}

/// Submit a directive; on success respond `{ok:true}`, on bus error respond
/// `-32603`. For directives the bus forwards to the engine driver (i.e. not
/// `Approve`/`Interrupt`), submit fails only when the bus/pump is closed.
async fn submit_ack(bus: Arc<InProcessBus>, id: Value, directive: Directive) -> Response {
    match bus.submit(directive).await {
        Ok(()) => Response::ok(id, json!({"ok": true})),
        Err(e) => Response::err(id, RpcError::internal(e.to_string())),
    }
}

/// Await a blocking-ack resolver receiver; map the fulfilled value to a success
/// response, or a -32603 if the engine closed before the resolving yield.
async fn resolve_or_closed(id: Value, rx: tokio::sync::oneshot::Receiver<Value>) -> Response {
    match rx.await {
        Ok(val) => Response::ok(id, val),
        Err(_) => Response::err(id, RpcError::internal("engine closed before result yield")),
    }
}

/// Extract a required typed field from JSON-RPC `params`. Returns an
/// `INVALID_PARAMS` error on absence/type mismatch.
fn field<T: DeserializeOwned>(params: &Value, key: &str) -> std::result::Result<T, RpcError> {
    serde_json::from_value(params.get(key).cloned().unwrap_or(Value::Null))
        .map_err(|e| RpcError::invalid_params(format!("{key}: {e}")))
}

/// Extract an optional typed field; `None` if absent or wrong type.
fn opt_field<T: DeserializeOwned>(params: &Value, key: &str) -> Option<T> {
    serde_json::from_value(params.get(key)?.clone()).ok()
}

/// Serialize a `SessionInfo` to the epoch-millis shape the FFI
/// `SessionInfoView` exposes (`id` / `created_at_ms` / `updated_at_ms` /
/// `message_count` / `title`). The FFI path flattens `chrono::DateTime` to
/// millis at the UniFFI boundary (chrono can't cross FFI directly); the
/// sidecar JSON-RPC path mirrors that exact shape so a foreign UI decodes
/// one struct regardless of transport.
fn session_info_to_json(s: &SessionInfo) -> Value {
    json!({
        "id": s.id,
        "created_at_ms": s.created_at.timestamp_millis(),
        "updated_at_ms": s.updated_at.timestamp_millis(),
        "message_count": s.message_count,
        "title": s.title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::error_code;
    use serde_json::json;

    #[test]
    fn field_required_missing_is_invalid_params() {
        let params = json!({});
        let err: std::result::Result<String, _> = field(&params, "id");
        assert!(err.is_err());
        assert_eq!(err.unwrap_err().code, error_code::INVALID_PARAMS);
    }

    #[test]
    fn opt_field_absent_is_none() {
        let params = json!({"force": true});
        assert!(opt_field::<String>(&params, "format").is_none());
        assert_eq!(opt_field::<bool>(&params, "force"), Some(true));
    }

    #[tokio::test]
    async fn unknown_method_responds_method_not_found() {
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let dispatcher = Dispatcher::default();
        let scenario_store: SharedScenarioStore =
            Arc::new(crate::scenario::InMemoryScenarioStore::new());
        let session_store: SharedConversationStore =
            Arc::new(crate::conversation::InMemoryConversationStore::new());
        let req = crate::protocol::Request::new(json!(1), "no/such/method", json!({}));
        let resp = handle_request(req, bus, dispatcher, scenario_store, session_store).await;
        let err = resp.error.expect("error response");
        assert_eq!(err.code, error_code::METHOD_NOT_FOUND);
        assert_eq!(resp.id, json!(1));
    }
}
