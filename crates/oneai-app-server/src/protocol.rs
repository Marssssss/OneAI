//! JSON-RPC 2.0 wire envelope for the app-server frontend protocol.
//!
//! The app-server speaks JSON-RPC 2.0 with non-Rust frontends (IDE plugins,
//! web/JS, native macOS-Swift / Windows-C#). One *inbound* message is either a
//! [`Request`] (carries an `id` — the frontend expects a [`Response`] back) or a
//! [`Notification`] (no `id` — fire-and-forget; the app-server emits these for
//! outbound engine yields). The `id` is a [`serde_json::Value`] so the standard
//! `null` / string / number forms are all accepted (the bus side never reads
//! it; it's purely frontend↔app-server correlation).
//!
//! This is hand-rolled rather than reusing `oneai-a2a`'s `JsonRpcRequest`
//! because that type is HTTP request/response-shaped (`id: u64`, no
//! notification support, no bidirectional stream) — the app-server is a
//! bidirectional concurrent stream (arbitrary-time requests ↔ arbitrary-time
//! `event` notifications), so it needs the full envelope.
//!
//! See `docs/app-server-mechanism.md` for the method table and the
//! Directive/EngineYield mapping.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Crate-local JSON-RPC version constant.
pub const JSONRPC: &str = "2.0";

// ─── Inbound methods (frontend → app-server → bus) ───────────────────────────
//
// Each maps to a `Directive` variant. `serde_json::Value` params keep the
// adapter thin — it reuses the Directive/EngineYield serde shapes directly
// rather than re-declaring a parallel schema that would drift.

/// Inbound methods. Documented in `docs/app-server-mechanism.md`'s method table.
pub mod method {
    // Turn lifecycle.
    pub const TURN_RUN: &str = "turn/run";
    pub const TURN_CANCEL: &str = "turn/cancel";
    /// Outbound notification carrying one `EngineYield` (params = the yield
    /// value, with its `kind` tag). The single outbound method — new yield
    /// variants arrive as unknown `kind`s a frontend ignores, so the protocol
    /// grows with the bus without breaking old frontends.
    pub const EVENT: &str = "event";

    // Approval.
    pub const APPROVAL_RESPOND: &str = "approval/respond";

    // Paradigm / config.
    pub const PARADIGM_SWITCH: &str = "paradigm/switch";
    pub const CONFIG_UPDATE: &str = "config/update";

    // Session lifecycle.
    pub const SESSION_CREATE: &str = "session/create";
    pub const SESSION_LOAD: &str = "session/load";
    pub const SESSION_CLEAR: &str = "session/clear";
    pub const SESSION_DELETE: &str = "session/delete";
    /// List saved conversations (metadata only — synchronous CRUD, mirrors
    /// `scenario/list`: handled directly against the shared conversation
    /// store, no bus/Directive). Returns `{sessions: [...]}` where each entry
    /// carries `id` / `created_at_ms` / `updated_at_ms` / `message_count` /
    /// `title` / `archived` / `workspace?` — the same shape the FFI
    /// `list_conversations` returns as `SessionInfoView`, so a foreign UI
    /// renders one list regardless of transport.
    pub const SESSION_LIST: &str = "session/list";
    /// Rename a saved conversation (§W4 #10). Sync CRUD against the shared
    /// conversation store (no bus/Directive, like session/list). Params:
    /// `{id, title}`; an empty/whitespace title is a no-op (keep current).
    /// Returns `{ok: true}` on success, `-32603` when no session matches `id`.
    pub const SESSION_RENAME: &str = "session/rename";
    /// Toggle a saved conversation's archived flag (§W4 #10). Sync CRUD.
    /// Params: `{id, archived: bool}`; returns `{ok: true}` or `-32603` when
    /// not found.
    pub const SESSION_ARCHIVE: &str = "session/archive";
    /// Load one session's persisted bus-event log (issue #40 trajectory
    /// replay). Params: `{id}`. Returns `{ok, events: [<serialized
    /// EngineYield>...]}` — trajectory-relevant yields in append order;
    /// empty when the session has no log. `ok:false` when no
    /// `SessionEventStore` is wired.
    pub const SESSION_TRAJECTORY: &str = "session/trajectory";

    /// Open the native OS directory picker (deepseek-harness parity). The
    /// sidecar — a local process — shells out to the platform's folder-chooser
    /// (`osascript choose folder` on macOS, `zenity`/`kdialog` on Linux,
    /// `rfd`/Win32 `IFileOpenDialog` on Windows) and returns the chosen
    /// absolute path. `{ path: Option<String> }` — `None` when the user cancels.
    /// Browsers can't get a host path from any web picker (sandbox); the local
    /// backend can, so the web frontend asks it to show the dialog. No file
    /// upload — the agent operates on the real folder (the path is absolute).
    pub const DIALOG_PICK_DIRECTORY: &str = "dialog/pick_directory";

