//! Newline-delimited JSON wire protocol.
//!
//! Each message is a single `serde_json::Value` serialized to one line and
//! terminated by `\n`. This framing works over both Unix domain sockets and
//! Windows named pipes with no extra dependencies (the workspace's `tokio-util`
//! only exposes the `rt` feature — no `codec`).
//!
//! ## Message shapes
//!
//! - **Request** (client → server):
//!   `{ "id": <u64>, "method": "<snake_case>", "params": <json> }`
//! - **Single response** (server → client):
//!   `{ "id": <u64>, "ok": true, "result": <json> }`
//!   `{ "id": <u64>, "ok": false, "error": "<msg>" }`
//! - **Streaming response** (`rpc_stream` only): zero or more event lines
//!   `{ "id": <u64>, "event": <json> }`
//!   followed by one terminal line:
//!   `{ "id": <u64>, "done": true, "result": <TurnSummary> }`
//!   `{ "id": <u64>, "done": true, "error": "<msg>" }`

use serde::{Deserialize, Serialize};

use crate::error::{Result, SupervisorError};

/// All RPC methods exposed by the supervisor daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RpcMethod {
    /// Spawn a new supervised instance.
    Spawn,
    /// List all instances.
    List,
    /// Stop and unregister an instance.
    Stop,
    /// Query one instance's status.
    Status,
    /// Run one agent turn, returning the final result (request-response).
    Rpc,
    /// Run one agent turn, streaming live events (request-stream).
    RpcStream,
}

/// A client request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: RpcMethod,
    pub params: serde_json::Value,
}

/// A single-line response (non-streaming methods).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    /// Build a successful single response.
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error single response.
    pub fn err(id: u64, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(message.into()),
        }
    }
}

/// A streaming response line — either an interim event or the terminal done.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamLine {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl StreamLine {
    /// An interim event line.
    pub fn event(id: u64, event: serde_json::Value) -> Self {
        Self {
            id,
            event: Some(event),
            done: None,
            result: None,
            error: None,
        }
    }

    /// The terminal success line for a stream.
    pub fn done_ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            event: None,
            done: Some(true),
            result: Some(result),
            error: None,
        }
    }

    /// The terminal error line for a stream.
    pub fn done_err(id: u64, message: impl Into<String>) -> Self {
        Self {
            id,
            event: None,
            done: Some(true),
            result: None,
            error: Some(message.into()),
        }
    }
}

/// Serialize a message to a single `\n`-terminated line.
///
/// Rejects embedded newlines so the framing stays unambiguous.
pub fn encode<T: Serialize>(value: &T) -> Result<String> {
    let mut s = serde_json::to_string(value)?;
    if s.contains('\n') {
        return Err(SupervisorError::Protocol(
            "encoded message contains a newline".to_string(),
        ));
    }
    s.push('\n');
    Ok(s)
}

/// Decode a single line (without the trailing newline) into a JSON value.
pub fn decode(line: &str) -> Result<serde_json::Value> {
    serde_json::from_str(line).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_methods() {
        for (method, name) in [
            (RpcMethod::Spawn, "spawn"),
            (RpcMethod::List, "list"),
            (RpcMethod::Stop, "stop"),
            (RpcMethod::Status, "status"),
            (RpcMethod::Rpc, "rpc"),
            (RpcMethod::RpcStream, "rpc_stream"),
        ] {
            let req = Request {
                id: 1,
                method,
                params: serde_json::json!({"k": "v"}),
            };
            let line = encode(&req).unwrap();
            assert!(line.ends_with('\n'));
            assert!(!line[..line.len() - 1].contains('\n'));
            let val = decode(line.trim()).unwrap();
            assert_eq!(val["method"], name);
            assert_eq!(val["id"], 1);
            assert_eq!(val["params"]["k"], "v");
        }
    }

    #[test]
    fn response_ok_err() {
        let ok = Response::ok(7, serde_json::json!({"id": "x"}));
        let s = serde_json::to_string(&ok).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(back.ok);
        assert_eq!(back.id, 7);
        assert!(back.error.is_none());

        let err = Response::err(8, "boom");
        let back: Response = serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
        assert!(!back.ok);
        assert_eq!(back.error.as_deref(), Some("boom"));
    }

    #[test]
    fn stream_line_shapes() {
        let ev = StreamLine::event(1, serde_json::json!({"type": "stream_chunk"}));
        let v = decode(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["event"]["type"], "stream_chunk");
        assert!(v.get("done").is_none());

        let done = StreamLine::done_ok(1, serde_json::json!({"final_answer": "ok"}));
        let v = decode(&serde_json::to_string(&done).unwrap()).unwrap();
        assert_eq!(v["done"], true);
        assert_eq!(v["result"]["final_answer"], "ok");
    }

    #[test]
    fn rejects_newline_payload() {
        // Manually craft a value whose string field contains a newline.
        let v = serde_json::json!({"bad": "a\nb"});
        // `encode` serializes the *value* — serde escapes \n as \\n, so this is
        // actually safe. Confirm:
        let s = encode(&v).unwrap();
        assert_eq!(s.matches('\n').count(), 1); // only the terminator
    }

    #[test]
    fn decode_malformed() {
        assert!(decode("{ not json").is_err());
    }
}
