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
use crate::probe::{ProviderEntryDto, SharedAppProbe};
use crate::protocol::{decode_inbound, method, Notification, Response, RpcError};
use crate::{
    SharedConversationStore, SharedFeedbackStore, SharedHostAllowlistRpc, SharedScenarioStore,
};

/// Serve one connection until either side closes.
///
/// `inbound_rx` yields one JSON-RPC message per item (raw JSON text, as framed
/// by the transport — a line for stdio/ipc, a text frame for ws). `outbound_tx`
/// accepts one JSON-RPC message per item to write back. The adapter owns neither
/// channel's transport framing — it only moves parsed messages.
#[allow(clippy::too_many_arguments)]
pub async fn serve_connection(
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    feedback_store: SharedFeedbackStore,
    host_allowlist_rpc: SharedHostAllowlistRpc,
    probe: SharedAppProbe,
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
                let feedback_store = feedback_store.clone();
                let host_allowlist_rpc = host_allowlist_rpc.clone();
                let probe = probe.clone();
                let out_tx = outbound_tx.clone();
                // Each request runs on its own task so a long turn/run (awaiting
                // TurnStart) doesn't block the next inbound message (e.g. an
                // approval/respond arriving mid-turn). JSON-RPC id correlation
                // makes out-of-order responses fine.
                tokio::spawn(async move {
                    let resp = handle_request(
                        req,
                        bus,
                        dispatcher,
                        scenario_store,
                        session_store,
                        feedback_store,
                        host_allowlist_rpc,
                        probe,
                    )
                    .await;
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
#[allow(clippy::too_many_arguments)]
async fn handle_request(
    req: crate::protocol::Request,
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    feedback_store: SharedFeedbackStore,
    host_allowlist_rpc: SharedHostAllowlistRpc,
    probe: SharedAppProbe,
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
            // The workspace the user bound this session to (a working-directory
            // path); None ⇒ app-global cwd. Forwarded into the directive so the
            // engine persists + applies it.
            let workspace = opt_field::<String>(&params, "workspace");
            let rx = dispatcher.register_session_create(id.clone());
            if let Err(e) = bus
                .submit(Directive::CreateSession { id: sid, workspace })
                .await
            {
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
        // session/rename — synchronous CRUD against the shared conversation
        // store (no Directive/bus round-trip, like session/list). Params:
        // {id, title}. The store swallows backend errors (returning false),
        // so a false result is surfaced to the frontend as a not-found error;
        // an empty title is a no-op (keep current) and acks.
        method::SESSION_RENAME => {
            let sid = match field::<String>(&params, "id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let title = match field::<String>(&params, "title") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            if session_store.rename(&sid, &title).await {
                ack(id)
            } else {
                Response::err(id, RpcError::internal(format!("session not found: {sid}")))
            }
        }
        // session/archive — toggle the archived flag. Params: {id, archived}.
        method::SESSION_ARCHIVE => {
            let sid = match field::<String>(&params, "id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let archived = opt_field::<bool>(&params, "archived").unwrap_or(true);
            if session_store.set_archived(&sid, archived).await {
                ack(id)
            } else {
                Response::err(id, RpcError::internal(format!("session not found: {sid}")))
            }
        }
        // dialog/pick_directory — open the native OS folder picker (macOS
        // `osascript choose folder` / Linux zenity·kdialog / Windows
        // FolderBrowserDialog) and return the chosen absolute path. The local
        // sidecar can show a native dialog (a browser can't get a host path);
        // the web frontend delegates to it. `None` ⇐ user cancelled or no
        // picker installed. No bus round-trip — handled inline.
        method::DIALOG_PICK_DIRECTORY => {
            let path = crate::dialog::pick_directory().await;
            Response::ok(id, json!({ "path": path }))
        }
        // feedback/submit — record one per-message 👍/👎/note. Sync CRUD
        // against the shared feedback store (no bus round-trip, like
        // session/list). `text` is optional (only for `note`-kind).
        method::FEEDBACK_SUBMIT => {
            let session_id = match field::<String>(&params, "session_id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let turn_id = match field::<String>(&params, "turn_id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let message_role = field::<String>(&params, "message_role")
                .unwrap_or_else(|_| "assistant".to_string());
            let kind = match field::<String>(&params, "kind") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let text = field::<String>(&params, "text").ok();
            feedback_store
                .record(&session_id, &turn_id, &message_role, &kind, text.as_deref())
                .await;
            Response::ok(id, json!({"ok": true}))
        }
        // feedback/list — all feedback entries for a session (so a reloaded
        // session can restore 👍/👎 markers on its assistant bubbles).
        method::FEEDBACK_LIST => {
            let session_id = match field::<String>(&params, "session_id") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            let entries = feedback_store.list(&session_id).await;
            let arr: Vec<Value> = entries
                .iter()
                .map(|e| {
                    json!({
                        "id": e.id,
                        "session_id": e.session_id,
                        "turn_id": e.turn_id,
                        "message_role": e.message_role,
                        "kind": e.kind,
                        "text": e.text,
                        "created_at_ms": e.created_at_ms,
                    })
                })
                .collect();
            Response::ok(id, json!({"feedback": arr}))
        }
        // host/list — both admitted and denied hosts in one round-trip so the
        // Settings panel renders without a second call. Sync CRUD against the
        // shared durable store (no bus/Directive, like feedback/list).
        method::HOST_LIST => {
            let allowed = host_allowlist_rpc.list_allowed().await;
            let denied = host_allowlist_rpc.list_denied().await;
            let allow_arr: Vec<Value> = allowed
                .iter()
                .map(|e| json!({"host": e.host, "recorded_at_ms": e.recorded_at_ms}))
                .collect();
            let deny_arr: Vec<Value> = denied
                .iter()
                .map(|e| json!({"host": e.host, "recorded_at_ms": e.recorded_at_ms}))
                .collect();
            Response::ok(id, json!({"allowed": allow_arr, "denied": deny_arr}))
        }
        // host/allow — admit a host persistently ("always"); the engine
        // NetworkProxy consults the same durable table on its next CONNECT, so
        // the host no longer re-prompts across sessions.
        method::HOST_ALLOW => {
            let host = match field::<String>(&params, "host") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            host_allowlist_rpc.admit(host).await;
            ack(id)
        }
        // host/deny — block a host persistently; future tunnel attempts are
        // blocked without re-prompting.
        method::HOST_DENY => {
            let host = match field::<String>(&params, "host") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            host_allowlist_rpc.deny(host).await;
            ack(id)
        }
        // host/remove — revoke an admission (delete from the allowlist).
        method::HOST_REMOVE => {
            let host = match field::<String>(&params, "host") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            host_allowlist_rpc.remove(host).await;
            ack(id)
        }
        // host/remove-denied — revoke a denial (delete from the denylist).
        method::HOST_REMOVE_DENIED => {
            let host = match field::<String>(&params, "host") {
                Ok(s) => s,
                Err(e) => return Response::err(id, e),
            };
            host_allowlist_rpc.remove_denied(host).await;
            ack(id)
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

        // ── App probe (read-only config + skill lifecycle) — synchronous,
        //    handled directly against the shared AppProbe, no Directive/bus.
        //    `domainpack/switch` / `provider/add` are intentionally absent
        //    (no hot-swap path — see probe.rs); a pack/provider change
        //    restarts the app-server. ─────────────────────────────────────
        method::CONFIG_GET => {
            let snap = probe.config().await;
            Response::ok(id, serde_json::to_value(snap).unwrap_or(json!({})))
        }
        method::PROVIDER_LIST => {
            let list = probe.providers().await;
            Response::ok(id, json!({"providers": list}))
        }
        method::PROVIDER_ADD => {
            let entry = match field::<ProviderEntryDto>(&params, "entry") {
                Ok(e) => e,
                Err(e) => return Response::err(id, e),
            };
            let res = probe.provider_add(entry).await;
            Response::ok(
                id,
                serde_json::to_value(res).unwrap_or(json!({"ok": false})),
            )
        }
        method::PROVIDER_DELETE => {
            let name = match field::<String>(&params, "name") {
                Ok(n) => n,
                Err(e) => return Response::err(id, e),
            };
            let res = probe.provider_delete(&name).await;
            Response::ok(
                id,
                serde_json::to_value(res).unwrap_or(json!({"ok": false})),
            )
        }
        method::PROVIDER_SET_ACTIVE => {
            let name = match field::<String>(&params, "name") {
                Ok(n) => n,
                Err(e) => return Response::err(id, e),
            };
            let res = probe.provider_set_active(&name).await;
            Response::ok(
                id,
                serde_json::to_value(res).unwrap_or(json!({"ok": false})),
            )
        }
        method::CONFIG_READ => {
            let view = probe.config_read().await;
            Response::ok(id, serde_json::to_value(view).unwrap_or(json!({})))
        }
        method::DOMAINPACK_LIST => {
            let list = probe.domainpacks().await;
            Response::ok(id, serde_json::to_value(list).unwrap_or(json!({})))
        }
        method::SKILL_LIST => {
            let list = probe.skills().await;
            Response::ok(id, json!({"skills": list}))
        }
        method::SKILL_PIN => skill_op(&probe, &id, &params, OpKind::Pin).await,
        method::SKILL_UNPIN => skill_op(&probe, &id, &params, OpKind::Unpin).await,
        method::SKILL_ARCHIVE => skill_op(&probe, &id, &params, OpKind::Archive).await,
        method::SKILL_RESTORE => skill_op(&probe, &id, &params, OpKind::Restore).await,

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

/// Which skill lifecycle op the inbound method requested.
enum OpKind {
    Pin,
    Unpin,
    Archive,
    Restore,
}

/// Handle a `skill/{pin|unpin|archive|restore}` request: extract the `name`
/// field, dispatch to the probe, and return the op result.
async fn skill_op(probe: &SharedAppProbe, id: &Value, params: &Value, kind: OpKind) -> Response {
    let name = match field::<String>(params, "name") {
        Ok(n) => n,
        Err(e) => return Response::err(id.clone(), e),
    };
    let res = match kind {
        OpKind::Pin => probe.skill_pin(&name).await,
        OpKind::Unpin => probe.skill_unpin(&name).await,
        OpKind::Archive => probe.skill_archive(&name).await,
        OpKind::Restore => probe.skill_restore(&name).await,
    };
    Response::ok(
        id.clone(),
        serde_json::to_value(res).unwrap_or(json!({"ok": false})),
    )
}

/// Serialize a `SessionInfo` to the epoch-millis shape the FFI
/// `SessionInfoView` exposes (`id` / `created_at_ms` / `updated_at_ms` /
/// `message_count` / `title` / `workspace`). The FFI path flattens
/// `chrono::DateTime` to millis at the UniFFI boundary (chrono can't cross
/// FFI directly); the sidecar JSON-RPC path mirrors that exact shape so a
/// foreign UI decodes one struct regardless of transport.
fn session_info_to_json(s: &SessionInfo) -> Value {
    let mut v = json!({
        "id": s.id,
        "created_at_ms": s.created_at.timestamp_millis(),
        "updated_at_ms": s.updated_at.timestamp_millis(),
        "message_count": s.message_count,
        "title": s.title,
        "archived": s.archived,
    });
    if let Some(w) = &s.workspace {
        v["workspace"] = json!(w);
    }
    v
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
        let feedback_store: SharedFeedbackStore =
            Arc::new(crate::feedback::InMemoryFeedbackStore::new());
        let host_allowlist_rpc: SharedHostAllowlistRpc =
            Arc::new(crate::host_allowlist::InMemoryHostAllowlistRpc::new());
        let probe: SharedAppProbe = Arc::new(crate::probe::NullAppProbe);
        let req = crate::protocol::Request::new(json!(1), "no/such/method", json!({}));
        let resp = handle_request(
            req,
            bus,
            dispatcher,
            scenario_store,
            session_store,
            feedback_store,
            host_allowlist_rpc,
            probe,
        )
        .await;
        let err = resp.error.expect("error response");
        assert_eq!(err.code, error_code::METHOD_NOT_FOUND);
        assert_eq!(resp.id, json!(1));
    }

    /// Shared harness for the W4 probe method tests: a `NullAppProbe` (empty
    /// / not_supported) so we verify routing + response *shape*, not the
    /// probe's own logic (which the CLI's `AppProbeImpl` owns).
    async fn probe_response(method: &str, params: Value) -> Response {
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let dispatcher = Dispatcher::default();
        let scenario_store: SharedScenarioStore =
            Arc::new(crate::scenario::InMemoryScenarioStore::new());
        let session_store: SharedConversationStore =
            Arc::new(crate::conversation::InMemoryConversationStore::new());
        let feedback_store: SharedFeedbackStore =
            Arc::new(crate::feedback::InMemoryFeedbackStore::new());
        let host_allowlist_rpc: SharedHostAllowlistRpc =
            Arc::new(crate::host_allowlist::InMemoryHostAllowlistRpc::new());
        let probe: SharedAppProbe = Arc::new(crate::probe::NullAppProbe);
        let req = crate::protocol::Request::new(json!(2), method, params);
        handle_request(
            req,
            bus,
            dispatcher,
            scenario_store,
            session_store,
            feedback_store,
            host_allowlist_rpc,
            probe,
        )
        .await
    }

    #[tokio::test]
    async fn config_get_returns_snapshot() {
        let resp = probe_response("config/get", json!({})).await;
        assert!(resp.error.is_none(), "config/get should not error");
        // NullAppProbe ⇒ all-Default snapshot (plan_mode is the one required
        // field; the rest are skip-if-none).
        assert_eq!(resp.result.unwrap()["plan_mode"], json!(false));
    }

    #[tokio::test]
    async fn provider_list_returns_array() {
        let resp = probe_response("provider/list", json!({})).await;
        assert!(resp.error.is_none());
        assert!(resp.result.unwrap()["providers"].is_array());
    }

    #[tokio::test]
    async fn domainpack_list_returns_list() {
        let resp = probe_response("domainpack/list", json!({})).await;
        assert!(resp.error.is_none());
        let res = resp.result.unwrap();
        assert!(res["available"].is_array());
    }

    #[tokio::test]
    async fn skill_list_returns_array() {
        let resp = probe_response("skill/list", json!({})).await;
        assert!(resp.error.is_none());
        assert!(resp.result.unwrap()["skills"].is_array());
    }

    #[tokio::test]
    async fn skill_pin_missing_name_is_invalid_params() {
        // No `name` field ⇒ INVALID_PARAMS, not a probe call.
        let resp = probe_response("skill/pin", json!({})).await;
        let err = resp.error.expect("invalid params");
        assert_eq!(err.code, error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn skill_pin_null_probe_reports_not_supported() {
        // With a name but a NullAppProbe, the probe reports not_supported —
        // surfaced as an *ok result* with ok:false (op errors aren't JSON-RPC
        // errors, they're a normal result the UI renders).
        let resp = probe_response("skill/pin", json!({"name": "x"})).await;
        assert!(resp.error.is_none());
        let res = resp.result.unwrap();
        assert_eq!(res["ok"], json!(false));
        assert!(res["error"].as_str().unwrap().contains("not supported"));
    }

    #[tokio::test]
    async fn provider_add_routes_and_returns_op_result() {
        let resp = probe_response(
            "provider/add",
            json!({"entry": {"name": "openai", "kind": "openai", "model": "gpt-4"}}),
        )
        .await;
        assert!(resp.error.is_none());
        let res = resp.result.unwrap();
        assert_eq!(res["ok"], json!(false));
        assert!(res["error"].as_str().unwrap().contains("not supported"));
    }

    #[tokio::test]
    async fn provider_add_missing_entry_is_invalid_params() {
        let resp = probe_response("provider/add", json!({})).await;
        let err = resp.error.expect("invalid params");
        assert_eq!(err.code, error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn provider_set_active_routes() {
        let resp = probe_response("provider/set_active", json!({"name": "x"})).await;
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap()["ok"], json!(false));
    }

    #[tokio::test]
    async fn provider_delete_missing_name_is_invalid_params() {
        let resp = probe_response("provider/delete", json!({})).await;
        assert_eq!(resp.error.unwrap().code, error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn config_read_returns_path_and_content() {
        let resp = probe_response("config/read", json!({})).await;
        assert!(resp.error.is_none());
        let res = resp.result.unwrap();
        assert!(res["path"].is_string());
        assert!(res["content"].is_string());
    }

    /// Shared harness for the W4 feedback method tests — one `InMemoryFeedback
    /// Store` held across submit + list so the round-trip is observable.
    async fn feedback_response(
        store: SharedFeedbackStore,
        method: &str,
        params: Value,
    ) -> Response {
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let dispatcher = Dispatcher::default();
        let scenario_store: SharedScenarioStore =
            Arc::new(crate::scenario::InMemoryScenarioStore::new());
        let session_store: SharedConversationStore =
            Arc::new(crate::conversation::InMemoryConversationStore::new());
        let host_allowlist_rpc: SharedHostAllowlistRpc =
            Arc::new(crate::host_allowlist::InMemoryHostAllowlistRpc::new());
        let probe: SharedAppProbe = Arc::new(crate::probe::NullAppProbe);
        let req = crate::protocol::Request::new(json!(3), method, params);
        handle_request(
            req,
            bus,
            dispatcher,
            scenario_store,
            session_store,
            store,
            host_allowlist_rpc,
            probe,
        )
        .await
    }

    #[tokio::test]
    async fn feedback_submit_then_list_round_trips() {
        let store: SharedFeedbackStore = Arc::new(crate::feedback::InMemoryFeedbackStore::new());
        let submit = feedback_response(
            store.clone(),
            "feedback/submit",
            json!({"session_id": "s1", "turn_id": "t1", "kind": "up"}),
        )
        .await;
        assert!(submit.error.is_none());
        assert_eq!(submit.result.unwrap()["ok"], json!(true));

        // A note with text.
        let _ = feedback_response(
            store.clone(),
            "feedback/submit",
            json!({"session_id": "s1", "turn_id": "t2", "kind": "note", "text": "nice"}),
        )
        .await;

        let list = feedback_response(store, "feedback/list", json!({"session_id": "s1"})).await;
        assert!(list.error.is_none());
        let res = list.result.unwrap();
        let arr = res["feedback"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr
            .iter()
            .any(|e| e["turn_id"] == "t1" && e["kind"] == "up"));
        assert!(arr
            .iter()
            .any(|e| e["turn_id"] == "t2" && e["text"] == "nice"));
    }

    #[tokio::test]
    async fn feedback_submit_rejects_missing_fields() {
        let store: SharedFeedbackStore = Arc::new(crate::feedback::InMemoryFeedbackStore::new());
        let resp = feedback_response(store, "feedback/submit", json!({"kind": "up"})).await;
        let err = resp.error.expect("error response");
        assert_eq!(err.code, error_code::INVALID_PARAMS);
    }

    /// Shared harness for the `host/*` synchronous-CRUD methods — one
    /// `InMemoryHostAllowlistRpc` held across allow/list so the round-trip is
    /// observable (mirrors `feedback_response`).
    async fn host_response(store: SharedHostAllowlistRpc, method: &str, params: Value) -> Response {
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let dispatcher = Dispatcher::default();
        let scenario_store: SharedScenarioStore =
            Arc::new(crate::scenario::InMemoryScenarioStore::new());
        let session_store: SharedConversationStore =
            Arc::new(crate::conversation::InMemoryConversationStore::new());
        let feedback_store: SharedFeedbackStore =
            Arc::new(crate::feedback::InMemoryFeedbackStore::new());
        let probe: SharedAppProbe = Arc::new(crate::probe::NullAppProbe);
        let req = crate::protocol::Request::new(json!(5), method, params);
        handle_request(
            req,
            bus,
            dispatcher,
            scenario_store,
            session_store,
            feedback_store,
            store,
            probe,
        )
        .await
    }

    #[tokio::test]
    async fn host_allow_then_list_round_trips() {
        let store: SharedHostAllowlistRpc =
            Arc::new(crate::host_allowlist::InMemoryHostAllowlistRpc::new());
        let allow = host_response(
            store.clone(),
            "host/allow",
            json!({"host": "api.example.com"}),
        )
        .await;
        assert!(allow.error.is_none());
        assert_eq!(allow.result.unwrap()["ok"], json!(true));

        let deny = host_response(store.clone(), "host/deny", json!({"host": "evil.example"})).await;
        assert!(deny.error.is_none());

        let list = host_response(store, "host/list", json!({})).await;
        assert!(list.error.is_none());
        let res = list.result.unwrap();
        let allowed = res["allowed"].as_array().unwrap();
        let denied = res["denied"].as_array().unwrap();
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0]["host"], json!("api.example.com"));
        assert!(allowed[0]["recorded_at_ms"].is_u64());
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0]["host"], json!("evil.example"));
    }

    #[tokio::test]
    async fn host_remove_revokes() {
        let store: SharedHostAllowlistRpc =
            Arc::new(crate::host_allowlist::InMemoryHostAllowlistRpc::new());
        host_response(store.clone(), "host/allow", json!({"host": "a.example"})).await;
        host_response(store.clone(), "host/deny", json!({"host": "b.example"})).await;
        let rm = host_response(store.clone(), "host/remove", json!({"host": "a.example"})).await;
        assert_eq!(rm.result.unwrap()["ok"], json!(true));
        let rm_d = host_response(
            store.clone(),
            "host/remove-denied",
            json!({"host": "b.example"}),
        )
        .await;
        assert_eq!(rm_d.result.unwrap()["ok"], json!(true));
        let list = host_response(store, "host/list", json!({})).await;
        let res = list.result.unwrap();
        assert!(res["allowed"].as_array().unwrap().is_empty());
        assert!(res["denied"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn host_allow_rejects_missing_host() {
        let store: SharedHostAllowlistRpc =
            Arc::new(crate::host_allowlist::InMemoryHostAllowlistRpc::new());
        let resp = host_response(store, "host/allow", json!({})).await;
        let err = resp.error.expect("error response");
        assert_eq!(err.code, error_code::INVALID_PARAMS);
    }

    /// Shared harness for the `session/*` synchronous-CRUD methods, wired with a
    /// seeded conversation store so rename/archive/list round-trip one store.
    async fn session_response(
        store: SharedConversationStore,
        method: &str,
        params: Value,
    ) -> Response {
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let dispatcher = Dispatcher::default();
        let scenario_store: SharedScenarioStore =
            Arc::new(crate::scenario::InMemoryScenarioStore::new());
        let feedback_store: SharedFeedbackStore =
            Arc::new(crate::feedback::InMemoryFeedbackStore::new());
        let host_allowlist_rpc: SharedHostAllowlistRpc =
            Arc::new(crate::host_allowlist::InMemoryHostAllowlistRpc::new());
        let probe: SharedAppProbe = Arc::new(crate::probe::NullAppProbe);
        let req = crate::protocol::Request::new(json!(4), method, params);
        handle_request(
            req,
            bus,
            dispatcher,
            scenario_store,
            store,
            feedback_store,
            host_allowlist_rpc,
            probe,
        )
        .await
    }

    fn seeded_session_store() -> SharedConversationStore {
        use chrono::Utc;
        Arc::new(crate::conversation::InMemoryConversationStore::from_seed(
            vec![oneai_core::SessionInfo::new(
                "s1".into(),
                Utc::now(),
                Utc::now(),
                3,
            )],
        ))
    }

    #[tokio::test]
    async fn session_list_includes_archived_field() {
        let store = seeded_session_store();
        let resp = session_response(store, "session/list", json!({})).await;
        assert!(resp.error.is_none());
        let s = &resp.result.unwrap()["sessions"][0];
        assert_eq!(s["id"], json!("s1"));
        assert_eq!(s["archived"], json!(false));
    }

    #[tokio::test]
    async fn session_rename_then_list_reflects_new_title() {
        let store = seeded_session_store();
        let rename = session_response(
            store.clone(),
            "session/rename",
            json!({"id": "s1", "title": "Renamed"}),
        )
        .await;
        assert!(rename.error.is_none());
        assert_eq!(rename.result.unwrap()["ok"], json!(true));

        let list = session_response(store, "session/list", json!({})).await;
        assert_eq!(
            list.result.unwrap()["sessions"][0]["title"],
            json!("Renamed")
        );
    }

    #[tokio::test]
    async fn session_rename_missing_id_is_internal_error() {
        let store = seeded_session_store();
        let resp =
            session_response(store, "session/rename", json!({"id": "nope", "title": "x"})).await;
        let err = resp.error.expect("error response");
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn session_archive_toggles_then_list_reflects() {
        let store = seeded_session_store();
        let archive = session_response(
            store.clone(),
            "session/archive",
            json!({"id": "s1", "archived": true}),
        )
        .await;
        assert_eq!(archive.result.unwrap()["ok"], json!(true));

        let list = session_response(store, "session/list", json!({})).await;
        assert_eq!(list.result.unwrap()["sessions"][0]["archived"], json!(true));
    }
}