    // Per-message feedback — synchronous CRUD against the shared feedback
    // store (no bus/Directive, like session/list). `feedback/submit` records a
    // 👍/👎/note for one assistant message (params: session_id / turn_id /
    // message_role? / kind / text?); `feedback/list` returns all entries for a
    // session so a reloaded session restores its reaction markers.
    pub const FEEDBACK_SUBMIT: &str = "feedback/submit";
    pub const FEEDBACK_LIST: &str = "feedback/list";

    // Durable host allow/deny list — synchronous CRUD against the shared host
    // allowlist store (no bus/Directive, like session/list). `host/list`
    // returns both admitted and denied hosts so the Settings panel renders in
    // one round-trip; `host/allow` / `host/deny` persist cross-session ("always"
    // — the engine `NetworkProxy` consults the same `~/.oneai/oneai.db`
    // table on every CONNECT); `host/remove` / `host/remove-denied` revoke.
    pub const HOST_LIST: &str = "host/list";
    pub const HOST_ALLOW: &str = "host/allow";
    pub const HOST_DENY: &str = "host/deny";
    pub const HOST_REMOVE: &str = "host/remove";
    pub const HOST_REMOVE_DENIED: &str = "host/remove-denied";

    // Conversation ops.
    pub const CONVERSATION_COMPACT: &str = "conversation/compact";
    pub const PROJECT_INIT: &str = "project/init";

    // Group chat.
    pub const GROUP_START: &str = "group/start";
    pub const GROUP_OPEN: &str = "group/open";
    pub const GROUP_RUN: &str = "group/run";
    pub const GROUP_SET_ORDER: &str = "group/set_order";

    // Scenario library (shared editor unit — pure CRUD, not engine directives;
    // handled directly against the shared ScenarioStore, no bus submit).
    pub const SCENARIO_LIST: &str = "scenario/list";
    pub const SCENARIO_GET: &str = "scenario/get";
    pub const SCENARIO_UPSERT: &str = "scenario/upsert";
    pub const SCENARIO_DELETE: &str = "scenario/delete";
    pub const SCENARIO_VALIDATE: &str = "scenario/validate";

    // App probe — read-only config + skill lifecycle. Handled directly against
    // the shared AppProbe, no bus/Directive. `domainpack/switch` and
    // `provider/add` are deliberately absent (architecture has no hot-swap
    // path — see probe.rs); a pack/provider change restarts the app-server.
    pub const CONFIG_GET: &str = "config/get";
    pub const PROVIDER_LIST: &str = "provider/list";
    pub const PROVIDER_ADD: &str = "provider/add";
    pub const PROVIDER_UPDATE: &str = "provider/update";
    pub const PROVIDER_DELETE: &str = "provider/delete";
    pub const PROVIDER_SET_ACTIVE: &str = "provider/set_active";
    pub const PROVIDER_SET_MODEL: &str = "provider/set_model";
    /// List the models an endpoint serves (kind/api_key/base_url query, or a
    /// `name` that resolves a configured entry — the add-provider form's and
    /// the composer switcher's model dropdown data source, issue #37/#41).
    pub const PROVIDER_MODELS: &str = "provider/models";
    /// Auto-detect the protocol family + normalized base URL from a base_url
    /// (no API key required) — issue #41.
    pub const PROVIDER_DETECT: &str = "provider/detect";
    pub const DOMAINPACK_LIST: &str = "domainpack/list";
    pub const THINKING_GET: &str = "thinking/get";
    pub const THINKING_SET: &str = "thinking/set";
    pub const SKILL_LIST: &str = "skill/list";
    pub const SKILL_PIN: &str = "skill/pin";
    pub const SKILL_UNPIN: &str = "skill/unpin";
    pub const SKILL_ARCHIVE: &str = "skill/archive";
    pub const SKILL_RESTORE: &str = "skill/restore";
    pub const CONFIG_READ: &str = "config/read";

    // Background sub-agent task control (Phase 2A gap-1). Handled directly
    // against the shared AppProbe, no bus/Directive — the probe reaches the
    // app-level `BackgroundTaskRegistry` so a cancel lands even after the
    // delegating turn ended.
    pub const BACKGROUND_LIST: &str = "background/list";
    pub const BACKGROUND_CANCEL: &str = "background/cancel";
    pub const BACKGROUND_CANCEL_ALL: &str = "background/cancel_all";

    // Engine.
    pub const SHUTDOWN: &str = "shutdown";
}

// ─── Standard JSON-RPC 2.0 error codes ──────────────────────────────────────

/// JSON-RPC 2.0 standard error codes (per spec §5.1).
pub mod error_code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            error_code::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::new(error_code::INVALID_PARAMS, msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(error_code::INTERNAL_ERROR, msg)
    }
}

// ─── Envelope ────────────────────────────────────────────────────────────────

/// A JSON-RPC 2.0 request (frontend → app-server). Carries a non-null `id` —
/// the app-server replies with a [`Response`] carrying the same `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default = "default_null")]
    pub params: Value,
}

impl Request {
    pub fn new(id: Value, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC.to_string(),
            id,
            method: method.to_string(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 notification (no `id`, no response expected). The app-server
/// emits these outbound — one per engine yield, under method `event`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default = "default_null")]
    pub params: Value,
}

impl Notification {
    pub fn event(yield_value: &oneai_bus::EngineYield) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: JSONRPC.to_string(),
            method: method::EVENT.to_string(),
            params: serde_json::to_value(yield_value)?,
        })
    }
}

/// A JSON-RPC 2.0 response (app-server → frontend). Carries the request's `id`
/// and exactly one of `result` / `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

fn default_null() -> Value {
    Value::Null
}

/// Decode one inbound JSON-RPC message. Returns `Ok(Ok(request))` for a
/// request (has a non-null `id`), `Ok(Err(notification))` for a notification,
/// or `Err` for a non-decodable / malformed message.
///
/// A message with `id: null` is treated as a notification per JSON-RPC 2.0
/// (spec: a notification is a request without an `id` member, but in practice
/// `id: null` is also notification-shaped — the app-server never sends a
/// response to either).
pub fn decode_inbound(json: &str) -> Result<Result<Request, Notification>, RpcError> {
    let value: Value = serde_json::from_str(json)
        .map_err(|e| RpcError::new(error_code::PARSE_ERROR, format!("parse error: {e}")))?;
    // Validate jsonrpc version (spec §4: must be exactly "2.0" or it's an
    // invalid request). Be lenient: missing is tolerated, wrong is rejected.
    if let Some(v) = value.get("jsonrpc").and_then(|v| v.as_str()) {
        if v != JSONRPC {
            return Err(RpcError::new(
                error_code::INVALID_REQUEST,
                format!("unsupported jsonrpc version: {v}"),
            ));
        }
    }
    let has_method = value.get("method").is_some_and(|m| !m.is_null());
    if !has_method {
        return Err(RpcError::new(error_code::INVALID_REQUEST, "missing method"));
    }
    match value.get("id") {
        Some(Value::Null) | None => serde_json::from_value::<Notification>(value)
            .map(Err)
            .map_err(|e| RpcError::new(error_code::INVALID_REQUEST, e.to_string())),
        Some(_) => serde_json::from_value::<Request>(value)
            .map(Ok)
            .map_err(|e| RpcError::new(error_code::INVALID_REQUEST, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trips() {
        let r = Request::new(json!(42), "turn/run", json!({"content": []}));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""jsonrpc":"2.0""#));
        assert!(s.contains(r#""method":"turn/run""#));
        assert!(s.contains(r#""id":42"#));
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "turn/run");
    }

    #[test]
    fn response_ok_and_err() {
        let ok = Response::ok(json!("req-1"), json!({"turn_id": "t1"}));
        let s = serde_json::to_string(&ok).unwrap();
        assert!(s.contains(r#""result":{"turn_id":"t1"}"#));
        assert!(!s.contains(r#""error""#));
        assert!(s.contains(r#""id":"req-1""#));

        let err = Response::err(json!(7), RpcError::method_not_found("nope"));
        let s = serde_json::to_string(&err).unwrap();
        assert!(s.contains("-32601"));
        assert!(s.contains(r#""id":7"#));
    }

    #[test]
    fn decode_request_with_numeric_id() {
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"turn/run","params":{"content":[]}}"#;
        match decode_inbound(msg) {
            Ok(Ok(req)) => {
                assert_eq!(req.method, "turn/run");
                assert_eq!(req.id, json!(1));
            }
            _ => panic!("expected request"),
        }
    }

    #[test]
    fn decode_notification_with_null_id() {
        // An outbound event notification re-decoded inbound-shaped: id null.
        let msg = r#"{"jsonrpc":"2.0","method":"event","params":{"kind":"turn_start"}}"#;
        match decode_inbound(msg) {
            Ok(Err(n)) => {
                assert_eq!(n.method, "event");
            }
            _ => panic!("expected notification"),
        }
    }

    #[test]
    fn decode_bad_json_is_parse_error() {
        let err = decode_inbound("{ not json").unwrap_err();
        assert_eq!(err.code, error_code::PARSE_ERROR);
    }

    #[test]
    fn decode_missing_method_is_invalid_request() {
        let err = decode_inbound(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err();
        assert_eq!(err.code, error_code::INVALID_REQUEST);
    }

    #[test]
    fn event_notification_carries_yield_kind() {
        let y = oneai_bus::EngineYield::TurnStart {
            turn_id: "t1".into(),
            task: "hi".into(),
        };
        let n = Notification::event(&y).unwrap();
        assert_eq!(n.method, "event");
        assert_eq!(n.params["kind"], "turn_start");
        assert_eq!(n.params["turn_id"], "t1");
    }
}
